// 这里负责操作系统的内存管理：汇总内存管理模块，导出页表、地址空间、堆和物理页分配接口。
mod frame_allocator;
mod heap_allocator;
mod memory_set;
mod page_table;
mod vpn_range;

pub use frame_allocator::add_frames_range;
pub use frame_allocator::{
    allocated_frames, available_frames, frame_alloc, frame_alloc_failures, frame_alloc_persist,
    frame_dealloc, frames_alloc, total_frames, FrameTracker,
};
pub use heap_allocator::init_heap;
pub use memory_set::{
    get_elf_interp, get_elf_interp_file, last_page_fault_failure, LoadedElf, MapArea,
    MapPermission, MapType, MemorySet, PageFaultAccess, PageFaultResult, UnmapResult,
};
pub use page_table::{translated_byte_buffer, translated_ref, translated_refmut, translated_str};
