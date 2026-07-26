mod action;
mod fp;
mod manager;
mod pid;
mod processor;
mod signal;
#[allow(clippy::module_inception)]
mod task;

use crate::mm::{translated_ref, translated_refmut};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;
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
    take_waitpid, task_snapshot, update_task_affinity, wake_pipe_waiters, wake_task, BlockQueue,
};
pub use pid::{pid_alloc, PidHandle};
pub use processor::{
    current_hart_id, current_task, current_user_token, flush_remote_tlb, handle_ipi,
    init_current_cpu, init_kernel_page, record_timer_tick, request_reschedule, run_tasks,
    schedule, scheduler_cpu_snapshots, take_current_task, try_current_hart_id,
};
pub use signal::{PendingSignalInfo, SignalFlags, MAX_SIG};
pub use task::{
    blank_kcontext, BlockReason, KernelStack, TaskControlBlock, TaskControlBlockInner, TaskStatus,
};

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
        manager::wake_task(parent);
    }
}

pub(super) fn reap_exited_tasks() {
    for task in manager::take_exited_tasks() {
        processor::set_scheduler_phase(40);
        let (parent, files_to_close) = {
            let mut inner = task.inner_exclusive_access();
            processor::set_scheduler_phase(41);
            let parent = inner.parent.as_ref().and_then(|weak| weak.upgrade());
            inner.memory_set.recycle_data_pages_inactive();
            processor::set_scheduler_phase(42);
            let files = inner.drain_fd_table();
            (parent, files)
        };
        processor::set_scheduler_phase(43);
        for file in files_to_close {
            file.close_ref();
        }
        processor::set_scheduler_phase(44);
        task.publish_exit_cleanup_done();
        if let Some(parent) = parent {
            wake_parent_waiter(&parent);
        }
    }
}

pub fn reap_exited_task(pid: usize) {
    manager::reap_exited_task(pid);
}

pub fn suspend_current_and_run_next() {
    //trace!("os::task::suspend_current_and_run_next");
    // There must be an application running.
    let task = current_task().unwrap();

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
    let tasks: Vec<_> = manager::net_blocked_snapshot()
        .into_iter()
        .filter(|task| {
            task.try_inner_borrow()
                .map(|inner| matches!(inner.block_reason, Some(BlockReason::PollWait)))
                .unwrap_or(false)
        })
        .collect();
    for task in tasks {
        manager::wake_task(task);
    }
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
    let _ = crate::syscall::process::remove_futex_waiter(pid);
    let _ = manager::wake_task(task.clone());
}

pub fn check_itimers() {
    let now = polyhal::time::Time::now().to_usec() as u64;
    let tasks = task_snapshot();
    for task in tasks {
        let expired = {
            let Some(mut inner) = task.try_inner_exclusive_access() else {
                continue;
            };
            if inner.task_status == TaskStatus::Zombie
                || inner.itimer_expire == 0
                || now < inner.itimer_expire
            {
                false
            } else {
                inner.signals.insert(SignalFlags::SIGALRM);
                if inner.itimer_interval_us > 0 {
                    inner.itimer_expire = now + inner.itimer_interval_us;
                } else {
                    inner.itimer_expire = 0;
                }
                true
            }
        };
        if expired {
            wake_blocked_task(&task);
        }
    }
}

fn notify_parent_sigchld(parent: &Arc<TaskControlBlock>) {
    {
        let mut inner = parent.inner_exclusive_access();
        inner.signals.insert(SignalFlags::SIGCHLD);
    }
    let woke_signal_waiter = wake_signal_wait_task(parent, SignalFlags::SIGCHLD);
    let should_interrupt = parent
        .try_inner_borrow()
        .map(|inner| has_interrupting_signal(&inner))
        .unwrap_or(false);
    if should_interrupt && !woke_signal_waiter {
        wake_blocked_task(parent);
    }
}

fn wake_parent_waiter(parent: &Arc<TaskControlBlock>) {
    let before = parent
        .try_inner_borrow()
        .map(|inner| inner.task_status);
    let woke = manager::wake_task(parent.clone());
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
                wake_parent_waiter(&parent);
            } else {
                notify_parent_sigchld(&parent);
                wake_parent_waiter(&parent);
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

pub fn force_exit_task_group(task: &Arc<TaskControlBlock>, exit_code: i32) {
    let (pid, tgid) = {
        let inner = task.inner_exclusive_access();
        (task.getpid(), inner.tgid)
    };
    let group_tasks: Vec<Arc<TaskControlBlock>> = task_snapshot()
        .into_iter()
        .filter(|other| {
            other.getpid() != pid
                && other
                    .try_inner_borrow()
                    .map(|inner| inner.tgid == tgid)
                    .unwrap_or(false)
        })
        .collect();
    for other in group_tasks {
        force_exit_task(&other, exit_code);
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

#[cfg(target_arch = "riscv64")]
static STALL_SNAPSHOT_PRINTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[cfg(target_arch = "riscv64")]
static STALL_SNAPSHOT_TICKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_arch = "riscv64")]
const SCHED_STALL_DIAGNOSTICS: bool = cfg!(feature = "smp-diagnostics");
// Lifecycle state remains available through the diagnostic snapshot.  Per
// exit logging is disabled during acceptance so independent harts cannot
// splice text into official test markers.
const LIFECYCLE_TRACE_ENABLED: bool = false;

/// Emit one read-only scheduler snapshot when all runnable work has vanished
/// behind at least two wait registrations.  This is a low-frequency kernel
/// diagnostic only: it neither wakes tasks nor changes benchmark timing.
#[cfg(target_arch = "riscv64")]
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
                "[sched-stall] tid={} tgid={} tcb={:#x} owner={} queued={} state={:?} reason={:?} children={}",
                task.getpid(),
                inner.tgid,
                Arc::as_ptr(&task) as usize,
                task.running_cpu(),
                task.is_enqueued(),
                inner.task_status,
                inner.block_reason,
                inner.children.len()
            );
        }
    }
    #[cfg(feature = "smp-diagnostics")]
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
    if LIFECYCLE_TRACE_ENABLED {
        println!("[lifecycle] exit-zombie pid={}", pid);
    }
    // Publish the parent-visible zombie event before page-table and fd
    // teardown, robust-list walking, or registry removal.  Those operations
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
                wake_parent_waiter(&parent);
            } else {
                notify_parent_sigchld(&parent);
                wake_parent_waiter(&parent);
            }
        }
    }
    if LIFECYCLE_TRACE_ENABLED {
        println!("[lifecycle] exit-parent-notified pid={}", pid);
    }

    crate::syscall::process::cleanup_task_exit_futexes(&task);
    remove_from_pid2task(task.getpid());
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
    // Resource teardown is deferred until this hart has switched to its idle
    // context and boot page table.  The idle reaper publishes cleanup_done and
    // performs the final waitpid wake.
    if let Some(current) = take_current_task() {
        defer_drop_task(current);
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

pub fn check_signals_error_of_current() -> Option<(i32, &'static str)> {
    trace!("os::task::check_signals_error_of_current");
    let task = current_task()?;
    let task_inner = task.try_inner_exclusive_access()?;
    // Only an unmasked signal with the default disposition is a fatal
    // fallback here.  Looking at the raw pending bitmap killed processes
    // which had a valid user SIGSEGV handler whenever delivery was deferred
    // for one trap boundary (notably the repeated `lat_sig prot` fault).
    let pending = task_inner.signals & !task_inner.signal_mask;
    for sig in 1..=MAX_SIG {
        let Some(signal) = SignalFlags::from_bits(1u64 << sig) else {
            continue;
        };
        if pending.contains(signal)
            && task_inner.signal_actions.lock().table[sig].handler == 0
        {
            if let Some(error) = signal.check_error() {
                return Some(error);
            }
        }
    }
    None
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

pub fn has_interrupting_signal(inner: &TaskControlBlockInner) -> bool {
    if inner.killed {
        return true;
    }
    let pending = inner.signals & !inner.signal_mask;
    if pending.is_empty() {
        return false;
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
            return true;
        }
    }
    false
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
        SignalFlags::SIGTERM | SignalFlags::SIGKILL => {
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
    if sig == 32 || sig == 33 {
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
                    interrupted.sepc,
                    interrupted.x[2],
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
        interrupted.sepc;
    for reg in 1..32 {
        *translated_refmut(
            token,
            (ucontext_addr + UCONTEXT_MCONTEXT_OFFSET + reg * size_of::<usize>())
                as *mut usize,
        ) = interrupted.x[reg];
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
    restored.sepc = pc;
    for reg in 1..32 {
        restored.x[reg] = *translated_ref(
            token,
            (ucontext_addr + UCONTEXT_MCONTEXT_OFFSET + reg * size_of::<usize>())
                as *const usize,
        );
    }
    let ret = restored.x[10] as isize;
    if SIGNAL_TRACE_ENABLED {
        let n = SIGNAL_TRACE_COUNT.load(AtomicOrdering::Relaxed);
        if n > 0 && n <= 12 {
            println!(
                "[signal-trace] return pid={} pc={:#x} sp={:#x}",
                task.getpid(),
                restored.sepc,
                restored.x[2]
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

fn check_pending_signals() {
    trace!("os::task::check_pending_signals");
    let task = match current_task() {
        Some(t) => t,
        None => return,
    };
    for sig in 0..(MAX_SIG + 1) {
        let signal = SignalFlags::from_bits(1u64 << sig).unwrap();
        let (masked, kernel_signal) = {
            let Some(mut task_inner) = task.try_inner_exclusive_access() else {
                return;
            };
            if !task_inner.signals.contains(signal) || task_inner.signal_mask.contains(signal) {
                continue;
            }
            let mut m = true;
            if task_inner.handling_sig == -1 {
                m = false;
            } else {
                let hs = task_inner.handling_sig as usize;
                if !task_inner.signal_actions.lock().table[hs]
                    .mask
                    .contains(signal)
                {
                    m = false;
                }
            }
            if m {
                continue;
            }
            let is_kernel = signal == SignalFlags::SIGKILL
                || signal == SignalFlags::SIGTERM
                || signal == SignalFlags::SIGSTOP
                || signal == SignalFlags::SIGCONT
                || signal == SignalFlags::SIGDEF;
            (false, is_kernel)
        };
        if kernel_signal {
            call_kernel_signal_handler(signal);
        } else {
            call_user_signal_handler(sig, signal);
            return;
        }
    }
}

pub fn handle_signals() {
    trace!("os::task::handle_signals");
    loop {
        check_pending_signals();
        let killed = {
            let task = match current_task() {
                Some(t) => t,
                None => break,
            };
            let Some(task_inner) = task.try_inner_borrow() else {
                break;
            };
            task_inner.killed
        };
        if killed {
            let task = current_task().unwrap();
            let exit_code = {
                let inner = task.inner_exclusive_access();
                let sig = inner.exit_signal;
                if sig != 0 {
                    sig
                } else {
                    -9
                }
            };
            force_exit_task_group(&task, exit_code);
            exit_current_and_run_next(exit_code);
        }
        let frozen = {
            let task = match current_task() {
                Some(t) => t,
                None => break,
            };
            let Some(task_inner) = task.try_inner_borrow() else {
                break;
            };
            task_inner.frozen
        };
        if !frozen {
            break;
        }
        crate::fs::stdio::stdout_release_if_owner();
        suspend_current_and_run_next();
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
