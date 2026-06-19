// 这里负责操作系统的内存管理：初始化内核堆，并处理堆分配失败。
use crate::config::KERNEL_HEAP_SIZE;
use buddy_system_allocator::LockedHeap;

#[global_allocator]
static HEAP_ALLOCATOR: LockedHeap<32> = LockedHeap::empty();

#[alloc_error_handler]
// 内核堆分配失败时直接 panic，带上失败的 layout。
// 执行时不尝试恢复，直接把失败的 Layout 打进 panic 信息，避免内核继续在无内存状态下运行。
pub fn handle_alloc_error(layout: core::alloc::Layout) -> ! {
    panic!("Heap allocation error, layout = {:#x?}", layout);
}

static mut HEAP_SPACE: [u8; KERNEL_HEAP_SIZE] = [0; KERNEL_HEAP_SIZE];

// 初始化内核全局堆。
// 执行时把静态 HEAP_SPACE 的地址和大小交给 LockedHeap，之后全局 allocator 才能服务 alloc。
pub fn init_heap() {
    unsafe {
        HEAP_ALLOCATOR
            .lock()
            .init(HEAP_SPACE.as_ptr() as usize, KERNEL_HEAP_SIZE);
    }
}
