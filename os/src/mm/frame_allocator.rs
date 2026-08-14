// 这里负责操作系统的内存管理：实现物理页帧分配、释放和共享页引用计数。
use crate::config::{MAX_CPUS, PAGE_SIZE, PLATFORM_CPUS};
use crate::sync::FrameAllocatorLock;
use alloc::boxed::Box;
use alloc::vec::Vec;
use buddy_system_allocator::FrameAllocator;
use core::{
    fmt::{self, Debug, Formatter},
    ptr,
    sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering},
};
use polyhal::{pa, PhysAddr};

const MAX_FRAME_REF_REGIONS: usize = 8;
const FRAME_ALLOCATOR_SHARDS: usize = PLATFORM_CPUS;
const MAX_FRAME_ALLOCATOR_RANGES: usize = MAX_FRAME_REF_REGIONS * MAX_CPUS;

struct FrameRefRegion {
    start_frame: AtomicUsize,
    end_frame: AtomicUsize,
    refs: AtomicPtr<AtomicU32>,
}

struct FrameAllocatorRange {
    start_frame: AtomicUsize,
    end_frame: AtomicUsize,
    shard: AtomicUsize,
}

impl FrameAllocatorRange {
    const fn new() -> Self {
        Self {
            start_frame: AtomicUsize::new(0),
            end_frame: AtomicUsize::new(0),
            shard: AtomicUsize::new(0),
        }
    }
}

impl FrameRefRegion {
    const fn new() -> Self {
        Self {
            start_frame: AtomicUsize::new(0),
            end_frame: AtomicUsize::new(0),
            refs: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

// Reference metadata is allocated once for each boot-discovered RAM range.
// This makes the common clone/drop path lock-free while avoiding a sparse
// physical-address-sized table on platforms with discontiguous memory.
static FRAME_REF_REGIONS: [FrameRefRegion; MAX_FRAME_REF_REGIONS] =
    [const { FrameRefRegion::new() }; MAX_FRAME_REF_REGIONS];
static FRAME_REF_REGION_COUNT: AtomicUsize = AtomicUsize::new(0);
static FRAME_ALLOCATOR_RANGES: [FrameAllocatorRange; MAX_FRAME_ALLOCATOR_RANGES] =
    [const { FrameAllocatorRange::new() }; MAX_FRAME_ALLOCATOR_RANGES];
static FRAME_ALLOCATOR_RANGE_COUNT: AtomicUsize = AtomicUsize::new(0);

// 把物理地址折算成按页编号的引用计数 key。
// 执行时用物理地址除以 PAGE_SIZE，得到 FRAME_REFS 里记录共享计数的页号。
fn ref_key(paddr: PhysAddr) -> usize {
    paddr.raw() / PAGE_SIZE
}

fn register_frame_ref_region(start_frame: usize, end_frame: usize) {
    let frame_count = end_frame.saturating_sub(start_frame);
    let mut refs = Vec::new();
    refs.try_reserve_exact(frame_count)
        .expect("failed to reserve physical-frame reference metadata");
    refs.resize_with(frame_count, || AtomicU32::new(0));
    let refs = Box::leak(refs.into_boxed_slice());

    let index = FRAME_REF_REGION_COUNT.load(Ordering::Relaxed);
    assert!(
        index < MAX_FRAME_REF_REGIONS,
        "too many physical-memory regions for frame reference metadata"
    );
    let region = &FRAME_REF_REGIONS[index];
    region.start_frame.store(start_frame, Ordering::Relaxed);
    region.end_frame.store(end_frame, Ordering::Relaxed);
    region.refs.store(refs.as_mut_ptr(), Ordering::Release);
    FRAME_REF_REGION_COUNT.store(index + 1, Ordering::Release);
}

fn frame_ref_count(paddr: PhysAddr) -> &'static AtomicU32 {
    let key = ref_key(paddr);
    let region_count = FRAME_REF_REGION_COUNT.load(Ordering::Acquire);
    for region in &FRAME_REF_REGIONS[..region_count] {
        let start = region.start_frame.load(Ordering::Relaxed);
        let end = region.end_frame.load(Ordering::Relaxed);
        if key < start || key >= end {
            continue;
        }
        let refs = region.refs.load(Ordering::Acquire);
        assert!(!refs.is_null(), "frame reference region was not published");
        // The backing slice is leaked for the kernel lifetime, and the bounds
        // check above guarantees this frame's index is inside that slice.
        return unsafe { &*refs.add(key - start) };
    }
    panic!("physical frame {:#x} has no reference metadata", paddr.raw());
}

// 增加共享物理页的引用计数。
fn ref_inc(paddr: PhysAddr) -> usize {
    let refs = frame_ref_count(paddr);
    let previous = refs.fetch_add(1, Ordering::AcqRel);
    assert!(previous != 0, "cloning an unowned physical frame");
    assert!(previous != u32::MAX, "physical-frame reference overflow");
    previous as usize + 1
}

fn ref_dec(paddr: PhysAddr) -> usize {
    let previous = frame_ref_count(paddr).fetch_sub(1, Ordering::AcqRel);
    assert!(previous != 0, "dropping an unowned physical frame");
    let remaining = previous - 1;
    #[cfg(feature = "smp-diagnostics")]
    trace_drop(paddr, remaining as usize);
    remaining as usize
}

pub struct FrameTracker {
    pub paddr: PhysAddr,
}

impl FrameTracker {
    // 接管一页物理内存，并先清零。
    // 执行时把整页物理内存清零，再把物理地址保存进 FrameTracker。
    pub fn new(paddr: PhysAddr) -> Self {
        frame_ref_count(paddr)
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .expect("physical frame allocated while it is still referenced");
        paddr.clear_len(PAGE_SIZE);
        Self { paddr }
    }
    // 为共享映射复制一个 tracker，不实际分配新页。
    // 执行时只增加这页的共享引用计数，并返回指向同一物理页的新 tracker。
    pub fn shared_clone(&self) -> Self {
        let refs = ref_inc(self.paddr);
        #[cfg(feature = "smp-diagnostics")]
        trace_shared_clone(self.paddr, refs);
        Self { paddr: self.paddr }
    }
    // 判断这页是否还有别的 tracker 引用。
    // 执行时查看这页的原子计数，计数大于 1 就说明还被共享。
    pub fn is_shared(&self) -> bool {
        frame_ref_count(self.paddr).load(Ordering::Acquire) > 1
    }
}

impl Drop for FrameTracker {
    // tracker 释放时扣引用计数，最后一个引用才真正归还物理页。
    // 执行时原子扣减共享计数，最后一个所有者把页归还给帧分配器。
    fn drop(&mut self) {
        if ref_dec(self.paddr) != 0 {
            return;
        }
        frame_dealloc(self.paddr);
    }
}

impl Debug for FrameTracker {
    // 调试输出时只打印物理地址，避免展开太多内部状态。
    // 执行时把结构体里的关键字段写进 formatter，不修改对象状态，最后返回格式化结果。
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("FrameTracker:PA={:?}", self.paddr))
    }
}

/// Physical pages are partitioned across independent buddy allocators. The
/// per-CPU magazines remain the fast path, while a refill normally touches only
/// the local CPU's shard instead of serializing every hart on one BTreeSet.
static FRAME_ALLOCATORS: [FrameAllocatorLock<FrameAllocator>; FRAME_ALLOCATOR_SHARDS] = [const {
    FrameAllocatorLock::new_named(FrameAllocator::new(), "Vm.frame_allocator")
}; FRAME_ALLOCATOR_SHARDS];
const FRAME_MAGAZINE_CAPACITY: usize = 64;
const FRAME_MAGAZINE_BATCH: usize = 32;

struct FrameMagazine {
    frames: [usize; FRAME_MAGAZINE_CAPACITY],
    len: usize,
}

impl FrameMagazine {
    const fn new() -> Self {
        Self {
            frames: [0; FRAME_MAGAZINE_CAPACITY],
            len: 0,
        }
    }

    fn pop(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(self.frames[self.len])
    }

    fn push(&mut self, frame: usize) -> bool {
        if self.len == FRAME_MAGAZINE_CAPACITY {
            return false;
        }
        self.frames[self.len] = frame;
        self.len += 1;
        true
    }
}

static FRAME_MAGAZINES: [FrameAllocatorLock<FrameMagazine>; MAX_CPUS] = [const {
    FrameAllocatorLock::new_named(FrameMagazine::new(), "Vm.frame_magazine")
}; MAX_CPUS];
static TOTAL_FRAMES: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_FRAMES: AtomicUsize = AtomicUsize::new(0);
static FRAME_ALLOC_FAILURES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp-diagnostics")]
static WATCHED_FRAME: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp-diagnostics")]
static WATCHED_FRAME_OFFSET: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp-diagnostics")]
static WATCHED_FRAME_HEAD: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp-diagnostics")]
static WATCHED_FRAME_WORD: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp-diagnostics")]
static WATCHED_FRAME_REPORTED: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "smp-diagnostics")]
pub fn watch_frame(paddr: PhysAddr, offset: usize, head: u32, word: u32) {
    WATCHED_FRAME_OFFSET.store(offset, Ordering::Relaxed);
    WATCHED_FRAME_HEAD.store(head as usize, Ordering::Relaxed);
    WATCHED_FRAME_WORD.store(word as usize, Ordering::Relaxed);
    WATCHED_FRAME_REPORTED.store(0, Ordering::Relaxed);
    WATCHED_FRAME.store(paddr.raw(), Ordering::Release);
    crate::println!(
        "[frame-diag] watch paddr={:#x} offset={:#x} head={:#010x} word={:#010x}",
        paddr.raw(),
        offset,
        head,
        word,
    );
}

/// Return the first observed mutation of the currently watched physical page.
/// This is diagnostic-only and deliberately checks two independent words so a
/// benign instruction-cache effect cannot be confused with memory corruption.
#[cfg(feature = "smp-diagnostics")]
pub fn take_watched_frame_corruption() -> Option<(PhysAddr, usize, u32, u32, u32, u32)> {
    let raw = WATCHED_FRAME.load(Ordering::Acquire);
    if raw == 0 {
        return None;
    }
    let offset = WATCHED_FRAME_OFFSET.load(Ordering::Relaxed);
    let expected_head = WATCHED_FRAME_HEAD.load(Ordering::Relaxed) as u32;
    let expected_word = WATCHED_FRAME_WORD.load(Ordering::Relaxed) as u32;
    let paddr = pa!(raw);
    let actual_head = unsafe { core::ptr::read_unaligned(paddr.get_ptr::<u32>()) };
    let actual_word = unsafe {
        core::ptr::read_unaligned((paddr + offset).get_ptr::<u32>())
    };
    if actual_head == expected_head && actual_word == expected_word {
        return None;
    }
    if WATCHED_FRAME_REPORTED
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    Some((
        paddr,
        offset,
        expected_head,
        actual_head,
        expected_word,
        actual_word,
    ))
}

/// Check the watched page without consuming the first-corruption report.
/// Callers that serialize an external writer can use this to identify the
/// operation which changed the page before the next timer interrupt observes
/// it.
#[cfg(feature = "smp-diagnostics")]
pub fn watched_frame_is_intact() -> bool {
    let raw = WATCHED_FRAME.load(Ordering::Acquire);
    if raw == 0 {
        return true;
    }
    let offset = WATCHED_FRAME_OFFSET.load(Ordering::Relaxed);
    let expected_head = WATCHED_FRAME_HEAD.load(Ordering::Relaxed) as u32;
    let expected_word = WATCHED_FRAME_WORD.load(Ordering::Relaxed) as u32;
    let paddr = pa!(raw);
    let actual_head = unsafe { core::ptr::read_unaligned(paddr.get_ptr::<u32>()) };
    let actual_word = unsafe {
        core::ptr::read_unaligned((paddr + offset).get_ptr::<u32>())
    };
    actual_head == expected_head && actual_word == expected_word
}

#[cfg(feature = "smp-diagnostics")]
pub fn watched_frame_paddr() -> Option<PhysAddr> {
    let raw = WATCHED_FRAME.load(Ordering::Acquire);
    (raw != 0).then(|| pa!(raw))
}

#[cfg(feature = "smp-diagnostics")]
fn trace_allocation(paddr: PhysAddr, kind: &'static str) {
    if WATCHED_FRAME
        .compare_exchange(paddr.raw(), 0, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        crate::println!(
            "[frame-diag] allocate kind={} paddr={:#x}; stop free-frame watch",
            kind,
            paddr.raw(),
        );
    }
}

#[cfg(feature = "smp-diagnostics")]
fn trace_shared_clone(paddr: PhysAddr, refs: usize) {
    if WATCHED_FRAME.load(Ordering::Acquire) == paddr.raw() {
        crate::println!(
            "[frame-diag] shared-clone paddr={:#x} refs={}",
            paddr.raw(),
            refs,
        );
    }
}

#[cfg(feature = "smp-diagnostics")]
fn trace_drop(paddr: PhysAddr, remaining_refs: usize) {
    if WATCHED_FRAME.load(Ordering::Acquire) == paddr.raw() {
        crate::println!(
            "[frame-diag] drop paddr={:#x} remaining_refs={}",
            paddr.raw(),
            remaining_refs,
        );
    }
}

#[cfg(feature = "smp-diagnostics")]
fn trace_deallocation(paddr: PhysAddr) {
    if WATCHED_FRAME.load(Ordering::Acquire) == paddr.raw() {
        crate::println!("[frame-diag] deallocate paddr={:#x}", paddr.raw());
    }
}

// Register a boot-discovered physical-memory range with the frame allocator.
// Pages are cleared when handed out, not eagerly here: clearing all RAM at
// boot makes large-memory QEMU configurations spend most of their startup
// time writing unused pages while providing no additional isolation.
pub fn add_frames_range(mm_start: usize, mm_end: usize) {
    let start = (mm_start + 0xfff) / PAGE_SIZE;
    let end = mm_end / PAGE_SIZE;
    register_frame_ref_region(start, end);
    TOTAL_FRAMES.fetch_add(end - start, Ordering::Relaxed);
    let frame_count = end - start;
    for shard in 0..FRAME_ALLOCATOR_SHARDS {
        let shard_start = start + frame_count * shard / FRAME_ALLOCATOR_SHARDS;
        let shard_end = start + frame_count * (shard + 1) / FRAME_ALLOCATOR_SHARDS;
        if shard_start == shard_end {
            continue;
        }
        register_frame_allocator_range(shard_start, shard_end, shard);
        FRAME_ALLOCATORS[shard]
            .lock()
            .add_frame(shard_start, shard_end);
    }
}

fn current_frame_magazine() -> Option<usize> {
    crate::task::try_current_hart_id().filter(|cpu| *cpu < MAX_CPUS)
}

fn preferred_allocator_shard() -> usize {
    current_frame_magazine().unwrap_or(0) % FRAME_ALLOCATOR_SHARDS
}

fn register_frame_allocator_range(start_frame: usize, end_frame: usize, shard: usize) {
    let index = FRAME_ALLOCATOR_RANGE_COUNT.load(Ordering::Relaxed);
    assert!(
        index < MAX_FRAME_ALLOCATOR_RANGES,
        "too many physical-memory ranges for allocator shards"
    );
    let range = &FRAME_ALLOCATOR_RANGES[index];
    range.start_frame.store(start_frame, Ordering::Relaxed);
    range.end_frame.store(end_frame, Ordering::Relaxed);
    range.shard.store(shard, Ordering::Relaxed);
    FRAME_ALLOCATOR_RANGE_COUNT.store(index + 1, Ordering::Release);
}

fn allocator_shard_for_frame(frame: usize) -> usize {
    let range_count = FRAME_ALLOCATOR_RANGE_COUNT.load(Ordering::Acquire);
    for range in &FRAME_ALLOCATOR_RANGES[..range_count] {
        let start = range.start_frame.load(Ordering::Relaxed);
        let end = range.end_frame.load(Ordering::Relaxed);
        if frame >= start && frame < end {
            return range.shard.load(Ordering::Relaxed);
        }
    }
    panic!("physical frame {frame:#x} has no allocator shard");
}

fn refill_frame_numbers(frames: &mut [usize]) -> usize {
    let preferred = preferred_allocator_shard();
    for offset in 0..FRAME_ALLOCATOR_SHARDS {
        let shard = (preferred + offset) % FRAME_ALLOCATOR_SHARDS;
        let mut allocator = FRAME_ALLOCATORS[shard].lock();
        let mut filled = 0usize;

        while filled < frames.len() {
            let remaining = frames.len() - filled;
            let mut chunk = 1usize << (usize::BITS - 1 - remaining.leading_zeros());
            let mut allocation = None;
            while chunk != 0 {
                if let Some(start) = allocator.alloc(chunk) {
                    allocation = Some((start, chunk));
                    break;
                }
                chunk >>= 1;
            }
            let Some((start, chunk)) = allocation else {
                break;
            };
            for (index, frame) in frames[filled..filled + chunk].iter_mut().enumerate() {
                *frame = start + index;
            }
            filled += chunk;
        }
        if filled != 0 {
            return filled;
        }
    }
    0
}

fn dealloc_contiguous_frame_range(
    allocator: &mut FrameAllocator,
    mut start: usize,
    mut count: usize,
) {
    while count != 0 {
        let by_length = 1usize << (usize::BITS - 1 - count.leading_zeros());
        let by_alignment = if start == 0 {
            by_length
        } else {
            start & start.wrapping_neg()
        };
        let chunk = by_length.min(by_alignment);
        allocator.dealloc(start, chunk);
        start += chunk;
        count -= chunk;
    }
}

fn dealloc_sorted_frame_numbers(frames: &[usize]) {
    let Some(&first) = frames.first() else {
        return;
    };
    let mut run_start = first;
    let mut previous = first;
    let mut run_shard = allocator_shard_for_frame(first);
    for &frame in &frames[1..] {
        assert!(frame > previous, "duplicate physical frame in magazine flush");
        let shard = allocator_shard_for_frame(frame);
        if frame != previous + 1 || shard != run_shard {
            let mut allocator = FRAME_ALLOCATORS[run_shard].lock();
            dealloc_contiguous_frame_range(
                &mut allocator,
                run_start,
                previous - run_start + 1,
            );
            run_start = frame;
            run_shard = shard;
        }
        previous = frame;
    }
    let mut allocator = FRAME_ALLOCATORS[run_shard].lock();
    dealloc_contiguous_frame_range(&mut allocator, run_start, previous - run_start + 1);
}

fn alloc_frame_number() -> Option<usize> {
    let Some(cpu) = current_frame_magazine() else {
        let mut refill = [0usize; 1];
        return (refill_frame_numbers(&mut refill) == 1).then_some(refill[0]);
    };
    if let Some(frame) = FRAME_MAGAZINES[cpu].lock().pop() {
        return Some(frame);
    }

    let mut refill = [0usize; FRAME_MAGAZINE_BATCH];
    let refill_len = refill_frame_numbers(&mut refill);
    if refill_len != 0 {
        let allocated = refill[0];
        let mut magazine = FRAME_MAGAZINES[cpu].lock();
        for &frame in &refill[1..refill_len] {
            assert!(magazine.push(frame));
        }
        return Some(allocated);
    }

    // A nearly exhausted buddy may still have free pages in another CPU's
    // magazine. Steal one before reporting allocation failure.
    for offset in 1..MAX_CPUS {
        let other = (cpu + offset) % MAX_CPUS;
        if let Some(frame) = FRAME_MAGAZINES[other].lock().pop() {
            return Some(frame);
        }
    }
    None
}

fn dealloc_frame_number(frame: usize) {
    let Some(cpu) = current_frame_magazine() else {
        let shard = allocator_shard_for_frame(frame);
        FRAME_ALLOCATORS[shard].lock().dealloc(frame, 1);
        return;
    };

    let mut flush = [0usize; FRAME_MAGAZINE_BATCH];
    {
        let mut magazine = FRAME_MAGAZINES[cpu].lock();
        if magazine.push(frame) {
            return;
        }
        for slot in &mut flush {
            *slot = magazine
                .pop()
                .expect("full frame magazine lost an entry while flushing");
        }
        assert!(magazine.push(frame));
    }
    flush.sort_unstable();
    dealloc_sorted_frame_numbers(&flush);
}

fn flush_frame_magazines() {
    for magazine in &FRAME_MAGAZINES {
        let mut frames = [0usize; FRAME_MAGAZINE_CAPACITY];
        let len = {
            let mut magazine = magazine.lock();
            let mut len = 0usize;
            while let Some(frame) = magazine.pop() {
                frames[len] = frame;
                len += 1;
            }
            len
        };
        if len == 0 {
            continue;
        }
        frames[..len].sort_unstable();
        dealloc_sorted_frame_numbers(&frames[..len]);
    }
}

fn alloc_contiguous_frame_numbers(count: usize) -> Option<usize> {
    if count == 0 {
        return Some(0);
    }
    let preferred = preferred_allocator_shard();
    for offset in 0..FRAME_ALLOCATOR_SHARDS {
        let shard = (preferred + offset) % FRAME_ALLOCATOR_SHARDS;
        let mut allocator = FRAME_ALLOCATORS[shard].lock();
        let Some(start) = allocator.alloc(count) else {
            continue;
        };
        let allocated = count.next_power_of_two();
        if allocated > count {
            dealloc_contiguous_frame_range(&mut allocator, start + count, allocated - count);
        }
        return Some(start);
    }
    None
}

// 分配一页受 RAII 管理的物理页。
// 执行时从 buddy 分配器取 1 页，转换成 PhysAddr 并包成 FrameTracker，成功后增加已分配计数。
pub fn frame_alloc() -> Option<FrameTracker> {
    let paddr = alloc_frame_number().map(|x| pa!(x * PAGE_SIZE));
    let ret = paddr.map(|paddr| {
        #[cfg(feature = "smp-diagnostics")]
        trace_allocation(paddr, "tracked");
        FrameTracker::new(paddr)
    });
    if ret.is_some() {
        ALLOCATED_FRAMES.fetch_add(1, Ordering::Relaxed);
    } else {
        FRAME_ALLOC_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    ret
}

// 连续分配多页物理页，并逐页包装成 tracker。
// 执行时从 buddy 分配器取连续页号，再按页大小展开成多个 FrameTracker。
pub fn frames_alloc(count: usize) -> Option<Vec<FrameTracker>> {
    if count == 0 {
        return Some(Vec::new());
    }
    let mut start = alloc_contiguous_frame_numbers(count);
    if start.is_none() {
        flush_frame_magazines();
        start = alloc_contiguous_frame_numbers(count);
    }
    let Some(start) = start.map(|x| pa!(x * PAGE_SIZE)) else {
        FRAME_ALLOC_FAILURES.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    ALLOCATED_FRAMES.fetch_add(count, Ordering::Relaxed);
    let ret = (0..count)
        .into_iter()
        .map(|idx| (start + idx * PAGE_SIZE))
        .map(|paddr| {
            #[cfg(feature = "smp-diagnostics")]
            trace_allocation(paddr, "tracked-batch");
            FrameTracker::new(paddr)
        })
        .collect();
    Some(ret)
}

// Allocate independent pages through the per-CPU magazine. Unlike
// `frames_alloc`, this does not require a physically contiguous range. Large
// file reads can request thousands of pages, so never hold the global buddy
// lock for the whole batch; magazine refills bound each critical section.
pub fn frames_alloc_batch(count: usize) -> Vec<FrameTracker> {
    let mut paddrs = Vec::new();
    if paddrs.try_reserve_exact(count).is_err() {
        FRAME_ALLOC_FAILURES.fetch_add(1, Ordering::Relaxed);
        return Vec::new();
    }

    while paddrs.len() < count {
        let Some(frame) = alloc_frame_number() else {
            break;
        };
        paddrs.push(pa!(frame * PAGE_SIZE));
    }
    if paddrs.len() < count {
        FRAME_ALLOC_FAILURES.fetch_add(1, Ordering::Relaxed);
    }

    ALLOCATED_FRAMES.fetch_add(paddrs.len(), Ordering::Relaxed);
    paddrs
        .into_iter()
        .map(|paddr| {
            #[cfg(feature = "smp-diagnostics")]
            trace_allocation(paddr, "tracked-batch-independent");
            FrameTracker::new(paddr)
        })
        .collect()
}

// 归还一页物理内存。
// 执行时把物理地址换算成页号交回 buddy 分配器，并减少已分配页计数。
pub fn frame_dealloc(paddr: PhysAddr) {
    #[cfg(feature = "smp-diagnostics")]
    trace_deallocation(paddr);
    dealloc_frame_number(paddr.raw() / PAGE_SIZE);
    ALLOCATED_FRAMES.fetch_sub(1, Ordering::Relaxed);
}

pub(crate) fn frames_dealloc_batch(frames: Vec<FrameTracker>) {
    if frames.is_empty() {
        return;
    }
    let mut released = Vec::new();
    if released.try_reserve_exact(frames.len()).is_err() {
        drop(frames);
        return;
    }
    for frame in frames {
        let frame = core::mem::ManuallyDrop::new(frame);
        if ref_dec(frame.paddr) == 0 {
            #[cfg(feature = "smp-diagnostics")]
            trace_deallocation(frame.paddr);
            released.push(frame.paddr.raw() / PAGE_SIZE);
        }
    }
    if released.is_empty() {
        return;
    }
    released.sort_unstable();
    dealloc_sorted_frame_numbers(&released);
    ALLOCATED_FRAMES.fetch_sub(released.len(), Ordering::Relaxed);
}

// 返回内核当前管理的总页数。
// 执行时直接读取 TOTAL_FRAMES 原子计数。
pub fn total_frames() -> usize {
    TOTAL_FRAMES.load(Ordering::Relaxed)
}

// 返回粗略可用页数，用于 /proc/meminfo 这类统计。
// 执行时用总页数减去已分配页数，并用 saturating_sub 避免统计误差导致下溢。
pub fn available_frames() -> usize {
    total_frames().saturating_sub(ALLOCATED_FRAMES.load(Ordering::Relaxed))
}

pub fn allocated_frames() -> usize {
    ALLOCATED_FRAMES.load(Ordering::Relaxed)
}

pub fn frame_alloc_failures() -> usize {
    FRAME_ALLOC_FAILURES.load(Ordering::Relaxed)
}

// 分配一页不会被 FrameTracker 自动释放的物理页。
// 执行时从 buddy 分配器取一页并清零，只返回 PhysAddr，让调用方自己管理生命周期。
pub fn frame_alloc_persist() -> Option<PhysAddr> {
    let ret = alloc_frame_number().map(|x| pa!(x * PAGE_SIZE));
    if let Some(paddr) = ret {
        #[cfg(feature = "smp-diagnostics")]
        trace_allocation(paddr, "persistent");
        paddr.clear_len(PAGE_SIZE);
        ALLOCATED_FRAMES.fetch_add(1, Ordering::Relaxed);
    }
    if ret.is_none() {
        FRAME_ALLOC_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    ret
}
