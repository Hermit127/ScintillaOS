// 这里负责操作系统的内存管理：初始化内核堆，并处理堆分配失败。
use crate::config::KERNEL_HEAP_SIZE;
use buddy_system_allocator::LockedHeap;
use core::alloc::{GlobalAlloc, Layout};
use polyhal::irq::IRQ;

struct IrqGuard {
    restore: bool,
}

impl IrqGuard {
    fn disable() -> Self {
        let restore = IRQ::int_enabled();
        IRQ::int_disable();
        Self { restore }
    }
}

impl Drop for IrqGuard {
    fn drop(&mut self) {
        if self.restore {
            IRQ::int_enable();
        }
    }
}

struct IrqSafeHeap {
    inner: LockedHeap<32>,
}

impl IrqSafeHeap {
    const fn empty() -> Self {
        Self {
            inner: LockedHeap::empty(),
        }
    }

    unsafe fn init(&self, start: usize, size: usize) {
        let _irq = IrqGuard::disable();
        self.inner.lock().init(start, size);
    }

    unsafe fn add_region(&self, start: usize, size: usize) {
        let _irq = IrqGuard::disable();
        self.inner.lock().add_to_heap(start, start + size);
    }

    fn stats(&self) -> (usize, usize, usize) {
        let _irq = IrqGuard::disable();
        let heap = self.inner.lock();
        (
            heap.stats_alloc_user(),
            heap.stats_alloc_actual(),
            heap.stats_total_bytes(),
        )
    }
}

unsafe impl GlobalAlloc for IrqSafeHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _irq = IrqGuard::disable();
        GlobalAlloc::alloc(&self.inner, layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _irq = IrqGuard::disable();
        GlobalAlloc::dealloc(&self.inner, ptr, layout);
    }
}

#[global_allocator]
static HEAP_ALLOCATOR: IrqSafeHeap = IrqSafeHeap::empty();

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
    unsafe {
        HEAP_ALLOCATOR.add_region(virt_start, size);
    }
}
