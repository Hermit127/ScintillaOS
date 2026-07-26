// 这里负责操作系统的内存管理：实现物理页帧分配、释放和共享页引用计数。
use crate::config::PAGE_SIZE;
use crate::sync::{FrameAllocatorLock, FrameRefLock};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use buddy_system_allocator::FrameAllocator;
use core::{
    fmt::{self, Debug, Formatter},
    mem::size_of,
    sync::atomic::{AtomicUsize, Ordering},
};
use polyhal::{pa, PhysAddr};

type RefMap = BTreeMap<usize, usize>;
/// Shared-frame reference counts.  This lock is acquired before the frame
/// allocator when the final `FrameTracker` reference is dropped.
static FRAME_REFS: FrameRefLock<RefMap> =
    FrameRefLock::new_named(BTreeMap::new(), "Vm.frame_refs");

// 把物理地址折算成按页编号的引用计数 key。
// 执行时用物理地址除以 PAGE_SIZE，得到 FRAME_REFS 里记录共享计数的页号。
fn ref_key(paddr: PhysAddr) -> usize {
    paddr.raw() / PAGE_SIZE
}

// 增加共享物理页的引用计数。
// 执行时锁住 FRAME_REFS，找到页号对应计数，没有记录时从 1 开始，再递增一次。
fn ref_inc(paddr: PhysAddr) {
    let mut refs = FRAME_REFS.lock();
    let entry = refs.entry(ref_key(paddr)).or_insert(1);
    *entry += 1;
}

pub struct FrameTracker {
    pub paddr: PhysAddr,
}

impl FrameTracker {
    // 接管一页物理内存，并先清零。
    // 执行时把整页物理内存清零，再把物理地址保存进 FrameTracker。
    pub fn new(paddr: PhysAddr) -> Self {
        paddr.clear_len(PAGE_SIZE);
        Self { paddr }
    }
    // 为共享映射复制一个 tracker，不实际分配新页。
    // 执行时只增加这页的共享引用计数，并返回指向同一物理页的新 tracker。
    pub fn shared_clone(&self) -> Self {
        ref_inc(self.paddr);
        Self { paddr: self.paddr }
    }
    // 判断这页是否还有别的 tracker 引用。
    // 执行时查看 FRAME_REFS 里这页的计数，计数大于 1 就说明还被共享。
    pub fn is_shared(&self) -> bool {
        let refs = FRAME_REFS.lock();
        refs.get(&ref_key(self.paddr)).map_or(false, |&c| c > 1)
    }
}

impl Drop for FrameTracker {
    // tracker 释放时扣引用计数，最后一个引用才真正归还物理页。
    // 执行时先扣共享引用计数；如果没有共享记录，说明这是最后一个所有者，就归还给帧分配器。
    fn drop(&mut self) {
        let mut refs = FRAME_REFS.lock();
        let key = ref_key(self.paddr);
        if let Some(count) = refs.get_mut(&key) {
            if *count > 1 {
                *count -= 1;
                return;
            }
            refs.remove(&key);
        }
        drop(refs);
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

/// Buddy allocator state for physical pages.  Allocation never nests the
/// reference-count table; final destruction follows the documented
/// `frame_refs -> frame_allocator` order.
pub static FRAME_ALLOCATOR: FrameAllocatorLock<FrameAllocator> =
    FrameAllocatorLock::new_named(FrameAllocator::new(), "Vm.frame_allocator");
static TOTAL_FRAMES: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_FRAMES: AtomicUsize = AtomicUsize::new(0);

// 把启动阶段探测到的一段物理内存加入帧分配器。
// 执行时先把这段物理内存清零，再按页对齐算出页号范围，最后加入 buddy frame allocator。
pub fn add_frames_range(mm_start: usize, mm_end: usize) {
    unsafe {
        core::slice::from_raw_parts_mut(
            pa!(mm_start).get_mut_ptr::<u128>(),
            (mm_end - mm_start) / size_of::<u128>(),
        )
        .fill(0);
    }
    let start = (mm_start + 0xfff) / PAGE_SIZE;
    let end = mm_end / PAGE_SIZE;
    TOTAL_FRAMES.fetch_add(end - start, Ordering::Relaxed);
    FRAME_ALLOCATOR.lock().add_frame(start, end);
}

// 分配一页受 RAII 管理的物理页。
// 执行时从 buddy 分配器取 1 页，转换成 PhysAddr 并包成 FrameTracker，成功后增加已分配计数。
pub fn frame_alloc() -> Option<FrameTracker> {
    let ret = FRAME_ALLOCATOR
        .lock()
        .alloc(1)
        .map(|x| pa!(x * PAGE_SIZE))
        .map(FrameTracker::new);
    if ret.is_some() {
        ALLOCATED_FRAMES.fetch_add(1, Ordering::Relaxed);
    }
    ret
}

// 连续分配多页物理页，并逐页包装成 tracker。
// 执行时从 buddy 分配器取连续页号，再按页大小展开成多个 FrameTracker。
pub fn frames_alloc(count: usize) -> Option<Vec<FrameTracker>> {
    let start = FRAME_ALLOCATOR
        .lock()
        .alloc(count)
        .map(|x| pa!(x * PAGE_SIZE))?;
    ALLOCATED_FRAMES.fetch_add(count, Ordering::Relaxed);
    let ret = (0..count)
        .into_iter()
        .map(|idx| (start + idx * PAGE_SIZE))
        .map(FrameTracker::new)
        .collect();
    Some(ret)
}

// 归还一页物理内存。
// 执行时把物理地址换算成页号交回 buddy 分配器，并减少已分配页计数。
pub fn frame_dealloc(paddr: PhysAddr) {
    FRAME_ALLOCATOR.lock().dealloc(paddr.raw() / PAGE_SIZE, 1);
    ALLOCATED_FRAMES.fetch_sub(1, Ordering::Relaxed);
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

// 分配一页不会被 FrameTracker 自动释放的物理页。
// 执行时从 buddy 分配器取一页并清零，只返回 PhysAddr，让调用方自己管理生命周期。
pub fn frame_alloc_persist() -> Option<PhysAddr> {
    FRAME_ALLOCATOR
        .lock()
        .alloc(1)
        .map(|x| pa!(x * PAGE_SIZE))
        .inspect(|x| {
            x.clear_len(PAGE_SIZE);
            ALLOCATED_FRAMES.fetch_add(1, Ordering::Relaxed);
        })
}
