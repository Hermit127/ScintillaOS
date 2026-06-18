use crate::config::KERNEL_HEAP_SIZE;
use buddy_system_allocator::LockedHeap;

#[global_allocator]
static HEAP_ALLOCATOR: LockedHeap<32> = LockedHeap::empty();

#[alloc_error_handler]
pub fn handle_alloc_error(layout: core::alloc::Layout) -> ! {
    {
        let heap = HEAP_ALLOCATOR.lock();
        println!(
            "[heap-debug] user={} actual={} total={}",
            heap.stats_alloc_user(),
            heap.stats_alloc_actual(),
            heap.stats_total_bytes()
        );
    }
    crate::task::dump_task_stats_for_oom("heap_alloc_error");
    panic!("Heap allocation error, layout = {:#x?}", layout);
}

static mut HEAP_SPACE: [u8; KERNEL_HEAP_SIZE] = [0; KERNEL_HEAP_SIZE];

pub fn init_heap() {
    unsafe {
        HEAP_ALLOCATOR
            .lock()
            .init(HEAP_SPACE.as_ptr() as usize, KERNEL_HEAP_SIZE);
    }
}
