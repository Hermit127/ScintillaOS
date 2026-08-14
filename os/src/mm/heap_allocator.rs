// 这里负责操作系统的内存管理：初始化内核堆，并处理堆分配失败。
use crate::config::{KERNEL_HEAP_SIZE, MAX_CPUS, PAGE_SIZE, PLATFORM_CPUS};
use buddy_system_allocator::Heap;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;
#[cfg(feature = "lock-stats")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicUsize, Ordering};
use polyhal::utils::{MutexNoIrq, MutexNoIrqGuard};

const SMALL_ALLOC_MIN_SHIFT: usize = 3;
const SMALL_ALLOC_MAX_SHIFT: usize = 12;
const SMALL_ALLOC_CLASS_COUNT: usize = SMALL_ALLOC_MAX_SHIFT - SMALL_ALLOC_MIN_SHIFT + 1;
const SMALL_ALLOC_CACHE_LIMIT: u16 = 64;
const MAX_HEAP_SHARD_RANGES: usize = MAX_CPUS * 8;

struct SmallObjectCache {
    heads: [usize; SMALL_ALLOC_CLASS_COUNT],
    counts: [u16; SMALL_ALLOC_CLASS_COUNT],
}

impl SmallObjectCache {
    const fn empty() -> Self {
        Self {
            heads: [0; SMALL_ALLOC_CLASS_COUNT],
            counts: [0; SMALL_ALLOC_CLASS_COUNT],
        }
    }

    unsafe fn pop(&mut self, class: usize) -> *mut u8 {
        let head = self.heads[class];
        if head == 0 {
            return core::ptr::null_mut();
        }
        self.heads[class] = *(head as *const usize);
        self.counts[class] -= 1;
        head as *mut u8
    }

    unsafe fn push(&mut self, class: usize, ptr: *mut u8) -> bool {
        if self.counts[class] >= SMALL_ALLOC_CACHE_LIMIT {
            return false;
        }
        *(ptr as *mut usize) = self.heads[class];
        self.heads[class] = ptr as usize;
        self.counts[class] += 1;
        true
    }
}

static SMALL_OBJECT_CACHES: [MutexNoIrq<SmallObjectCache>; MAX_CPUS] = [const {
    MutexNoIrq::new(SmallObjectCache::empty())
}; MAX_CPUS];

struct HeapShardRange {
    start: AtomicUsize,
    end: AtomicUsize,
    shard: AtomicUsize,
}

impl HeapShardRange {
    const fn empty() -> Self {
        Self {
            start: AtomicUsize::new(0),
            end: AtomicUsize::new(0),
            shard: AtomicUsize::new(0),
        }
    }
}

static KERNEL_HEAP_SHARDS: [MutexNoIrq<Heap<32>>; PLATFORM_CPUS] = [const {
    MutexNoIrq::new(Heap::empty())
}; PLATFORM_CPUS];
static HEAP_SHARD_RANGES: [HeapShardRange; MAX_HEAP_SHARD_RANGES] =
    [const { HeapShardRange::empty() }; MAX_HEAP_SHARD_RANGES];
static HEAP_SHARD_RANGE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn small_alloc_class(layout: Layout) -> Option<usize> {
    let required = layout.size().max(layout.align()).max(1usize << SMALL_ALLOC_MIN_SHIFT);
    if required > 1usize << SMALL_ALLOC_MAX_SHIFT {
        return None;
    }
    Some(required.next_power_of_two().trailing_zeros() as usize - SMALL_ALLOC_MIN_SHIFT)
}

#[inline]
fn small_alloc_layout(class: usize) -> Layout {
    let size = 1usize << (class + SMALL_ALLOC_MIN_SHIFT);
    unsafe { Layout::from_size_align_unchecked(size, size) }
}

#[inline]
fn small_alloc_cpu() -> Option<usize> {
    crate::task::try_current_hart_id().filter(|cpu| *cpu < MAX_CPUS)
}

#[inline]
fn lock_heap(heap: &MutexNoIrq<Heap<32>>) -> MutexNoIrqGuard<'_, Heap<32>> {
    #[cfg(feature = "lock-stats")]
    {
        HEAP_LOCK_ACQUISITIONS.fetch_add(1, Ordering::Relaxed);
        let contended = heap.is_locked();
        let wait_started = contended.then(|| polyhal::time::Time::now().to_nsec() as u64);
        let guard = heap.lock();
        if let Some(wait_started) = wait_started {
            let waited = (polyhal::time::Time::now().to_nsec() as u64)
                .saturating_sub(wait_started);
            HEAP_LOCK_CONTENDED.fetch_add(1, Ordering::Relaxed);
            HEAP_LOCK_WAIT_NS.fetch_add(waited, Ordering::Relaxed);
            HEAP_LOCK_WAIT_MAX_NS.fetch_max(waited, Ordering::Relaxed);
        }
        guard
    }
    #[cfg(not(feature = "lock-stats"))]
    {
        heap.lock()
    }
}

fn register_heap_shard_range(start: usize, end: usize, shard: usize) {
    let index = HEAP_SHARD_RANGE_COUNT.load(Ordering::Relaxed);
    assert!(
        index < MAX_HEAP_SHARD_RANGES,
        "too many dynamic kernel heap ranges"
    );
    let range = &HEAP_SHARD_RANGES[index];
    range.start.store(start, Ordering::Relaxed);
    range.end.store(end, Ordering::Relaxed);
    range.shard.store(shard, Ordering::Relaxed);
    HEAP_SHARD_RANGE_COUNT.store(index + 1, Ordering::Release);
}

fn heap_shard_for_ptr(ptr: usize) -> Option<usize> {
    let range_count = HEAP_SHARD_RANGE_COUNT.load(Ordering::Acquire);
    for range in &HEAP_SHARD_RANGES[..range_count] {
        let start = range.start.load(Ordering::Relaxed);
        let end = range.end.load(Ordering::Relaxed);
        if ptr >= start && ptr < end {
            return Some(range.shard.load(Ordering::Relaxed));
        }
    }
    None
}

fn alloc_from_heap_shards(cpu: usize, layout: Layout) -> Option<NonNull<u8>> {
    if HEAP_SHARD_RANGE_COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    let preferred = cpu % PLATFORM_CPUS;
    for offset in 0..PLATFORM_CPUS {
        let shard = (preferred + offset) % PLATFORM_CPUS;
        if let Ok(allocation) = lock_heap(&KERNEL_HEAP_SHARDS[shard]).alloc(layout) {
            #[cfg(feature = "lock-stats")]
            HEAP_SHARD_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            return Some(allocation);
        }
    }
    None
}

struct IrqSafeHeap {
    inner: MutexNoIrq<Heap<32>>,
}

impl IrqSafeHeap {
    const fn empty() -> Self {
        Self {
            inner: MutexNoIrq::new(Heap::empty()),
        }
    }

    #[inline]
    fn lock(&self) -> MutexNoIrqGuard<'_, Heap<32>> {
        lock_heap(&self.inner)
    }

    unsafe fn init(&self, start: usize, size: usize) {
        self.lock().init(start, size);
    }

    fn stats(&self) -> (usize, usize, usize) {
        let heap = self.lock();
        let mut stats = (
            heap.stats_alloc_user(),
            heap.stats_alloc_actual(),
            heap.stats_total_bytes(),
        );
        drop(heap);
        for shard in &KERNEL_HEAP_SHARDS {
            let heap = lock_heap(shard);
            stats.0 += heap.stats_alloc_user();
            stats.1 += heap.stats_alloc_actual();
            stats.2 += heap.stats_total_bytes();
        }
        stats
    }
}

unsafe impl GlobalAlloc for IrqSafeHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        #[cfg(feature = "lock-stats")]
        HEAP_ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        let class = small_alloc_class(layout);
        let cpu = small_alloc_cpu();
        if let (Some(class), Some(cpu)) = (class, cpu) {
            let ptr = SMALL_OBJECT_CACHES[cpu].lock().pop(class);
            if !ptr.is_null() {
                #[cfg(feature = "lock-stats")]
                HEAP_SMALL_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
                return ptr;
            }
            #[cfg(feature = "lock-stats")]
            HEAP_SMALL_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
        }
        let alloc_layout = class.map_or(layout, small_alloc_layout);
        if let Some(allocation) = cpu.and_then(|cpu| alloc_from_heap_shards(cpu, alloc_layout)) {
            return allocation.as_ptr();
        }
        #[cfg(feature = "lock-stats")]
        HEAP_BOOTSTRAP_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        self.lock()
            .alloc(alloc_layout)
            .ok()
            .map_or(core::ptr::null_mut(), |allocation| allocation.as_ptr())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        #[cfg(feature = "lock-stats")]
        HEAP_DEALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        let class = small_alloc_class(layout);
        if let (Some(class), Some(cpu)) = (class, small_alloc_cpu()) {
            if SMALL_OBJECT_CACHES[cpu].lock().push(class, ptr) {
                #[cfg(feature = "lock-stats")]
                HEAP_SMALL_CACHE_RETURNS.fetch_add(1, Ordering::Relaxed);
                return;
            }
            #[cfg(feature = "lock-stats")]
            HEAP_SMALL_CACHE_SPILLS.fetch_add(1, Ordering::Relaxed);
        }
        let alloc_layout = class.map_or(layout, small_alloc_layout);
        if let Some(shard) = heap_shard_for_ptr(ptr as usize) {
            lock_heap(&KERNEL_HEAP_SHARDS[shard])
                .dealloc(NonNull::new_unchecked(ptr), alloc_layout);
        } else {
            self.lock()
                .dealloc(NonNull::new_unchecked(ptr), alloc_layout);
        }
    }
}

#[global_allocator]
static HEAP_ALLOCATOR: IrqSafeHeap = IrqSafeHeap::empty();
static HEAP_TOTAL_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static HEAP_ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "lock-stats")]
static HEAP_DEALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "lock-stats")]
static HEAP_LOCK_ACQUISITIONS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "lock-stats")]
static HEAP_LOCK_CONTENDED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "lock-stats")]
static HEAP_LOCK_WAIT_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "lock-stats")]
static HEAP_LOCK_WAIT_MAX_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "lock-stats")]
static HEAP_SMALL_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "lock-stats")]
static HEAP_SMALL_CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "lock-stats")]
static HEAP_SMALL_CACHE_RETURNS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "lock-stats")]
static HEAP_SMALL_CACHE_SPILLS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "lock-stats")]
static HEAP_SHARD_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "lock-stats")]
static HEAP_BOOTSTRAP_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

#[alloc_error_handler]
// 内核堆分配失败时直接 panic，带上失败的 layout。
// 执行时不尝试恢复，直接把失败的 Layout 打进 panic 信息，避免内核继续在无内存状态下运行。
pub fn handle_alloc_error(layout: core::alloc::Layout) -> ! {
    let (user, actual, total) = HEAP_ALLOCATOR.stats();
    println!(
        "[heap-debug] user={} actual={} total={}",
        user, actual, total
    );
    crate::task::dump_task_stats_for_oom("heap_alloc_error");
    #[cfg(feature = "buildstorm-diagnostics")]
    crate::syscall::dump_active_syscall_for_oom();
    panic!("Heap allocation error, layout = {:#x?}", layout);
}

static mut HEAP_SPACE: [u8; KERNEL_HEAP_SIZE] = [0; KERNEL_HEAP_SIZE];

// 初始化内核全局堆。
// 执行时把静态 HEAP_SPACE 的地址和大小交给 LockedHeap，之后全局 allocator 才能服务 alloc。
pub fn init_heap() {
    unsafe {
        HEAP_ALLOCATOR.init(HEAP_SPACE.as_ptr() as usize, KERNEL_HEAP_SIZE);
    }
    HEAP_TOTAL_BYTES.store(KERNEL_HEAP_SIZE, Ordering::Release);
}

/// Add a boot-reserved physical-memory range to the kernel allocator.
///
/// The caller must remove this range from the frame allocator.  PolyHAL's
/// direct map makes the physical range permanently addressable by the kernel.
pub fn add_heap_region(phys_start: usize, size: usize) {
    if size == 0 {
        return;
    }
    let virt_start = polyhal::PhysAddr::new(phys_start).get_mut_ptr::<u8>() as usize;
    let pages = size / PAGE_SIZE;
    for shard in 0..PLATFORM_CPUS {
        let start = virt_start + pages * shard / PLATFORM_CPUS * PAGE_SIZE;
        let end = virt_start + pages * (shard + 1) / PLATFORM_CPUS * PAGE_SIZE;
        if start == end {
            continue;
        }
        unsafe {
            lock_heap(&KERNEL_HEAP_SHARDS[shard]).add_to_heap(start, end);
        }
        register_heap_shard_range(start, end, shard);
    }
    HEAP_TOTAL_BYTES.fetch_add(size, Ordering::AcqRel);
}

pub fn heap_total_bytes() -> usize {
    HEAP_TOTAL_BYTES.load(Ordering::Acquire)
}

#[cfg(feature = "lock-stats")]
pub fn dump_heap_allocator_stats() {
    println!(
        "[heap-lock-stats] allocs={} deallocs={} acquisitions={} contended={} wait_total_us={} wait_max_us={} cache_hits={} cache_misses={} cache_returns={} cache_spills={} shard_allocs={} bootstrap_allocs={}",
        HEAP_ALLOC_CALLS.load(Ordering::Relaxed),
        HEAP_DEALLOC_CALLS.load(Ordering::Relaxed),
        HEAP_LOCK_ACQUISITIONS.load(Ordering::Relaxed),
        HEAP_LOCK_CONTENDED.load(Ordering::Relaxed),
        HEAP_LOCK_WAIT_NS.load(Ordering::Relaxed) / 1_000,
        HEAP_LOCK_WAIT_MAX_NS.load(Ordering::Relaxed) / 1_000,
        HEAP_SMALL_CACHE_HITS.load(Ordering::Relaxed),
        HEAP_SMALL_CACHE_MISSES.load(Ordering::Relaxed),
        HEAP_SMALL_CACHE_RETURNS.load(Ordering::Relaxed),
        HEAP_SMALL_CACHE_SPILLS.load(Ordering::Relaxed),
        HEAP_SHARD_ALLOCATIONS.load(Ordering::Relaxed),
        HEAP_BOOTSTRAP_ALLOCATIONS.load(Ordering::Relaxed),
    );
}
