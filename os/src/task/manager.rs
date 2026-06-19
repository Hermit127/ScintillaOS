// 这里负责操作系统的调度：实现全局任务队列和 pid 到任务控制块的索引表。
use super::{TaskControlBlock, TaskStatus};
use crate::sync::UPSafeCell;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use lazy_static::*;
use log::*;
pub struct TaskManager {
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
}

// 简单的 FIFO 就绪队列。
impl TaskManager {
    // 创建空的任务管理器。
    // 执行时只初始化 FIFO 就绪队列，任务映射表由外层 lazy_static 单独维护。
    pub fn new() -> Self {
        Self {
            ready_queue: VecDeque::new(),
        }
    }
    // 把任务放到就绪队列尾部。
    // 执行时保持 FIFO 调度语义，新任务或普通唤醒任务追加到队尾。
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        self.ready_queue.push_back(task);
    }
    // 把任务插到就绪队列头部，给刚唤醒的关键任务使用。
    // 执行时把任务放到队头，让需要尽快响应的唤醒任务先被调度。
    pub fn add_priority(&mut self, task: Arc<TaskControlBlock>) {
        self.ready_queue.push_front(task);
    }
    // 从队列头取一个任务。
    // 执行时弹出队头任务，是否仍为 Ready 由外层 fetch_task 再检查。
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.ready_queue.pop_front()
    }
}

lazy_static! {
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
    pub static ref PID2TCB: UPSafeCell<BTreeMap<usize, Arc<TaskControlBlock>>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
}

// 注册 pid 到 TCB 的映射，并把任务加入就绪队列。
// 执行时先更新 PID2TCB 索引表，再把任务排到普通就绪队列尾部。
pub fn add_task(task: Arc<TaskControlBlock>) {
    PID2TCB
        .exclusive_access()
        .insert(task.getpid(), Arc::clone(&task));
    TASK_MANAGER.exclusive_access().add(task);
}

// 以较高优先级把任务放回就绪队列。
// 执行时不改变 pid 映射，只把任务插入就绪队列头部。
pub fn add_priority_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.exclusive_access().add_priority(task);
}

// 取出下一个 Ready 任务，跳过已经不再就绪的旧队列项。
// 执行时循环弹出队列项，只有仍处于 Ready 的任务才交给调度器运行。
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    loop {
        let task = TASK_MANAGER.exclusive_access().fetch()?;
        let is_ready = task
            .try_inner_borrow()
            .map(|inner| inner.task_status == TaskStatus::Ready)
            .unwrap_or(false);
        if is_ready {
            return Some(task);
        }
    }
}

// 返回当前就绪队列长度。
// 执行时只读取队列长度，用于调试或状态观测。
pub fn ready_queue_len() -> usize {
    TASK_MANAGER.exclusive_access().ready_queue.len()
}

// 通过 pid 查找任务控制块。
// 执行时从 PID2TCB 索引表按 pid 克隆任务引用，找不到就返回 None。
pub fn pid2task(pid: usize) -> Option<Arc<TaskControlBlock>> {
    let map = PID2TCB.exclusive_access();
    map.get(&pid).map(Arc::clone)
}

// 从 pid 映射表里移除任务。
// 执行时从 PID2TCB 删除 pid 项；如果已经被删过，只打印警告方便排查重复回收。
pub fn remove_from_pid2task(pid: usize) {
    let mut map = PID2TCB.exclusive_access();
    if map.remove(&pid).is_none() {
        warn!(
            "remove_from_pid2task: pid {} not found, may have been removed already",
            pid
        );
    }
}
