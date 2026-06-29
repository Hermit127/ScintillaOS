use super::{TaskControlBlock, TaskStatus};
use crate::sync::UPSafeCell;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use lazy_static::*;
use log::*;
pub struct TaskManager {
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
}

/// A simple FIFO scheduler.
impl TaskManager {
    pub fn new() -> Self {
        Self {
            ready_queue: VecDeque::new(),
        }
    }
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        self.remove(task.getpid());
        self.ready_queue.push_back(task);
    }
    pub fn add_priority(&mut self, task: Arc<TaskControlBlock>) {
        self.remove(task.getpid());
        self.ready_queue.push_front(task);
    }
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.ready_queue.pop_front()
    }
    pub fn remove(&mut self, pid: usize) -> bool {
        let old_len = self.ready_queue.len();
        self.ready_queue.retain(|task| task.getpid() != pid);
        self.ready_queue.len() != old_len
    }
}

lazy_static! {
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
    pub static ref PID2TCB: UPSafeCell<BTreeMap<usize, Arc<TaskControlBlock>>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
}

pub fn add_task(task: Arc<TaskControlBlock>) {
    //trace!("os::task::manager::add_task");
    PID2TCB
        .exclusive_access()
        .insert(task.getpid(), Arc::clone(&task));
    TASK_MANAGER.exclusive_access().add(task);
}

pub fn add_priority_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.exclusive_access().add_priority(task);
}

pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    //trace!("os::task::manager::fetch_task");
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

pub fn ready_queue_len() -> usize {
    TASK_MANAGER.exclusive_access().ready_queue.len()
}

pub fn ready_queue_counts() -> Option<(usize, usize)> {
    let manager = TASK_MANAGER.try_borrow()?;
    let mut zombies = 0;
    for task in manager.ready_queue.iter() {
        if task
            .try_inner_borrow()
            .map(|inner| inner.task_status == TaskStatus::Zombie)
            .unwrap_or(false)
        {
            zombies += 1;
        }
    }
    Some((manager.ready_queue.len(), zombies))
}

pub fn remove_from_ready_queue(pid: usize) -> bool {
    TASK_MANAGER.exclusive_access().remove(pid)
}

pub fn pid2task(pid: usize) -> Option<Arc<TaskControlBlock>> {
    let map = PID2TCB.exclusive_access();
    map.get(&pid).map(Arc::clone)
}

pub fn pid2task_counts() -> Option<(usize, usize)> {
    let map = PID2TCB.try_borrow()?;
    let mut zombies = 0;
    for task in map.values() {
        if task
            .try_inner_borrow()
            .map(|inner| inner.task_status == TaskStatus::Zombie)
            .unwrap_or(false)
        {
            zombies += 1;
        }
    }
    Some((map.len(), zombies))
}

pub fn remove_from_pid2task(pid: usize) {
    let mut map = PID2TCB.exclusive_access();
    if map.remove(&pid).is_none() {
        warn!(
            "remove_from_pid2task: pid {} not found, may have been removed already",
            pid
        );
    }
}
