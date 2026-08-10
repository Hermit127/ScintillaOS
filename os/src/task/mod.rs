mod action;
mod fp;
mod manager;
mod pid;
mod processor;
mod signal;
#[allow(clippy::module_inception)]
mod task;

use crate::mm::{translated_ref, translated_refmut};
use crate::sync::SleepLock;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::mem::size_of;
#[cfg(feature = "lock-stats")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use lazy_static::*;
use log::*;
use manager::fetch_task;
use manager::remove_from_pid2task;
use manager::remove_from_ready_queue;
use polyhal::instruction::shutdown;
use polyhal::kcontext::KContext;
use polyhal_trap::trapframe::{TrapFrame, TrapFrameArgs};

pub use crate::config::SIGNAL_RETURN_TRAMPOLINE;
const SA_SIGINFO: usize = 4;

#[inline]
fn trap_gpr(ctx: &TrapFrame, reg: usize) -> usize {
    #[cfg(target_arch = "riscv64")]
    {
        ctx.x[reg]
    }
    #[cfg(target_arch = "loongarch64")]
    {
        ctx.regs[reg]
    }
    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    {
        let _ = (ctx, reg);
        0
    }
}

#[inline]
fn set_trap_gpr(ctx: &mut TrapFrame, reg: usize, value: usize) {
    #[cfg(target_arch = "riscv64")]
    {
        ctx.x[reg] = value;
    }
    #[cfg(target_arch = "loongarch64")]
    {
        ctx.regs[reg] = value;
    }
    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    {
        let _ = (ctx, reg, value);
    }
}
const SA_RESTORER: usize = 0x0400_0000;
const SA_NOCLDWAIT: usize = 2;
const SIG_IGN: usize = 1;
const SIGSEGV_NUM: usize = 11;
const SIGCHLD_NUM: usize = 17;
const SIGINFO_SIZE: usize = 128;
// RISC-V ucontext: the 168-byte flags/link/stack/sigmask prefix is padded to
// the sigcontext's 16-byte alignment, so mcontext begins at byte 176.  Its
// first word is PC, followed by x1..x31.
const UCONTEXT_SIZE: usize = 960;
const UCONTEXT_SIGMASK_OFFSET: usize = 40;
const UCONTEXT_MCONTEXT_OFFSET: usize = 176;
const UCONTEXT_PC_OFFSET: usize = UCONTEXT_MCONTEXT_OFFSET;
static SIGNAL_TRACE_COUNT: AtomicUsize = AtomicUsize::new(0);
const SIGNAL_TRACE_ENABLED: bool = false;

pub use action::{SignalAction, SignalActions};
pub use fp::UserFloatState;
pub use manager::{
    add_priority_task, add_task, begin_block, begin_exit, cancel_block_before_switch,
    net_blocked_snapshot, online_cpu_mask, pid2task, register_online_cpu, remove_sleep,
    publish_itimer_deadline, take_waitpid, task_snapshot, update_task_affinity,
    wake_local_tcp_acceptors, wake_local_tcp_connection_waiters, wake_local_tcp_readers,
    wake_local_tcp_writers, wake_all_sleep_lock_waiters, wake_all_sleep_lock_waiters_any,
    wake_exec_peer_waiters,
    wake_file_page_load_waiters, wake_pipe_waiters, wake_sleep_lock_waiter, wake_task,
    wake_udp_waiters, BlockQueue,
};
pub use pid::{pid_alloc, PidHandle};
pub use processor::{
    current_hart_id, current_task, current_user_token, flush_remote_tlb, handle_ipi,
    init_current_cpu, init_kernel_page, invalidate_user_float_state,
    mark_current_task_address_space_active, mark_user_float_state_live, record_timer_tick,
    request_reschedule, run_tasks, schedule, scheduler_cpu_snapshots, take_current_task,
    try_current_hart_id, user_float_state_is_live,
};
#[cfg(feature = "lock-stats")]
pub use processor::dump_scheduler_runtime_stats;
pub use signal::{PendingSignalInfo, SignalFlags, MAX_SIG};
pub use task::{
    blank_kcontext, BlockReason, KernelStack, TaskControlBlock, TaskControlBlockInner, TaskStatus,
};

pub(crate) struct FileTableGroup {
    files_id: usize,
    members: Vec<Weak<TaskControlBlock>>,
}

pub(crate) struct FileTableState {
    groups: Vec<FileTableGroup>,
}

impl FileTableState {
    const fn new() -> Self {
        Self { groups: Vec::new() }
    }
}

/// Serializes mutations of CLONE_FILES descriptor groups and owns their
/// membership registry. Mutations may briefly collect several ordered
/// TaskLocks, so unrelated groups sleep instead of spinning on another CPU.
pub(crate) static FILE_TABLE_LOCK: SleepLock<FileTableState> =
    SleepLock::new_named(FileTableState::new(), "Files.descriptor_tables");

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum SuspendReason {
    Other,
    TimerPreempt,
    Frozen,
    CopyToUserRetry,
    SysYield,
    ExecPeerWait,
    SigSuspendWait,
    FdInstallRetry,
    FdCloseRetry,
    FdDuplicateRetry,
    FdReplaceRetry,
    FdCloexecRetry,
    ForkSnapshotRetry,
    CloneSnapshotRetry,
    CloneFilesRetry,
    StdoutAcquire,
    StdinPoll,
    NetYield,
}

#[cfg(feature = "lock-stats")]
const SUSPEND_REASON_NAMES: [&str; 18] = [
    "other",
    "timer-preempt",
    "frozen",
    "copy-to-user-retry",
    "sys-yield",
    "exec-peer-wait",
    "sigsuspend-wait",
    "fd-install-retry",
    "fd-close-retry",
    "fd-duplicate-retry",
    "fd-replace-retry",
    "fd-cloexec-retry",
    "fork-snapshot-retry",
    "clone-snapshot-retry",
    "clone-files-retry",
    "stdout-acquire",
    "stdin-poll",
    "net-yield",
];
#[cfg(feature = "lock-stats")]
const SUSPEND_PID_SLOTS: usize = 128;
#[cfg(feature = "lock-stats")]
static SUSPEND_REASON_PID_COUNTS: [AtomicU64; SUSPEND_REASON_NAMES.len() * SUSPEND_PID_SLOTS] =
    [const { AtomicU64::new(0) }; SUSPEND_REASON_NAMES.len() * SUSPEND_PID_SLOTS];

#[cfg(feature = "lock-stats")]
pub(super) fn dump_suspend_reason_stats() {
    for (reason, name) in SUSPEND_REASON_NAMES.iter().enumerate() {
        for pid in 0..SUSPEND_PID_SLOTS {
            let count = SUSPEND_REASON_PID_COUNTS[reason * SUSPEND_PID_SLOTS + pid]
                .load(AtomicOrdering::Relaxed);
            if count != 0 {
                crate::println!(
                    "[suspend-stats] reason={} pid={} count={}",
                    name,
                    pid,
                    count,
                );
            }
        }
    }
}

pub(crate) fn register_files_task(task: &Arc<TaskControlBlock>) {
    let files_id = task.inner_exclusive_access().files_id;
    let mut state = FILE_TABLE_LOCK.lock();
    register_files_task_locked(&mut state, task, files_id);
}

pub(crate) fn register_files_task_locked(
    state: &mut FileTableState,
    task: &Arc<TaskControlBlock>,
    files_id: usize,
) {
    let group = if let Some(group) = state
        .groups
        .iter_mut()
        .find(|group| group.files_id == files_id)
    {
        group
    } else {
        state.groups.push(FileTableGroup {
            files_id,
            members: Vec::new(),
        });
        state.groups.last_mut().unwrap()
    };
    group.members.retain(|member| member.strong_count() != 0);
    if group.members.iter().all(|member| {
        member
            .upgrade()
            .map(|candidate| candidate.getpid() != task.getpid())
            .unwrap_or(true)
    }) {
        group.members.push(Arc::downgrade(task));
    }
}

pub(crate) fn file_group_tasks_locked(
    state: &mut FileTableState,
    task: &Arc<TaskControlBlock>,
    files_id: usize,
) -> Vec<Arc<TaskControlBlock>> {
    register_files_task_locked(state, task, files_id);
    let group = state
        .groups
        .iter_mut()
        .find(|group| group.files_id == files_id)
        .unwrap();
    let mut tasks = Vec::new();
    group.members.retain(|member| {
        let Some(task) = member.upgrade() else {
            return false;
        };
        let live = !task.is_waitable_zombie();
        if live {
            tasks.push(task);
        }
        live
    });
    tasks
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TimeSpec {
    pub tv_sec: usize,
    pub tv_nsec: usize,
}

fn defer_drop_task(task: Arc<TaskControlBlock>) {
    manager::defer_drop_task(task);
}

/// Finish Linux's vfork parent suspension after the child has either
/// committed a new image or begun exiting.
///
/// `take_vfork_parent` is the one-shot publication point.  Waking through the
/// normal scheduler state machine makes completion safe whether it races
/// before block registration, during `Blocking`, or after the context switch.
pub fn complete_vfork(task: &Arc<TaskControlBlock>) {
    let Some(parent_tid) = task.take_vfork_parent() else {
        return;
    };
    if let Some(parent) = pid2task(parent_tid) {
        manager::wake_vfork_parent(parent, task.getpid());
    }
}

pub(super) fn reap_exited_tasks() {
    if let Some(task) = manager::take_exited_task() {
        #[cfg(feature = "smp-diagnostics")]
        if task.getpid() >= 20 {
            println!("[reap-diag] begin tid={}", task.getpid());
        }
        processor::set_scheduler_phase(40);
        let replacement_memory_set = crate::mm::MemorySet::new_bare();
        let (parent, mut retired_memory_set, files_to_close) = {
            let mut inner = task.inner_exclusive_access();
            processor::set_scheduler_phase(41);
            let parent = inner.parent.as_ref().and_then(|weak| weak.upgrade());
            let retired_memory_set =
                core::mem::replace(&mut inner.memory_set, replacement_memory_set);
            let files = inner.drain_fd_table();
            (parent, retired_memory_set, files)
        };
        // Page-table and frame destruction can take milliseconds for compiler
        // processes. The task is already inactive, so keep that work outside
        // TaskLock and with local interrupts enabled.
        retired_memory_set.recycle_data_pages_inactive();
        processor::set_scheduler_phase(42);
        processor::set_scheduler_phase(43);
        for file in files_to_close {
            file.close_ref();
        }
        processor::set_scheduler_phase(44);
        task.publish_exit_cleanup_done();
        #[cfg(feature = "smp-diagnostics")]
        if task.getpid() >= 20 {
            println!("[reap-diag] cleanup-done tid={}", task.getpid());
        }
        if let Some(parent) = parent {
            wake_parent_group_waiters(&parent);
            #[cfg(feature = "smp-diagnostics")]
            if task.getpid() >= 20 {
                println!("[reap-diag] parent-woken tid={}", task.getpid());
            }
        }
    }
}

pub fn reap_exited_task(pid: usize) {
    manager::reap_exited_task(pid);
}

pub fn suspend_current_and_run_next() {
    suspend_current_and_run_next_reason(SuspendReason::Other);
}

pub(crate) fn suspend_current_and_run_next_reason(reason: SuspendReason) {
    //trace!("os::task::suspend_current_and_run_next");
    // There must be an application running.
    let task = current_task().unwrap();

    #[cfg(feature = "lock-stats")]
    {
        let pid = task.getpid();
        if pid < SUSPEND_PID_SLOTS {
            SUSPEND_REASON_PID_COUNTS[reason as usize * SUSPEND_PID_SLOTS + pid]
                .fetch_add(1, AtomicOrdering::Relaxed);
        }
    }
    #[cfg(not(feature = "lock-stats"))]
    let _ = reason;

    //added for kernel time accounting
    task.record_time_suspend();
    // ---- access current TCB exclusively
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut KContext;
    // Do not publish this task as Ready until the context switch has returned
    // to this CPU's idle loop.  Publishing it here would let a second hart
    // run it before `context_switch_pt` saved `task_cx`.
    task_inner.task_status = TaskStatus::Switching;
    drop(task_inner);
    // ---- release current PCB

    // Keep `current` and its running_cpu ownership until the switch has
    // completed.  `run_tasks` takes it and publishes Ready from the idle
    // context, after the task's context is valid for another hart to use.
    schedule(task_cx_ptr);
    //added fot reset time accounting
    task.record_time_resume();
}

pub fn sleep_current_and_run_next() {
    crate::fs::stdio::stdout_release_if_owner();
    let task = current_task().unwrap();
    task.record_time_suspend();
    let task_cx_ptr = manager::begin_block(&task, None, manager::BlockQueue::None, None);
    schedule(task_cx_ptr);
    task.record_time_resume();
}

pub fn sleep_current_until(wakeup_time: usize) {
    crate::fs::stdio::stdout_release_if_owner();
    let task = current_task().unwrap();
    task.record_time_suspend();
    let task_cx_ptr = manager::begin_block(
        &task,
        None,
        manager::BlockQueue::None,
        Some(wakeup_time),
    );
    schedule(task_cx_ptr);
    task.record_time_resume();
    let _ = manager::remove_sleep(task.getpid());
}

pub fn sleep_current_until_if<F>(wakeup_time: usize, should_sleep: F) -> bool
where
    F: FnOnce() -> bool,
{
    crate::fs::stdio::stdout_release_if_owner();
    let task = current_task().unwrap();
    task.record_time_suspend();
    let task_cx_ptr =
        manager::begin_block(&task, None, manager::BlockQueue::None, Some(wakeup_time));
    if !should_sleep() {
        manager::cancel_block_before_switch(&task);
        task.record_time_resume();
        return false;
    }
    schedule(task_cx_ptr);
    task.record_time_resume();
    let _ = manager::remove_sleep(task.getpid());
    true
}

pub fn block_current_and_run_next(block_reason: BlockReason) {
    let _ = block_current_and_run_next_if(block_reason, || true);
}

/// Publish the current task as a waiter, recheck the blocking condition, and
/// only then switch away.
///
/// A wakeup may happen between an object's initial condition check and
/// `begin_block`.  Registering first and invoking `should_block` afterwards
/// closes that lost-wakeup window.  If a waker races after the recheck,
/// `wake_task` records `WakePending` and the idle handoff publishes the task
/// after its context has been saved.
pub fn block_current_and_run_next_if<F>(
    block_reason: BlockReason,
    should_block: F,
) -> bool
where
    F: FnOnce() -> bool,
{
    crate::fs::stdio::stdout_release_if_owner();
    let task = current_task().unwrap();
    task.record_time_suspend();
    let task_cx_ptr = manager::begin_block(
        &task,
        Some(block_reason),
        manager::BlockQueue::Net,
        None,
    );
    if !should_block() {
        manager::cancel_block_before_switch(&task);
        task.record_time_resume();
        return false;
    }
    schedule(task_cx_ptr);
    task.record_time_resume();
    let _ = manager::remove_net_blocked(task.getpid());
    true
}

// Poll-style waits need both the post-registration condition check and a
// deadline, so they wait on the I/O and timer queues at the same time.
pub fn block_current_until_if<F>(
    block_reason: BlockReason,
    wakeup_time: usize,
    should_block: F,
) -> bool
where
    F: FnOnce() -> bool,
{
    crate::fs::stdio::stdout_release_if_owner();
    let task = current_task().unwrap();
    task.record_time_suspend();
    let task_cx_ptr = manager::begin_block(
        &task,
        Some(block_reason),
        manager::BlockQueue::Net,
        Some(wakeup_time),
    );
    if !should_block() {
        manager::cancel_block_before_switch(&task);
        task.record_time_resume();
        return false;
    }
    schedule(task_cx_ptr);
    task.record_time_resume();
    let _ = manager::remove_net_blocked(task.getpid());
    let _ = manager::remove_sleep(task.getpid());
    true
}

pub fn block_current_until(block_reason: BlockReason, wakeup_time: Option<usize>) {
    crate::fs::stdio::stdout_release_if_owner();
    let task = current_task().unwrap();
    task.record_time_suspend();
    let task_cx_ptr = manager::begin_block(
        &task,
        Some(block_reason),
        manager::BlockQueue::Net,
        wakeup_time,
    );
    schedule(task_cx_ptr);
    task.record_time_resume();
    let _ = manager::remove_net_blocked(task.getpid());
    if wakeup_time.is_some() {
        let _ = manager::remove_sleep(task.getpid());
    }
}

pub fn wake_poll_wait_tasks() {
    manager::wake_poll_waiters();
}

pub fn wake_pipe_read_tasks(pipe_id: usize) {
    wake_pipe_tasks(pipe_id, true);
}

pub fn wake_pipe_write_tasks(pipe_id: usize) {
    wake_pipe_tasks(pipe_id, false);
}

fn wake_pipe_tasks(pipe_id: usize, read_waiters: bool) {
    manager::wake_pipe_waiters(pipe_id, read_waiters);
}

pub fn wake_signal_wait_task(task: &Arc<TaskControlBlock>, signal: SignalFlags) -> bool {
    let should_wake = task
        .try_inner_borrow()
        .map(|inner| match inner.block_reason {
            Some(BlockReason::SignalWait(mask)) => mask.contains(signal),
            _ => false,
        })
        .unwrap_or(false);
    should_wake && manager::wake_task(task.clone())
}

pub fn wake_blocked_task(task: &Arc<TaskControlBlock>) {
    let pid = task.getpid();
    #[cfg(any(feature = "smp-diagnostics", feature = "timeout-trace"))]
    let before = task
        .try_inner_borrow()
        .map(|inner| inner.task_status);
    let _ = crate::syscall::process::remove_futex_waiter(pid);
    let woke = manager::wake_task(task.clone());
    #[cfg(any(feature = "smp-diagnostics", feature = "timeout-trace"))]
    if pid >= 3 {
        let after = task
            .try_inner_borrow()
            .map(|inner| inner.task_status);
        crate::println!(
            "[wake-diag] tid={} before={:?} after={:?} woke={}",
            pid,
            before,
            after,
            woke,
        );
    }
}

pub fn check_itimers() {
    let now = polyhal::time::Time::now().to_usec() as u64;
    if !manager::itimer_scan_due(now) {
        return;
    }
    let tasks = task_snapshot();
    let expired = Vec::with_capacity(tasks.len());
    let Some(expired) = manager::scan_itimers_if_due(now, move || {
        let mut expired = expired;
        let mut next_deadline = u64::MAX;
        for task in tasks {
            let mut inner = task.inner_exclusive_access();
            if inner.task_status == TaskStatus::Zombie || inner.itimer_expire == 0 {
                continue;
            }
            let expired_now = now >= inner.itimer_expire;
            if expired_now {
                inner.signals.insert(SignalFlags::SIGALRM);
                if inner.itimer_interval_us > 0 {
                    inner.itimer_expire = now.saturating_add(inner.itimer_interval_us);
                } else {
                    inner.itimer_expire = 0;
                }
            }
            if inner.itimer_expire != 0 {
                next_deadline = next_deadline.min(inner.itimer_expire);
            }
            drop(inner);
            if expired_now {
                expired.push(task);
            }
        }
        (expired, next_deadline)
    }) else {
        return;
    };
    for task in expired {
        #[cfg(any(feature = "smp-diagnostics", feature = "timeout-trace"))]
        crate::println!(
            "[itimer-diag] expire tid={} now={}",
            task.getpid(),
            now,
        );
        wake_blocked_task(&task);
    }
}

fn notify_parent_sigchld(parent: &Arc<TaskControlBlock>) {
    {
        let mut inner = parent.inner_exclusive_access();
        inner.signals.insert(SignalFlags::SIGCHLD);
    }
    let woke_signal_waiter = wake_signal_wait_task(parent, SignalFlags::SIGCHLD);
    let (should_interrupt, waiting_for_vfork) = parent
        .try_inner_borrow()
        .map(|inner| {
            (
                has_interrupting_signal(&inner),
                matches!(inner.block_reason, Some(BlockReason::Vfork(_))),
            )
        })
        .unwrap_or((false, false));
    if should_interrupt && !waiting_for_vfork && !woke_signal_waiter {
        wake_blocked_task(parent);
    }
}

fn wake_parent_waiter(parent: &Arc<TaskControlBlock>) {
    let before = parent
        .try_inner_borrow()
        .map(|inner| inner.task_status);
    let woke = manager::wake_waitpid_parent(parent.clone());
    if LIFECYCLE_TRACE_ENABLED {
        let after = parent
            .try_inner_borrow()
            .map(|inner| inner.task_status);
        println!(
            "[lifecycle] wake parent={} before={:?} after={:?} woke={}",
            parent.getpid(),
            before,
            after,
            woke
        );
    }
}

fn wake_parent_group_waiters(parent: &Arc<TaskControlBlock>) {
    let tgid = parent.thread_group_id();
    wake_parent_waiter(parent);
    for peer in collect_thread_group_tasks(parent.getpid(), tgid) {
        wake_parent_waiter(&peer);
    }
}

fn parent_auto_reaps_sigchld(parent: &Arc<TaskControlBlock>) -> bool {
    parent
        .try_inner_borrow()
        .map(|inner| {
            let action = inner.signal_actions.lock().table[SIGCHLD_NUM];
            action.handler == SIG_IGN || (action.flags & SA_NOCLDWAIT) != 0
        })
        .unwrap_or(false)
}

fn detach_child_from_parent(parent: &Arc<TaskControlBlock>, child_pid: usize) {
    let removed = {
        let mut parent_inner = parent.inner_exclusive_access();
        parent_inner
            .children
            .iter()
            .position(|child| child.getpid() == child_pid)
            .map(|idx| parent_inner.children.remove(idx))
    };
    drop(removed);
}

fn is_thread_group_leader(task: &Arc<TaskControlBlock>) -> bool {
    task.try_inner_borrow()
        .map(|inner| inner.tgid == task.getpid())
        .unwrap_or(false)
}

fn remove_task_from_block_queues(pid: usize) -> bool {
    crate::syscall::process::remove_futex_waiter(pid)
}

pub fn force_exit_task(task: &Arc<TaskControlBlock>, exit_code: i32) {
    let pid = task.getpid();
    let target_is_current = current_task()
        .map(|current| current.getpid() == pid)
        .unwrap_or(false);
    // Never publish or tear down a non-current task as Zombie from the
    // caller's hart.  A Ready/Blocked task can concurrently be in a wake or
    // dispatch handoff just as a remotely Running task can still own its trap
    // frame.  Publish SIGKILL, wake it if necessary, and let its local trap
    // path perform the single authoritative begin_exit transition.
    let running_cpu = task.running_cpu();
    if !target_is_current {
        let mut inner = task.inner_exclusive_access();
        if inner.task_status != TaskStatus::Zombie {
            inner.killed = true;
            inner.exit_code = exit_code;
            inner.exit_signal = exit_code;
            inner.terminating_for_exec = false;
            inner.terminating_for_group = false;
            inner.signals.insert(SignalFlags::SIGKILL);
        }
        drop(inner);
        manager::wake_task(task.clone());
        if running_cpu != task::NO_RUNNING_CPU {
            processor::request_reschedule(running_cpu);
        }
        return;
    }
    let notify_parent = {
        let inner = task.inner_exclusive_access();
        inner.tgid == pid
    };
    crate::fs::stdio::stdout_release_pid(pid);
    let _ = remove_task_from_block_queues(pid);
    if !manager::begin_exit(task, exit_code) {
        return;
    }
    manager::wake_exec_peer_waiters(task.thread_group_id());
    crate::syscall::process::cleanup_task_exit_futexes(task);
    remove_from_pid2task(pid);

    let (parent, orphan_children, files_to_close) = {
        let mut inner = task.inner_exclusive_access();
        let parent = inner.parent.as_ref().and_then(|w| w.upgrade());
        let orphan_children = core::mem::take(&mut inner.children);
        if target_is_current {
            inner.memory_set.recycle_data_pages();
        } else {
            inner.memory_set.recycle_data_pages_inactive();
        }
        let files_to_close = inner.drain_fd_table();
        (parent, orphan_children, files_to_close)
    };
    for file in files_to_close {
        file.close_ref();
    }
    task.publish_exit_cleanup_done();

    if notify_parent {
        if let Some(parent) = parent {
            if parent_auto_reaps_sigchld(&parent) {
                detach_child_from_parent(&parent, pid);
                wake_parent_group_waiters(&parent);
            } else {
                notify_parent_sigchld(&parent);
                wake_parent_group_waiters(&parent);
            }
        }
    }

    if !orphan_children.is_empty() {
        let mut initproc_inner = INITPROC.inner_exclusive_access();
        for child in orphan_children.iter() {
            child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
            initproc_inner.children.push(child.clone());
        }
    }
}

fn request_group_exit(task: &Arc<TaskControlBlock>, exit_code: i32) {
    let running_cpu = task.running_cpu();
    {
        let mut inner = task.inner_exclusive_access();
        if inner.task_status == TaskStatus::Zombie {
            return;
        }
        inner.killed = true;
        inner.exit_code = exit_code;
        inner.exit_signal = exit_code;
        inner.terminating_for_exec = false;
        inner.terminating_for_group = true;
        inner.signals.insert(SignalFlags::SIGKILL);
    }
    manager::wake_task(task.clone());
    if running_cpu != task::NO_RUNNING_CPU {
        processor::request_reschedule(running_cpu);
    }
}

fn collect_thread_group_tasks(pid: usize, tgid: usize) -> Vec<Arc<TaskControlBlock>> {
    let mut group_tasks = Vec::new();
    for other in task_snapshot() {
        if other.getpid() == pid {
            continue;
        }
        // Linux task-list membership is independent of a thread's mutable
        // syscall state. The published TGID and terminal-state bit provide a
        // stable snapshot without waiting behind file-backed page faults.
        if other.thread_group_id() == tgid && !other.is_waitable_zombie() {
            group_tasks.push(other);
        }
    }
    group_tasks
}

pub fn force_exit_task_group(task: &Arc<TaskControlBlock>, exit_code: i32) {
    let (pid, tgid) = {
        let inner = task.inner_exclusive_access();
        (task.getpid(), inner.tgid)
    };
    #[cfg(feature = "smp-diagnostics")]
    if pid >= 3 {
        crate::println!("[exit-diag] tid={} collect-group tgid={}", pid, tgid);
    }
    let group_tasks = collect_thread_group_tasks(pid, tgid);
    #[cfg(feature = "smp-diagnostics")]
    if pid >= 3 {
        crate::println!("[exit-diag] tid={} group-members={}", pid, group_tasks.len());
    }
    for other in group_tasks {
        request_group_exit(&other, exit_code);
    }
}

/// Stop every other thread before the caller replaces the shared address
/// space during execve. Linux retires sibling threads, but the execing thread
/// remains alive; this is deliberately distinct from a process-wide SIGKILL.
pub fn terminate_other_threads_for_exec(task: &Arc<TaskControlBlock>) {
    let (pid, tgid) = {
        let inner = task.inner_exclusive_access();
        (task.getpid(), inner.tgid)
    };
    let group_tasks = collect_thread_group_tasks(pid, tgid);
    for other in group_tasks {
        let running_cpu = other.running_cpu();
        {
            let mut inner = other.inner_exclusive_access();
            if inner.task_status == TaskStatus::Zombie {
                continue;
            }
            inner.killed = true;
            inner.exit_code = 0;
            inner.exit_signal = 0;
            inner.terminating_for_exec = true;
            inner.terminating_for_group = false;
        }
        manager::wake_task(other.clone());
        if running_cpu != task::NO_RUNNING_CPU {
            processor::request_reschedule(running_cpu);
        }
    }
}

pub fn dump_task_stats_for_oom(context: &str) {
    let (pid_count, pid_zombies) = manager::pid2task_counts().unwrap_or((usize::MAX, usize::MAX));
    let (ready_count, ready_zombies) =
        manager::ready_queue_counts().unwrap_or((usize::MAX, usize::MAX));
    let (net_blocked, waitpid_blocked, sleeping) =
        manager::wait_queue_counts().unwrap_or((usize::MAX, usize::MAX, usize::MAX));
    let (init_children, init_zombies) = INITPROC
        .try_inner_borrow()
        .map(|inner| {
            let mut zombies = 0;
            for child in inner.children.iter() {
                if child
                    .try_inner_borrow()
                    .map(|child_inner| child_inner.task_status == TaskStatus::Zombie)
                    .unwrap_or(false)
                {
                    zombies += 1;
                }
            }
            (inner.children.len(), zombies)
        })
        .unwrap_or((usize::MAX, usize::MAX));
    let current_pid = current_task().map(|task| task.getpid()).unwrap_or(0);
    println!(
        "[task-debug] {} pid2task={}/{} ready={}/{} net={} waitpid={} sleep={} init_children={}/{} current={}",
        context,
        pid_count,
        pid_zombies,
        ready_count,
        ready_zombies,
        net_blocked,
        waitpid_blocked,
        sleeping,
        init_children,
        init_zombies,
        current_pid
    );
}

#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
static STALL_SNAPSHOT_PRINTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
static STALL_SNAPSHOT_TICKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
const SCHED_STALL_DIAGNOSTICS: bool = cfg!(any(
    feature = "smp-diagnostics",
    feature = "scheduler-stall-diagnostics"
));
// Lifecycle state remains available through the diagnostic snapshot.  Per
// exit logging is disabled during acceptance so independent harts cannot
// splice text into official test markers.
const LIFECYCLE_TRACE_ENABLED: bool = cfg!(feature = "timeout-trace");

/// Emit one read-only scheduler snapshot when all runnable work has vanished
/// behind at least two wait registrations.  This is a low-frequency kernel
/// diagnostic only: it neither wakes tasks nor changes benchmark timing.
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
pub fn diagnose_scheduler_stall() {
    use core::sync::atomic::Ordering;

    if !SCHED_STALL_DIAGNOSTICS {
        return;
    }
    let phases = processor::scheduler_phases();
    // Exit cleanup runs from an idle hart.  If it spins while holding a lock,
    // runnable work may still exist and the queue-based detector below will
    // never arm.  Observe only the diagnostic phase atomics here, so another
    // hart can report the stuck stage without acquiring the contested lock.
    if phases
        .iter()
        .any(|phase| matches!(phase, 31 | 32 | 40..=43))
    {
        if STALL_SNAPSHOT_TICKS.fetch_add(1, Ordering::AcqRel) >= 200
            && !STALL_SNAPSHOT_PRINTED.swap(true, Ordering::AcqRel)
        {
            println!(
                "[cleanup-stall] phases={:?} scheduler_owner={}",
                phases,
                manager::scheduler_lock_owner()
            );
            crate::sync::dump_lockdep_state();
        }
    }
    let Some((ready, _)) = manager::ready_queue_counts() else {
        return;
    };
    let Some((net, waitpid, sleeping)) = manager::wait_queue_counts() else {
        return;
    };
    if ready != 0 || net + waitpid + sleeping < 2 {
        STALL_SNAPSHOT_TICKS.store(0, Ordering::Release);
        STALL_SNAPSHOT_PRINTED.store(false, Ordering::Release);
        return;
    }
    let Some(running) = manager::running_counts() else {
        return;
    };
    // A quiet, CPU-bound or block-I/O-heavy process can legitimately be the
    // only runnable task while several ancestor shells waitpid.  The old
    // queue-only heuristic labelled that normal state a deadlock.  Running
    // hangs are covered by lockdep and the host no-output watchdog; this
    // queue detector is specifically for work lost with no CPU owner.
    if running.iter().copied().sum::<usize>() != 0 {
        STALL_SNAPSHOT_TICKS.store(0, Ordering::Release);
        STALL_SNAPSHOT_PRINTED.store(false, Ordering::Release);
        return;
    }
    let tasks = task_snapshot();
    if tasks.len() < 4 {
        STALL_SNAPSHOT_TICKS.store(0, Ordering::Release);
        return;
    }
    if STALL_SNAPSHOT_TICKS.fetch_add(1, Ordering::AcqRel) < 100 {
        return;
    }
    if STALL_SNAPSHOT_PRINTED.swap(true, Ordering::AcqRel) {
        return;
    }
    println!(
        "[sched-stall] ready={} net={} waitpid={} sleeping={} phases={:?} scheduler_owner={} syscall={} pid={} args=({:#x},{:#x},{:#x})",
        ready,
        net,
        waitpid,
        sleeping,
        processor::scheduler_phases(),
        manager::scheduler_lock_owner(),
        crate::syscall::LAST_SYSCALL_ID.load(Ordering::Relaxed),
        crate::syscall::LAST_SYSCALL_PID.load(Ordering::Relaxed),
        crate::syscall::LAST_SYSCALL_ARG0.load(Ordering::Relaxed),
        crate::syscall::LAST_SYSCALL_ARG1.load(Ordering::Relaxed),
        crate::syscall::LAST_SYSCALL_ARG2.load(Ordering::Relaxed),
    );
    for (cpu, snapshot) in processor::scheduler_cpu_snapshots().iter().enumerate() {
        println!(
            "[sched-stall] hart={} phase={} tid={} tgid={} tcb={:#x} pending_ipi={:#x}",
            cpu,
            snapshot.phase,
            snapshot.pid,
            snapshot.tgid,
            snapshot.tcb,
            snapshot.pending_ipi
        );
    }
    println!("[sched-stall] running-counts={:?}", running);
    crate::sync::dump_lockdep_state();
    for task in tasks {
        if let Some(inner) = task.try_inner_borrow() {
            println!(
                "[sched-stall] tid={} tgid={} exe={} tcb={:#x} owner={} queued={} state={:?} reason={:?} children={}",
                task.getpid(),
                inner.tgid,
                inner.executable_path,
                Arc::as_ptr(&task) as usize,
                task.running_cpu(),
                task.is_enqueued(),
                inner.task_status,
                inner.block_reason,
                inner.children.len()
            );
        }
    }
    #[cfg(any(
        feature = "smp-diagnostics",
        feature = "scheduler-stall-diagnostics"
    ))]
    crate::syscall::process::dump_futex_waiters();
}

pub const IDLE_PID: usize = 0;

/// Exit the current 'Running' task and run the next task in task list.
pub fn exit_current_and_run_next(exit_code: i32) -> ! {
    crate::fs::stdio::stdout_release_if_owner();
    trace!("os::task::exit_current_and_run_next");
    // take from Processor
    let task = current_task().unwrap();

    let pid = task.getpid();
    #[cfg(feature = "smp-diagnostics")]
    let exit_diag = pid >= 3;
    #[cfg(feature = "smp-diagnostics")]
    if exit_diag {
        println!("[exit-diag] tid={} exit-enter", pid);
    }
    complete_vfork(&task);
    if LIFECYCLE_TRACE_ENABLED {
        println!("[lifecycle] exit-enter pid={} code={}", pid, exit_code);
    }
    if pid == IDLE_PID {
        println!(
            "[kernel] Idle process exit with exit_code {} ...",
            exit_code
        );
        shutdown();
    }

    // remove from pid2task
    let _ = crate::syscall::process::remove_futex_waiter(task.getpid());
    if !manager::begin_exit(&task, exit_code) {
        panic!("current task {} exited twice", task.getpid());
    }
    manager::wake_exec_peer_waiters(task.thread_group_id());
    #[cfg(feature = "smp-diagnostics")]
    if exit_diag {
        println!("[exit-diag] tid={} exit-published", pid);
    }
    if LIFECYCLE_TRACE_ENABLED {
        println!("[lifecycle] exit-zombie pid={}", pid);
    }
    // Publish the parent-visible zombie event before page-table and fd
    // teardown, robust-list walking, or registry removal. Those operations
    // may wait on remote TLB/futex/VFS state; parent notification must depend
    // only on the atomic Zombie/exit_code publication in `begin_exit`.
    let (parent, orphan_children) = {
        let mut inner = task.inner_exclusive_access();
        let parent = inner.parent.as_ref().and_then(|w| w.upgrade());
        let orphan_children = core::mem::take(&mut inner.children);
        (parent, orphan_children)
    };
    // The parent's waitable-child list is the authority for SIGCHLD/waitpid
    // visibility.  `tgid == pid` is only a derived property and can be stale
    // across clone/exec combinations; CLONE_THREAD entries are already
    // removed from `children`, so this test also excludes non-waitable peers.
    let notify_parent = parent
        .as_ref()
        .map(|parent| {
            parent
                .inner_exclusive_access()
                .children
                .iter()
                .any(|child| child.getpid() == pid)
        })
        .unwrap_or(false);
    if LIFECYCLE_TRACE_ENABLED {
        println!(
            "[lifecycle] exit pid={} tgid={} parent={:?} notify={}",
            pid,
            task.inner_exclusive_access().tgid,
            parent.as_ref().map(|parent| parent.getpid()),
            notify_parent
        );
    }

    // wake parent waiting in waitpid
    if notify_parent {
        if let Some(parent) = parent.as_ref() {
            if parent_auto_reaps_sigchld(&parent) {
                detach_child_from_parent(&parent, pid);
                wake_parent_group_waiters(&parent);
            } else {
                notify_parent_sigchld(&parent);
                wake_parent_group_waiters(&parent);
            }
        }
    }
    #[cfg(feature = "smp-diagnostics")]
    if exit_diag {
        println!("[exit-diag] tid={} parent-notified", pid);
    }
    if LIFECYCLE_TRACE_ENABLED {
        println!("[lifecycle] exit-parent-notified pid={}", pid);
    }

    crate::syscall::process::cleanup_task_exit_futexes(&task);
    #[cfg(feature = "smp-diagnostics")]
    if exit_diag {
        println!("[exit-diag] tid={} futex-clean", pid);
    }
    remove_from_pid2task(task.getpid());
    #[cfg(feature = "smp-diagnostics")]
    if exit_diag {
        println!("[exit-diag] tid={} registry-removed", pid);
    }
    if LIFECYCLE_TRACE_ENABLED {
        println!("[lifecycle] exit-registry-removed pid={}", pid);
    }

    // ++++++ access initproc TCB exclusively
    if !orphan_children.is_empty() {
        for child in orphan_children.iter() {
            child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
        }
        // Never nest TaskInner locks: child TCB addresses have no semantic
        // relationship to INITPROC's address.  Publish each parent pointer,
        // release it, and only then append the already-owned Arc snapshots.
        INITPROC
            .inner_exclusive_access()
            .children
            .extend(orphan_children.iter().cloned());
    }
    #[cfg(feature = "smp-diagnostics")]
    if exit_diag {
        println!("[exit-diag] tid={} orphans-complete", pid);
    }
    // Resource teardown is deferred until this hart has switched to its idle
    // context and boot page table.  The idle reaper publishes cleanup_done and
    // performs the final waitpid wake.
    manager::finish_current_exit(&task);
    if let Some(current) = take_current_task() {
        defer_drop_task(current);
    }
    #[cfg(feature = "smp-diagnostics")]
    if exit_diag {
        println!("[exit-diag] tid={} deferred", pid);
    }
    if LIFECYCLE_TRACE_ENABLED {
        println!("[lifecycle] exit-deferred pid={}", pid);
    }
    drop(task);
    loop {
        let mut unused = KContext::blank();
        schedule(&mut unused as *mut _);
    }
}

lazy_static! {
    pub static ref INITPROC: Arc<TaskControlBlock> =
        Arc::new({ TaskControlBlock::new(EMBEDDED_INITPROC) });
}

#[cfg(target_arch = "riscv64")]
static EMBEDDED_INITPROC: &[u8] =
    include_bytes!("../../../target/riscv64gc-unknown-none-elf/release/initproc");

#[cfg(target_arch = "loongarch64")]
static EMBEDDED_INITPROC: &[u8] =
    include_bytes!("../../../target/loongarch64-unknown-none/release/initproc");

pub fn add_initproc() {
    trace!("os::task::add_initproc");
    add_task(INITPROC.clone());
}

fn default_signal_interrupts(signal: SignalFlags) -> bool {
    signal.intersects(
        SignalFlags::SIGSEGV
            | SignalFlags::SIGABRT
            | SignalFlags::SIGILL
            | SignalFlags::SIGFPE
            | SignalFlags::SIGBUS
            | SignalFlags::SIGSYS
            | SignalFlags::SIGTRAP
            | SignalFlags::SIGALRM
            | SignalFlags::SIGVTALRM
            | SignalFlags::SIGPROF
            | SignalFlags::SIGTERM
            | SignalFlags::SIGINT
            | SignalFlags::SIGQUIT
            | SignalFlags::SIGHUP
            | SignalFlags::SIGPIPE
            | SignalFlags::SIGUSR1
            | SignalFlags::SIGUSR2
            | SignalFlags::SIGXCPU
            | SignalFlags::SIGXFSZ
            | SignalFlags::SIGIO
            | SignalFlags::SIGPWR
            | SignalFlags::SIGKILL
            | SignalFlags::SIGSTOP
            | SignalFlags::SIGDEF,
    )
}

pub fn next_interrupting_signal(inner: &TaskControlBlockInner) -> Option<SignalFlags> {
    let pending = inner.signals & !inner.signal_mask;
    if pending.is_empty() {
        return None;
    }
    for sig in 1..=MAX_SIG {
        let Some(signal) = SignalFlags::from_bits(1u64 << sig) else {
            continue;
        };
        if !pending.contains(signal) {
            continue;
        }
        let action = inner.signal_actions.lock().table[sig];
        if action.handler == SIG_IGN {
            continue;
        }
        if action.handler != 0 || default_signal_interrupts(signal) {
            return Some(signal);
        }
    }
    None
}

pub fn has_interrupting_signal(inner: &TaskControlBlockInner) -> bool {
    inner.killed || next_interrupting_signal(inner).is_some()
}

pub fn current_add_signal(signal: SignalFlags) {
    trace!("os::task::current_add_signal");
    let task = match current_task() {
        Some(t) => t,
        None => return,
    };
    let Some(mut task_inner) = task.try_inner_exclusive_access() else {
        return;
    };
    #[cfg(feature = "smp-diagnostics")]
    if signal == SignalFlags::SIGSEGV {
        let action = task_inner.signal_actions.lock().table[SIGSEGV_NUM];
        println!(
            "[signal-diag] queue tid={} tgid={} sig={} pending={:#x} mask={:#x} handling={} handler={:#x} flags={:#x}",
            task.getpid(),
            task_inner.tgid,
            SIGSEGV_NUM,
            task_inner.signals.bits(),
            task_inner.signal_mask.bits(),
            task_inner.handling_sig,
            action.handler,
            action.flags,
        );
    }
    task_inner.signals |= signal;
    // println!(
    //     "[K] current_add_signal:: current task sigflag {:?}",
    //     task_inner.signals
    // );
}

fn call_kernel_signal_handler(signal: SignalFlags) {
    trace!("os::task::call_kernel_signal_handler");
    let task = current_task().unwrap();
    let mut task_inner = task.inner_exclusive_access();
    match signal {
        SignalFlags::SIGSTOP => {
            task_inner.frozen = true;
            task_inner.signals ^= SignalFlags::SIGSTOP;
        }
        SignalFlags::SIGCONT => {
            if task_inner.signals.contains(SignalFlags::SIGCONT) {
                task_inner.signals ^= SignalFlags::SIGCONT;
                task_inner.frozen = false;
            }
        }
        SignalFlags::SIGKILL => {
            task_inner.killed = true;
            task_inner.signals ^= signal;
        }
        _ => {
            // println!(
            //     "[K] call_kernel_signal_handler:: current task sigflag {:?}",
            //     task_inner.signals
            // );
            task_inner.killed = true;
        }
    }
}

fn call_user_signal_handler(sig: usize, signal: SignalFlags) {
    trace!("os::task::call_user_signal_handler");
    let task = current_task().unwrap();
    let mut task_inner = task.inner_exclusive_access();

    let handler = task_inner.signal_actions.lock().table[sig].handler;
    #[cfg(feature = "smp-diagnostics")]
    if sig == 11 || sig == 14 || sig == 15 || sig == 32 || sig == 33 {
        println!(
            "[signal-diag] deliver-attempt tid={} sig={} pending={:#x} mask={:#x} handling={} handler={:#x}",
            task.getpid(),
            sig,
            task_inner.signals.bits(),
            task_inner.signal_mask.bits(),
            task_inner.handling_sig,
            handler,
        );
    }
    if handler == 1 {
        task_inner.signals ^= signal;
        task_inner.pending_signal_info[sig] = PendingSignalInfo::default();
        return;
    }
    if handler != 0 {
        let action = task_inner.signal_actions.lock().table[sig];
        let old_mask = task_inner.signal_mask;
        let signal_info = task_inner.pending_signal_info[sig];
        // Linux always builds an rt_sigframe, including for a one-argument
        // handler.  Keeping the interrupted state in the user frame makes
        // nested delivery a stack operation instead of overwriting one
        // kernel-side Option.
        let interrupted = task_inner.get_trap_cx().clone();
        // Signal delivery writes the frame from supervisor mode.  If the
        // destination is a demand-mapped stack page, taking the fault while
        // holding TaskLock would recursively enter the page-fault handler and
        // deadlock on the same lock.  Resolve every page in the frame through
        // MemorySet before performing any translated user writes.
        if prepare_signal_frame_pages(&mut task_inner, interrupted[TrapFrameArgs::SP]).is_none() {
            task_inner.signals ^= signal;
            task_inner.killed = true;
            task_inner.exit_signal = -(SIGSEGV_NUM as i32);
            return;
        }
        // handle flag
        task_inner.signals ^= signal;
        task_inner.pending_signal_info[sig] = PendingSignalInfo::default();
        task_inner.handling_sig = sig as isize;
        task_inner.signal_mask_backup = Some(old_mask);
        task_inner.signal_mask |= action.mask | signal;

        if SIGNAL_TRACE_ENABLED && signal == SignalFlags::SIGSEGV {
            let n = SIGNAL_TRACE_COUNT.fetch_add(1, AtomicOrdering::Relaxed);
            if n < 12 {
                println!(
                    "[signal-trace] deliver pid={} sig={} pc={:#x} sp={:#x} handler={:#x}",
                    task.getpid(),
                    sig,
                    interrupted[TrapFrameArgs::SEPC],
                    interrupted[TrapFrameArgs::SP],
                    handler
                );
            }
        }
        let token = task_inner.get_user_token();
        let frame_base = write_signal_frame(
            token,
            interrupted[TrapFrameArgs::SP],
            sig,
            signal_info,
            old_mask,
            &interrupted,
        );
        let siginfo_addr = frame_base;
        let ucontext_addr = frame_base + SIGINFO_SIZE;
        let trap_ctx = task_inner.get_trap_cx();
        trap_ctx[TrapFrameArgs::SP] = frame_base;
        if action.flags & SA_SIGINFO != 0 {
            trap_ctx[TrapFrameArgs::ARG1] = siginfo_addr;
            trap_ctx[TrapFrameArgs::ARG2] = ucontext_addr;
        }
        // modify trapframe
        trap_ctx[TrapFrameArgs::SEPC] = handler;
        trap_ctx[TrapFrameArgs::RA] =
            if action.flags & SA_RESTORER != 0 && action.restorer != 0 {
                action.restorer
            } else {
                SIGNAL_RETURN_TRAMPOLINE
            };

        // put args (a0)
        trap_ctx[TrapFrameArgs::ARG0] = sig;
    } else {
        // default action: fatal signals kill the process, others are ignored
        let fatal = signal.intersects(
            SignalFlags::SIGSEGV
                | SignalFlags::SIGABRT
                | SignalFlags::SIGILL
                | SignalFlags::SIGFPE
                | SignalFlags::SIGBUS
                | SignalFlags::SIGSYS
                | SignalFlags::SIGTRAP
                | SignalFlags::SIGALRM
                | SignalFlags::SIGVTALRM
                | SignalFlags::SIGPROF
                | SignalFlags::SIGTERM
                | SignalFlags::SIGINT
                | SignalFlags::SIGQUIT
                | SignalFlags::SIGHUP
                | SignalFlags::SIGPIPE
                | SignalFlags::SIGUSR1
                | SignalFlags::SIGUSR2
                | SignalFlags::SIGXCPU
                | SignalFlags::SIGXFSZ
                | SignalFlags::SIGIO
                | SignalFlags::SIGPWR,
        );
        if fatal {
            task_inner.killed = true;
            task_inner.exit_signal = -(sig as i32);
        }
        task_inner.signals ^= signal;
        task_inner.pending_signal_info[sig] = PendingSignalInfo::default();
    }
}

fn prepare_signal_frame_pages(
    task_inner: &mut TaskControlBlockInner,
    sp: usize,
) -> Option<usize> {
    let frame_size = SIGINFO_SIZE + UCONTEXT_SIZE;
    let frame_base = sp.checked_sub(frame_size)? & !15;
    let frame_end = frame_base.checked_add(frame_size)?;
    let mut page = frame_base & !(crate::config::PAGE_SIZE - 1);
    while page < frame_end {
        task_inner.memory_set.handle_page_fault(
            polyhal::VirtAddr::from(page),
            crate::mm::PageFaultAccess::Write,
        )?;
        page = page.checked_add(crate::config::PAGE_SIZE)?;
    }
    Some(frame_base)
}

fn flags_to_user_sigset(flags: SignalFlags) -> u64 {
    let mut raw = 0u64;
    for sig in 1..=MAX_SIG {
        if let Some(flag) = SignalFlags::from_bits(1u64 << sig) {
            if flags.contains(flag) {
                raw |= 1u64 << (sig - 1);
            }
        }
    }
    raw
}

fn user_sigset_to_flags(raw: u64) -> SignalFlags {
    let mut flags = SignalFlags::empty();
    for sig in 1..=MAX_SIG {
        if raw & (1u64 << (sig - 1)) != 0 {
            if let Some(flag) = SignalFlags::from_bits(1u64 << sig) {
                flags.insert(flag);
            }
        }
    }
    flags
}

fn write_signal_frame(
    token: polyhal::pagetable::PageTable,
    sp: usize,
    sig: usize,
    info: PendingSignalInfo,
    old_mask: SignalFlags,
    interrupted: &TrapFrame,
) -> usize {
    let frame_size = SIGINFO_SIZE + UCONTEXT_SIZE;
    let frame_base = sp.saturating_sub(frame_size) & !15;
    for i in 0..frame_size {
        *translated_refmut(token, (frame_base + i) as *mut u8) = 0;
    }
    *translated_refmut(token, frame_base as *mut i32) = sig as i32;
    // Linux generic 64-bit siginfo_t header followed by the _kill payload.
    *translated_refmut(token, (frame_base + 8) as *mut i32) = info.code;
    *translated_refmut(token, (frame_base + 16) as *mut i32) = info.sender_pid;
    *translated_refmut(token, (frame_base + 20) as *mut u32) = info.sender_uid;
    let ucontext_addr = frame_base + SIGINFO_SIZE;
    *translated_refmut(token, (ucontext_addr + UCONTEXT_SIGMASK_OFFSET) as *mut u64) =
        flags_to_user_sigset(old_mask);
    *translated_refmut(token, (ucontext_addr + UCONTEXT_PC_OFFSET) as *mut usize) =
        interrupted[TrapFrameArgs::SEPC];
    for reg in 1..32 {
        *translated_refmut(
            token,
            (ucontext_addr + UCONTEXT_MCONTEXT_OFFSET + reg * size_of::<usize>())
                as *mut usize,
        ) = trap_gpr(interrupted, reg);
    }
    frame_base
}

pub fn restore_current_signal_context() -> Option<isize> {
    let task = current_task()?;
    let mut inner = task.inner_exclusive_access();
    let frame_base = inner.get_trap_cx()[TrapFrameArgs::SP];
    let ucontext_addr = frame_base.checked_add(SIGINFO_SIZE)?;
    let token = inner.get_user_token();
    let raw_mask = *translated_ref(
        token,
        (ucontext_addr + UCONTEXT_SIGMASK_OFFSET) as *const u64,
    );
    let pc = *translated_ref(
        token,
        (ucontext_addr + UCONTEXT_PC_OFFSET) as *const usize,
    );
    if pc == 0 {
        return None;
    }
    let mut restored = inner.get_trap_cx().clone();
    restored[TrapFrameArgs::SEPC] = pc;
    for reg in 1..32 {
        let value = *translated_ref(
            token,
            (ucontext_addr + UCONTEXT_MCONTEXT_OFFSET + reg * size_of::<usize>())
                as *const usize,
        );
        set_trap_gpr(&mut restored, reg, value);
    }
    let ret = restored[TrapFrameArgs::RET] as isize;
    if SIGNAL_TRACE_ENABLED {
        let n = SIGNAL_TRACE_COUNT.load(AtomicOrdering::Relaxed);
        if n > 0 && n <= 12 {
            println!(
                "[signal-trace] return pid={} pc={:#x} sp={:#x}",
                task.getpid(),
                restored[TrapFrameArgs::SEPC],
                restored[TrapFrameArgs::SP]
            );
        }
    }
    inner.signal_mask = user_sigset_to_flags(raw_mask);
    inner.handling_sig = -1;
    inner.signal_mask_backup = None;
    inner.signal_ucontext = None;
    inner.trap_ctx_backup = None;
    let trap_ctx = inner.get_trap_cx();
    *trap_ctx = restored;
    Some(ret)
}

enum PendingSignalAction {
    State { killed: bool, frozen: bool },
    Kernel(SignalFlags),
    User(usize, SignalFlags),
}

fn check_pending_signals() -> Option<(Arc<TaskControlBlock>, bool, bool)> {
    trace!("os::task::check_pending_signals");
    let task = current_task()?;
    loop {
        let action = {
            let mut task_inner = task.try_inner_exclusive_access()?;
            let mut deliverable = task_inner.signals & !task_inner.signal_mask;
            if let Some(signal) = task_inner.sigsuspend_delivery {
                if task_inner.signals.contains(signal) {
                    deliverable.insert(signal);
                }
            }
            if task_inner.handling_sig != -1 {
                let hs = task_inner.handling_sig as usize;
                deliverable &= !task_inner.signal_actions.lock().table[hs].mask;
            }
            let sig = deliverable.bits().trailing_zeros() as usize;
            if sig > MAX_SIG {
                PendingSignalAction::State {
                    killed: task_inner.killed,
                    frozen: task_inner.frozen,
                }
            } else {
                let signal = SignalFlags::from_bits(1u64 << sig).unwrap();
                if task_inner.sigsuspend_delivery == Some(signal) {
                    task_inner.sigsuspend_delivery = None;
                }
                #[cfg(feature = "smp-diagnostics")]
                if sig == 14 || sig == 15 {
                    let signal_action = task_inner.signal_actions.lock().table[sig];
                    println!(
                        "[signal-diag] pending tid={} sig={} status={:?} mask={:#x} handling={} handler={:#x}",
                        task.getpid(),
                        sig,
                        task_inner.task_status,
                        task_inner.signal_mask.bits(),
                        task_inner.handling_sig,
                        signal_action.handler,
                    );
                }
                if signal == SignalFlags::SIGKILL
                    || signal == SignalFlags::SIGSTOP
                    || signal == SignalFlags::SIGCONT
                    || signal == SignalFlags::SIGDEF
                {
                    PendingSignalAction::Kernel(signal)
                } else {
                    PendingSignalAction::User(sig, signal)
                }
            }
        };
        match action {
            PendingSignalAction::State { killed, frozen } => {
                return Some((task, killed, frozen));
            }
            PendingSignalAction::Kernel(signal) => call_kernel_signal_handler(signal),
            PendingSignalAction::User(sig, signal) => {
                call_user_signal_handler(sig, signal);
                let task_inner = task.try_inner_borrow()?;
                let state = (task_inner.killed, task_inner.frozen);
                drop(task_inner);
                return Some((task, state.0, state.1));
            }
        }
    }
}

pub fn handle_signals() {
    trace!("os::task::handle_signals");
    loop {
        let Some((task, killed, frozen)) = check_pending_signals() else {
            break;
        };
        if killed {
            let (exit_code, terminating_for_exec, terminating_for_group) = {
                let inner = task.inner_exclusive_access();
                let sig = inner.exit_signal;
                (
                    if inner.terminating_for_group {
                        inner.exit_code
                    } else if inner.terminating_for_exec {
                        0
                    } else if sig != 0 {
                        sig
                    } else {
                        -9
                    },
                    inner.terminating_for_exec,
                    inner.terminating_for_group,
                )
            };
            if !terminating_for_exec && !terminating_for_group {
                force_exit_task_group(&task, exit_code);
            }
            exit_current_and_run_next(exit_code);
        }
        if !frozen {
            break;
        }
        crate::fs::stdio::stdout_release_if_owner();
        suspend_current_and_run_next_reason(SuspendReason::Frozen);
    }
}

pub fn add_sleep_task(wakeup_time: usize, task: Arc<TaskControlBlock>) {
    manager::enqueue_sleep(wakeup_time, task);
}

pub fn check_and_wake_up_tasks() {
    let now = polyhal::time::Time::now().to_nsec() as usize;
    // Take due entries before inspecting a TCB or putting a task back on the
    // ready queue.  This is the required two-phase wakeup pattern.
    for sleep_task in manager::take_due_sleep_tasks(now) {
        let _ = manager::wake_task(sleep_task.task);
    }
}
