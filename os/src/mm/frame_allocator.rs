use crate::config::PAGE_SIZE;
use alloc::vec::Vec;
use buddy_system_allocator::FrameAllocator;
use core::{
    fmt::{self, Debug, Formatter},
    mem::size_of,
    sync::atomic::{AtomicUsize, Ordering},
};
use log::trace;
use polyhal::{pa, utils::MutexNoIrq, PhysAddr};

pub struct FrameTracker {
    pub paddr: PhysAddr,
}

impl FrameTracker {
    pub fn new(paddr: PhysAddr) -> Self {
        paddr.clear_len(PAGE_SIZE);
        Self { paddr }
    }
}

impl Drop for FrameTracker {
    fn drop(&mut self) {
        frame_dealloc(self.paddr);
    }
}

impl Debug for FrameTracker {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("FrameTracker:PA={:?}", self.paddr))
    }
}

pub static FRAME_ALLOCATOR: MutexNoIrq<FrameAllocator> = MutexNoIrq::new(FrameAllocator::new());
static TOTAL_FRAMES: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_FRAMES: AtomicUsize = AtomicUsize::new(0);

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

pub fn frame_dealloc(paddr: PhysAddr) {
    FRAME_ALLOCATOR.lock().dealloc(paddr.raw() / PAGE_SIZE, 1);
    ALLOCATED_FRAMES.fetch_sub(1, Ordering::Relaxed);
}

pub fn total_frames() -> usize {
    TOTAL_FRAMES.load(Ordering::Relaxed)
}

pub fn available_frames() -> usize {
    total_frames().saturating_sub(ALLOCATED_FRAMES.load(Ordering::Relaxed))
}

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
