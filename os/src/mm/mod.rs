// 这里负责操作系统的内存管理：汇总内存管理模块，导出页表、地址空间、堆和物理页分配接口。
mod file_page_cache;
mod frame_allocator;
mod heap_allocator;
mod memory_set;
mod page_table;
mod vpn_range;

pub use frame_allocator::add_frames_range;
pub use frame_allocator::{
    allocated_frames, available_frames, frame_alloc, frame_alloc_failures, frame_alloc_persist,
    frame_dealloc, frames_alloc, frames_alloc_batch, total_frames, FrameTracker,
};
#[cfg(feature = "smp-diagnostics")]
pub use frame_allocator::{
    take_watched_frame_corruption, watch_frame, watched_frame_is_intact, watched_frame_paddr,
};
pub use heap_allocator::{add_heap_region, init_heap};
pub use file_page_cache::invalidate_file_page_cache;
#[cfg(feature = "lock-stats")]
pub use file_page_cache::dump_file_page_cache_stats;
pub(crate) use file_page_cache::{
    cache_file_read, invalidate_file_page_cache_after_write, read_file_page_cache,
    reserve_file_page_cache_read, wait_for_file_page_load, FilePageCacheRead,
    FilePageCacheReadLoad, FilePageCacheWriteGuard, FILE_PAGE_READAHEAD_SIZE,
};
pub(crate) use frame_allocator::frames_dealloc_batch;
#[cfg(feature = "lock-stats")]
pub(crate) use file_page_cache::{
    record_file_page_cache_hit, record_file_page_cache_miss,
};
pub use memory_set::{
    get_elf_interp, get_elf_interp_file, last_page_fault_failure, AddressSpaceActivityHandle,
    LoadedElf, MapArea, MapPermission, MapType, MemorySet, PageFaultAccess, PageFaultResult,
    UnmapResult,
};
pub use page_table::{translated_byte_buffer, translated_ref, translated_refmut, translated_str};
