use super::{BlockReason, TaskControlBlock, TaskStatus};
use crate::config::MAX_CPUS;
use crate::sync::{PidRegistryLock, SchedulerLock};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::*;
use log::*;

/// Fixed first-generation RISC-V SMP topology.  CPU0 is online during early
/// boot; each secondary hart registers itself only after completing its local
/// scheduler initialization.
static ONLINE_CPU_MASK: AtomicUsize = AtomicUsize::new(1);

pub fn online_cpu_mask() -> usize {
    ONLINE_CPU_MASK.load(Ordering::Acquire)
}

/// Register a CPU which has completed its local scheduler initialization.
///
/// Do not infer this from the fixed topology: the default `QEMU_SMP=1` must
/// not enqueue work for harts which QEMU did not create.
pub fn register_online_cpu(cpu: usize) -> usize {
    assert!(cpu < MAX_CPUS, "invalid scheduler CPU {}", cpu);
    ONLINE_CPU_MASK.fetch_or(1usize << cpu, Ordering::AcqRel) | (1usize << cpu)
}
/// A timer waiter owned by the scheduler.
///
/// Waiting queues intentionally live next to the ready queue instead of being
/// independently accessible globals.  This gives the SMP conversion one owner
/// for queue membership and makes it possible to make task-state transitions
/// atomic under `SchedulerLock` in the next step.
pub struct SleepTask {
    pub wakeup_time: usize,
    pub task: Arc<TaskControlBlock>,
}

/// Scheduler queue selected by a blocking operation.
///
/// Futex has not yet been migrated to SchedulerLock, so it uses `None` while
/// its separate wait-table conversion is in progress.  Network, pipe, signal,
/// timeout and waitpid users are all represented here.
#[derive(Clone, Copy)]
pub enum BlockQueue {
    None,
    Net,
    WaitPid,
}

/// All scheduler-owned queues.
///
/// Queue entries are moved out while this lock is held, then their TCB state is
/// inspected or changed only after the guard is released.  In particular, a
/// scheduler lock must never be held across a network/VFS callback or a call
/// which can schedule.
pub struct SchedulerState {
    ready_queues: [VecDeque<Arc<TaskControlBlock>>; MAX_CPUS],
    running: [usize; MAX_CPUS],
    net_blocked: Vec<Arc<TaskControlBlock>>,
    waitpid_blocked: Vec<Arc<TaskControlBlock>>,
    sleeping: Vec<SleepTask>,
    exited: Vec<Arc<TaskControlBlock>>,
}

impl SchedulerState {
    pub fn new() -> Self {
        Self {
            ready_queues: core::array::from_fn(|_| VecDeque::new()),
            running: [0; MAX_CPUS],
            net_blocked: Vec::new(),
            waitpid_blocked: Vec::new(),
            sleeping: Vec::new(),
            exited: Vec::new(),
        }
    }

    fn select_cpu(&self, task: &TaskControlBlock) -> usize {
        let allowed = task.allowed_cpus() & online_cpu_mask();
        // A task's mask is validated before it reaches the scheduler.  Keep
        // CPU0 as a defensive fallback so a transient bad mask cannot index
        // outside the fixed topology.
        if allowed == 0 {
            return 0;
        }
        let mut selected = 0;
        let mut selected_len = usize::MAX;
        for cpu in 0..MAX_CPUS {
            if (allowed & (1 << cpu)) == 0 {
                continue;
            }
            let load = self.ready_queues[cpu].len().saturating_add(self.running[cpu]);
            if load < selected_len {
                selected = cpu;
                selected_len = load;
            }
        }
        selected
    }

    pub fn add(&mut self, task: Arc<TaskControlBlock>) -> usize {
        self.remove(task.getpid());
        let cpu = self.select_cpu(&task);
        task.mark_enqueued();
        self.ready_queues[cpu].push_back(task);
        cpu
    }
    pub fn add_priority(&mut self, task: Arc<TaskControlBlock>) -> usize {
        self.remove(task.getpid());
        let cpu = self.select_cpu(&task);
        task.mark_enqueued();
        self.ready_queues[cpu].push_front(task);
        cpu
    }
    pub fn fetch(&mut self, cpu: usize) -> Option<Arc<TaskControlBlock>> {
        let task = self.ready_queues.get_mut(cpu)?.pop_front()?;
        task.mark_dequeued();
        Some(task)
    }

    fn mark_running(&mut self, cpu: usize) {
        self.running[cpu] = self.running[cpu].saturating_add(1);
    }

    fn complete_running(&mut self, cpu: usize) {
        assert!(self.running[cpu] > 0, "hart {} completed an unaccounted task", cpu);
        self.running[cpu] -= 1;
    }
    pub fn remove(&mut self, pid: usize) -> bool {
        let mut removed = false;
        for queue in &mut self.ready_queues {
            let old_len = queue.len();
            queue.retain(|task| {
                let keep = task.getpid() != pid;
                if !keep {
                    task.mark_dequeued();
                }
                keep
            });
            removed |= queue.len() != old_len;
        }
        removed
    }
}

lazy_static! {
    /// The sole owner of ready, wait and delayed-reclaim queues.
    static ref SCHEDULER: SchedulerLock<SchedulerState> =
        SchedulerLock::new_named(SchedulerState::new(), "Scheduler.queues");
    /// The registry lock is never held while inspecting a TCB; callers must
    /// clone the Arc and release this guard first.
    static ref PID2TCB: PidRegistryLock<BTreeMap<usize, Arc<TaskControlBlock>>> =
        PidRegistryLock::new_named(BTreeMap::new(), "PidRegistry");
}

pub fn add_task(task: Arc<TaskControlBlock>) {
    //trace!("os::task::manager::add_task");
    PID2TCB
        .exclusive_access()
        .insert(task.getpid(), Arc::clone(&task));
    let target = {
        let mut scheduler = SCHEDULER.exclusive_access();
        scheduler.add(task)
    };
    // SBI calls are external firmware calls; never make one while the
    // scheduler lock is held.
    super::processor::request_reschedule(target);
}

pub fn add_priority_task(task: Arc<TaskControlBlock>) {
    let target = SCHEDULER.exclusive_access().add_priority(task);
    super::processor::request_reschedule(target);
}

/// Apply a validated affinity mask and, if the task is already Ready, move its
/// one ready-queue entry to the least-loaded allowed CPU.
///
/// Running tasks are deliberately not force-migrated in this first SMP stage:
/// their next yield, block, or timer preemption returns them through
/// `complete_context_switch`, which observes the new mask.  Keeping the whole
/// Ready transition under SchedulerLock prevents a second hart from fetching
/// the task while its queue ownership changes.
///
/// Returns `false` when the task is already a zombie and therefore no longer
/// has a schedulable affinity.
pub fn update_task_affinity(task: Arc<TaskControlBlock>, allowed: usize) -> bool {
    let target = {
        let mut scheduler = SCHEDULER.exclusive_access();
        let status = {
            let inner = task.inner_exclusive_access();
            inner.task_status
        };
        if status == TaskStatus::Zombie {
            return false;
        }
        task.set_allowed_cpus(allowed);
        if status == TaskStatus::Ready {
            assert!(
                scheduler.remove(task.getpid()),
                "ready pid {} has no scheduler queue entry during affinity update",
                task.getpid()
            );
            Some(scheduler.add(task))
        } else {
            None
        }
    };
    if let Some(target) = target {
        super::processor::request_reschedule(target);
    }
    true
}

pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    //trace!("os::task::manager::fetch_task");
    let cpu = super::processor::current_hart_id();
    let selected = {
        let mut scheduler = SCHEDULER.exclusive_access();
        loop {
            let task = scheduler.fetch(cpu)?;
            // Scheduler -> Task is the only permitted nested order.  The
            // running handoff is complete before the entry is exposed to a
            // CPU, preventing a second hart from observing this task as Ready.
            let mut inner = task.inner_exclusive_access();
            if inner.task_status == TaskStatus::Ready {
                inner.task_status = TaskStatus::Running;
                drop(inner);
                scheduler.mark_running(cpu);
                task.mark_running_on(cpu);
                break task;
            }
        }
    };
    Some(selected)
}

/// Complete an outgoing task's context switch from the owning CPU's idle
/// loop.  The task is deliberately absent from ready queues until this point.
///
/// The scheduler and TCB state transition are kept under the same
/// `SchedulerLock -> TaskLock` acquisition.  A wake that arrived during the
/// switch is represented by `WakePending` and is published here, after the
/// context is safe for another hart to restore.
pub fn complete_context_switch(task: Arc<TaskControlBlock>) {
    let mut scheduler = SCHEDULER.exclusive_access();
    super::processor::set_scheduler_phase(31);
    scheduler.complete_running(super::processor::current_hart_id());
    let mut inner = task.inner_exclusive_access();
    super::processor::set_scheduler_phase(32);
    let priority = match inner.task_status {
        TaskStatus::Switching => {
            inner.task_status = TaskStatus::Ready;
            false
        }
        TaskStatus::Blocking => {
            inner.task_status = TaskStatus::Blocked;
            return;
        }
        TaskStatus::WakePending => {
            inner.task_status = TaskStatus::Ready;
            true
        }
        status => panic!(
            "idle handoff on hart {} for pid {} (owner {}) in invalid state {:?}",
            super::processor::current_hart_id(),
            task.getpid(),
            task.running_cpu(),
            status
        ),
    };
    drop(inner);
    let target = if priority {
        scheduler.add_priority(task)
    } else {
        scheduler.add(task)
    };
    drop(scheduler);
    super::processor::request_reschedule(target);
}

pub fn scheduler_lock_owner() -> usize {
    SCHEDULER.owner()
}

/// Atomically begin a task's blocking transition before it leaves its CPU.
///
/// The task becomes `Blocking` and is entered into its wait queues while the
/// scheduler lock is held.  A concurrent waker can therefore either observe
/// the fully registered task or change it to `WakePending`; it can never put a
/// still-running task on a ready queue.
pub fn begin_block(
    task: &Arc<TaskControlBlock>,
    reason: Option<BlockReason>,
    queue: BlockQueue,
    wakeup_time: Option<usize>,
) -> *mut polyhal::kcontext::KContext {
    let mut scheduler = SCHEDULER.exclusive_access();
    // A running task should not already be queued, but clean up any stale
    // entry defensively before changing its state.
    scheduler.remove(task.getpid());
    let mut inner = task.inner_exclusive_access();
    debug_assert_eq!(inner.task_status, TaskStatus::Running);
    inner.task_status = TaskStatus::Blocking;
    inner.block_reason = reason;
    let task_cx_ptr = &mut inner.task_cx as *mut _;
    drop(inner);

    match queue {
        BlockQueue::None => {}
        BlockQueue::Net => {
            let _ = remove_task(&mut scheduler.net_blocked, task.getpid());
            scheduler.net_blocked.push(task.clone());
        }
        BlockQueue::WaitPid => {
            let _ = remove_task(&mut scheduler.waitpid_blocked, task.getpid());
            scheduler.waitpid_blocked.push(task.clone());
        }
    }
    if let Some(wakeup_time) = wakeup_time {
        if let Some(index) = scheduler
            .sleeping
            .iter()
            .position(|entry| entry.task.getpid() == task.getpid())
        {
            scheduler.sleeping.remove(index);
        }
        scheduler.sleeping.push(SleepTask {
            wakeup_time,
            task: task.clone(),
        });
    }
    task_cx_ptr
}

/// Cancel a block before the caller has switched away (for example, waitpid's
/// second condition check).  `WakePending` is also converted back to Running:
/// the current CPU has not surrendered this context, so it must not be queued.
pub fn cancel_block_before_switch(task: &Arc<TaskControlBlock>) {
    let mut scheduler = SCHEDULER.exclusive_access();
    let pid = task.getpid();
    let _ = remove_task(&mut scheduler.net_blocked, pid);
    let _ = remove_task(&mut scheduler.waitpid_blocked, pid);
    if let Some(index) = scheduler
        .sleeping
        .iter()
        .position(|entry| entry.task.getpid() == pid)
    {
        scheduler.sleeping.remove(index);
    }
    let mut inner = task.inner_exclusive_access();
    if matches!(inner.task_status, TaskStatus::Blocking | TaskStatus::WakePending) {
        inner.task_status = TaskStatus::Running;
    }
    inner.block_reason = None;
}

/// Wake a task through the scheduler state machine.
///
/// If the task is already fully blocked, it is made Ready immediately.  If it
/// is still switching out, only `WakePending` is recorded and the idle loop
/// will perform the eventual Ready enqueue after the context has been saved.
pub fn wake_task(task: Arc<TaskControlBlock>) -> bool {
    loop {
        let mut scheduler = SCHEDULER.exclusive_access();
        let Some(mut inner) = task.try_inner_exclusive_access() else {
            // Never spin on TaskLock while holding SchedulerLock.  The owner
            // may itself be entering a blocking/wakeup path and need the
            // scheduler lock to release that TaskLock.
            drop(scheduler);
            core::hint::spin_loop();
            continue;
        };
        let pid = task.getpid();
        let _ = remove_task(&mut scheduler.net_blocked, pid);
        let _ = remove_task(&mut scheduler.waitpid_blocked, pid);
        if let Some(index) = scheduler
            .sleeping
            .iter()
            .position(|entry| entry.task.getpid() == pid)
        {
            scheduler.sleeping.remove(index);
        }

        let enqueue_now = match inner.task_status {
            TaskStatus::Blocking => {
                inner.task_status = TaskStatus::WakePending;
                inner.block_reason = None;
                return true;
            }
            TaskStatus::Blocked => {
                inner.task_status = TaskStatus::Ready;
                inner.block_reason = None;
                true
            }
            TaskStatus::WakePending => return true,
            _ => false,
        };
        drop(inner);
        let target = enqueue_now.then(|| scheduler.add_priority(task.clone()));
        drop(scheduler);
        if let Some(target) = target {
            super::processor::request_reschedule(target);
        }
        return enqueue_now;
    }
}

/// Wake every pipe waiter matching one endpoint direction in one scheduler
/// transaction.  The old snapshot/`try_inner_borrow` implementation could
/// silently skip a waiter whose TaskLock was briefly held by another hart,
/// permanently losing the only wakeup for a one-byte ping-pong.
pub fn wake_pipe_waiters(pipe_id: usize, read_waiters: bool) {
    let mut all_targets = 0usize;
    loop {
        let mut contended = false;
        let mut scheduler = SCHEDULER.exclusive_access();
        let mut index = 0;
        while index < scheduler.net_blocked.len() {
            let task = scheduler.net_blocked[index].clone();
            let Some(mut inner) = task.try_inner_exclusive_access() else {
                contended = true;
                break;
            };
            let matches = match inner.block_reason {
                Some(BlockReason::PipeRead(id)) => read_waiters && id == pipe_id,
                Some(BlockReason::PipeWrite(id)) => !read_waiters && id == pipe_id,
                _ => false,
            };
            if !matches {
                index += 1;
                continue;
            }

            scheduler.net_blocked.remove(index);
            match inner.task_status {
                TaskStatus::Blocking => {
                    inner.task_status = TaskStatus::WakePending;
                    inner.block_reason = None;
                }
                TaskStatus::Blocked => {
                    inner.task_status = TaskStatus::Ready;
                    inner.block_reason = None;
                    drop(inner);
                    let target = scheduler.add_priority(task);
                    all_targets |= 1usize << target;
                }
                TaskStatus::WakePending => {
                    inner.block_reason = None;
                }
                _ => {
                    // A stale queue member must not survive this transaction.
                    inner.block_reason = None;
                }
            }
        }
        drop(scheduler);
        if !contended {
            break;
        }
        core::hint::spin_loop();
    }
    for cpu in 0..MAX_CPUS {
        if all_targets & (1usize << cpu) != 0 {
            super::processor::request_reschedule(cpu);
        }
    }
}

/// Remove scheduler membership and publish a terminal task state together.
///
/// Callers must remove a futex-table entry before this function, because the
/// futex table precedes SchedulerLock in the global order.  Resource teardown
/// (files, page tables and parent notification) deliberately happens after
/// this transaction has released both locks.
pub fn begin_exit(task: &Arc<TaskControlBlock>, exit_code: i32) -> bool {
    let mut scheduler = SCHEDULER.exclusive_access();
    let pid = task.getpid();
    scheduler.remove(pid);
    let _ = remove_task(&mut scheduler.net_blocked, pid);
    let _ = remove_task(&mut scheduler.waitpid_blocked, pid);
    if let Some(index) = scheduler
        .sleeping
        .iter()
        .position(|entry| entry.task.getpid() == pid)
    {
        scheduler.sleeping.remove(index);
    }

    let mut inner = task.inner_exclusive_access();
    let was_running = inner.task_status == TaskStatus::Running;
    let running_cpu = task.running_cpu();
    if inner.task_status == TaskStatus::Zombie {
        return false;
    }
    if was_running && running_cpu < MAX_CPUS {
        scheduler.complete_running(running_cpu);
    }
    inner.task_status = TaskStatus::Zombie;
    inner.exit_code = exit_code;
    inner.block_reason = None;
    true
}

pub fn ready_queue_len() -> usize {
    SCHEDULER
        .exclusive_access()
        .ready_queues
        .iter()
        .map(VecDeque::len)
        .sum()
}

pub fn ready_queue_counts() -> Option<(usize, usize)> {
    // Scheduler -> Task is the approved direction.  Count in place instead
    // of allocating a snapshot while the scheduler lock is held.
    let scheduler = SCHEDULER.try_borrow()?;
    let tasks = scheduler.ready_queues.iter().map(VecDeque::len).sum();
    let mut zombies = 0;
    for queue in &scheduler.ready_queues {
        for task in queue {
            if task
                .try_inner_borrow()
                .map(|inner| inner.task_status == TaskStatus::Zombie)
                .unwrap_or(false)
            {
                zombies += 1;
            }
        }
    }
    Some((tasks, zombies))
}

pub fn running_counts() -> Option<[usize; MAX_CPUS]> {
    Some(SCHEDULER.try_borrow()?.running)
}

pub fn remove_from_ready_queue(pid: usize) -> bool {
    SCHEDULER.exclusive_access().remove(pid)
}

fn remove_task(queue: &mut Vec<Arc<TaskControlBlock>>, pid: usize) -> Option<Arc<TaskControlBlock>> {
    queue
        .iter()
        .position(|task| task.getpid() == pid)
        .map(|index| queue.remove(index))
}

pub fn enqueue_net_blocked(task: Arc<TaskControlBlock>) {
    let pid = task.getpid();
    let old = {
        let mut scheduler = SCHEDULER.exclusive_access();
        // A task may have only one net/pipe/poll/signal wait entry.
        let old = remove_task(&mut scheduler.net_blocked, pid);
        scheduler.net_blocked.push(task);
        old
    };
    drop(old);
}

pub fn remove_net_blocked(pid: usize) -> Option<Arc<TaskControlBlock>> {
    remove_task(&mut SCHEDULER.exclusive_access().net_blocked, pid)
}

/// Copy references for an operation which must inspect a network device.
/// The caller must subsequently remove the selected PIDs with
/// [`take_net_blocked_by_pids`] before making them Ready.
pub fn net_blocked_snapshot() -> Vec<Arc<TaskControlBlock>> {
    SCHEDULER
        .exclusive_access()
        .net_blocked
        .iter()
        .cloned()
        .collect()
}

pub fn take_net_blocked_by_pids(pids: &[usize]) -> Vec<Arc<TaskControlBlock>> {
    // Reserve before taking the scheduler lock: no allocation is permitted
    // while it is held.  Queue membership is unique, hence at most one entry
    // can be returned per PID.
    let mut taken = Vec::with_capacity(pids.len());
    let mut scheduler = SCHEDULER.exclusive_access();
    let mut index = 0;
    while index < scheduler.net_blocked.len() {
        if pids
            .iter()
            .any(|pid| *pid == scheduler.net_blocked[index].getpid())
        {
            taken.push(scheduler.net_blocked.remove(index));
        } else {
            index += 1;
        }
    }
    taken
}

pub fn enqueue_waitpid(task: Arc<TaskControlBlock>) {
    let pid = task.getpid();
    let old = {
        let mut scheduler = SCHEDULER.exclusive_access();
        let old = remove_task(&mut scheduler.waitpid_blocked, pid);
        scheduler.waitpid_blocked.push(task);
        old
    };
    drop(old);
}

pub fn take_waitpid(pid: usize) -> Option<Arc<TaskControlBlock>> {
    remove_task(&mut SCHEDULER.exclusive_access().waitpid_blocked, pid)
}

pub fn enqueue_sleep(wakeup_time: usize, task: Arc<TaskControlBlock>) {
    let pid = task.getpid();
    let old = {
        let mut scheduler = SCHEDULER.exclusive_access();
        let old = scheduler
            .sleeping
            .iter()
            .position(|entry| entry.task.getpid() == pid)
            .map(|index| scheduler.sleeping.remove(index));
        scheduler.sleeping.push(SleepTask { wakeup_time, task });
        old
    };
    drop(old);
}

pub fn remove_sleep(pid: usize) -> Option<SleepTask> {
    let mut scheduler = SCHEDULER.exclusive_access();
    scheduler
        .sleeping
        .iter()
        .position(|entry| entry.task.getpid() == pid)
        .map(|index| scheduler.sleeping.remove(index))
}

pub fn take_due_sleep_tasks(now: usize) -> Vec<SleepTask> {
    let capacity = SCHEDULER.exclusive_access().sleeping.len();
    let mut due = Vec::with_capacity(capacity);
    let mut scheduler = SCHEDULER.exclusive_access();
    let mut index = 0;
    while index < scheduler.sleeping.len() {
        if scheduler.sleeping[index].wakeup_time <= now {
            due.push(scheduler.sleeping.remove(index));
        } else {
            index += 1;
        }
    }
    due
}

/// Remove a task from every scheduler-owned waiting queue.  Returned objects
/// are dropped only after their individual scheduler guards have been
/// released.
pub fn remove_from_wait_queues(pid: usize) -> bool {
    let net = remove_net_blocked(pid);
    let waitpid = take_waitpid(pid);
    let sleep = remove_sleep(pid);
    net.is_some() || waitpid.is_some() || sleep.is_some()
}

pub fn wait_queue_counts() -> Option<(usize, usize, usize)> {
    let scheduler = SCHEDULER.try_borrow()?;
    Some((
        scheduler.net_blocked.len(),
        scheduler.waitpid_blocked.len(),
        scheduler.sleeping.len(),
    ))
}

pub fn defer_drop_task(task: Arc<TaskControlBlock>) {
    SCHEDULER.exclusive_access().exited.push(task);
}

pub fn take_exited_tasks() -> Vec<Arc<TaskControlBlock>> {
    // Resource destruction must run only after the outgoing CPU has switched
    // to the boot page table.  Move the list out with no destructors under the
    // scheduler lock; the idle loop performs the actual inactive cleanup.
    core::mem::take(&mut SCHEDULER.exclusive_access().exited)
}

pub fn reap_exited_task(pid: usize) {
    let task = {
        let mut scheduler = SCHEDULER.exclusive_access();
        scheduler
            .exited
            .iter()
            .position(|task| task.getpid() == pid)
            .map(|index| scheduler.exited.remove(index))
    };
    drop(task);
}

pub fn pid2task(pid: usize) -> Option<Arc<TaskControlBlock>> {
    let map = PID2TCB.exclusive_access();
    map.get(&pid).map(Arc::clone)
}

/// Copy task references out of the PID registry.
///
/// The returned `Arc`s may be inspected after the registry guard has been
/// released.  This is the only bulk-lookup API; it prevents callers from
/// nesting `PidRegistryLock` with a task, scheduler, VFS, or device lock.
pub fn task_snapshot() -> Vec<Arc<TaskControlBlock>> {
    PID2TCB.exclusive_access().values().cloned().collect()
}

pub fn pid2task_counts() -> Option<(usize, usize)> {
    let tasks: Vec<_> = PID2TCB.try_borrow()?.values().cloned().collect();
    let mut zombies = 0;
    for task in &tasks {
        if task
            .try_inner_borrow()
            .map(|inner| inner.task_status == TaskStatus::Zombie)
            .unwrap_or(false)
        {
            zombies += 1;
        }
    }
    Some((tasks.len(), zombies))
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
