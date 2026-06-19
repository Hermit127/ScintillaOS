// 这里负责操作系统的调度：保存当前 CPU 的运行任务，并实现从 idle 到用户任务的调度切换。
use core::cell::LazyCell;

use super::TaskControlBlock;
use super::{fetch_task, TaskStatus};
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use lazy_static::*;
use lazyinit::LazyInit;
use log::*;
use polyhal::kcontext::{context_switch_pt, KContext};
use polyhal::pagetable::PageTable;
pub struct Processor {
    current: Option<Arc<TaskControlBlock>>,
    idle_task_cx: KContext,
}

impl Processor {
    // 初始化当前 CPU 的 processor 状态。
    // 执行时把当前任务置空，并准备一份 idle 上下文作为之后切换回内核的落脚点。
    pub fn new() -> Self {
        Self {
            current: None,
            idle_task_cx: KContext::blank(),
        }
    }
    // 返回 idle 上下文指针，调度切换时作为内核落脚点。
    // 执行时返回 idle_task_cx 的裸指针，底层 context_switch 需要直接写这个上下文。
    fn get_idle_task_cx_ptr(&mut self) -> *mut KContext {
        &mut self.idle_task_cx as *mut _
    }
    // 取走当前正在运行的任务。
    // 执行时把 processor.current 置空，并把原来的任务引用交给阻塞或退出路径继续处理。
    pub fn take_current(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.current.take()
    }
    // 克隆一份当前任务引用。
    // 执行时不取走 current，只克隆 Arc，让系统调用能临时访问当前任务。
    pub fn current(&self) -> Option<Arc<TaskControlBlock>> {
        self.current.as_ref().map(Arc::clone)
    }
}

lazy_static! {
    pub static ref PROCESSOR: UPSafeCell<Processor> = unsafe { UPSafeCell::new(Processor::new()) };
}

// 调度主循环：不断取 Ready 任务并从 idle 上下文切过去。
// 执行时从就绪队列取任务，标记为 Running，记录到 Processor.current，然后切到任务上下文和用户页表。
pub fn run_tasks() {
    trace!("os::task::processor::run_tasks");
    let mut first = true;
    loop {
        let mut processor = PROCESSOR.exclusive_access();
        if let Some(task) = fetch_task() {
            if first {
                println!("[kernel] run_tasks: first task pid={}", task.getpid());
                first = false;
            }
            let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
            // 切换前要独占下一个任务，设置 Running 状态并取出它的页表。
            let mut task_inner = task.inner_exclusive_access();
            let next_task_cx_ptr = &task_inner.task_cx as *const KContext;
            task_inner.task_status = TaskStatus::Running;
            let token = task_inner.memory_set.token();
            drop(task_inner);
            // processor.current 记录当前 CPU 正在运行的任务。
            processor.current = Some(task);
            drop(processor);
            // 从 idle 切到用户任务，同时切换到该任务的用户页表。
            unsafe { context_switch_pt(idle_task_cx_ptr, next_task_cx_ptr, token) }
        }
    }
}

// 对外取走当前任务，通常在阻塞或退出时调用。
// 执行时进入全局 Processor，把当前 CPU 上运行的任务移交给调用方。
pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.exclusive_access().take_current()
}

// 返回当前任务引用。
// 执行时进入全局 Processor，克隆当前正在 CPU 上运行的任务引用。
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.exclusive_access().current()
}

// 返回当前任务的用户页表 token。
// 执行时从当前任务 inner 里取 memory_set 的页表 token，用户指针翻译会用到。
pub fn current_user_token() -> PageTable {
    let task = current_task().unwrap();
    let token = task.inner_exclusive_access().get_user_token();
    token
}

static BOOT_PAGE_TABLE: LazyInit<PageTable> = LazyInit::new();

// 从当前任务切回 idle 上下文，等待下一轮调度。
// 执行时取出 idle 上下文指针，并用启动时记录的内核页表切回调度主循环。
pub fn schedule(switched_task_cx_ptr: *mut KContext) {
    let mut processor = PROCESSOR.exclusive_access();
    let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
    drop(processor);
    // 当前任务主动让出或退出时，先切回 idle，并恢复启动时记录的内核页表。
    unsafe {
        context_switch_pt(switched_task_cx_ptr, idle_task_cx_ptr, *BOOT_PAGE_TABLE);
    }
}

// 记录启动时的内核页表，之后调度回 idle 时使用。
// 执行时只初始化一次 BOOT_PAGE_TABLE，供 schedule 恢复内核地址空间。
pub fn init_kernel_page() {
    BOOT_PAGE_TABLE.init_once(PageTable::current());
}
