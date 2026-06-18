mod action;
mod fp;
mod manager;
mod pid;
mod processor;
mod signal;
#[allow(clippy::module_inception)]
mod task;

use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;
use log::*;
use manager::fetch_task;
use manager::remove_from_pid2task;
use manager::remove_from_ready_queue;
use polyhal::instruction::shutdown;
use polyhal::kcontext::KContext;
use polyhal_trap::trapframe::TrapFrameArgs;

pub const SIGNAL_RETURN_TRAMPOLINE: usize = 0x7ffeff000;

pub use action::{SignalAction, SignalActions};
pub use fp::UserFloatState;
pub use manager::{add_priority_task, add_task, pid2task, ready_queue_len, PID2TCB};
pub use pid::{pid_alloc, PidHandle};
pub use processor::{
    current_task, current_user_token, init_kernel_page, run_tasks, schedule, take_current_task,
};
pub use signal::{SignalFlags, MAX_SIG};
pub use task::{
    blank_kcontext, BlockReason, KernelStack, TaskControlBlock, TaskControlBlockInner, TaskStatus,
};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TimeSpec {
    pub tv_sec: usize,
    pub tv_nsec: usize,
}

pub struct SleepTask {
    pub wakeup_time: usize,
    pub task: Arc<TaskControlBlock>,
}

lazy_static! {
    // public sleep queue for timer interrupt handler to access
    pub static ref SLEEP_QUEUE: UPSafeCell<Vec<SleepTask>> = unsafe {
        UPSafeCell::new_named(Vec::new(), "SleepQueue")
    };
    pub static ref NET_BLOCK_QUEUE: UPSafeCell<Vec<Arc<TaskControlBlock>>> = unsafe {
        UPSafeCell::new_named(Vec::new(), "NetBlockQueue")
    };
pub static ref WAITPID_QUEUE: UPSafeCell<Vec<Arc<TaskControlBlock>>> = unsafe {
        UPSafeCell::new_named(Vec::new(), "WaitpidQueue")
    };
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
    // Change status to Ready
    task_inner.task_status = TaskStatus::Ready;
    drop(task_inner);
    // ---- release current PCB

    // push back to ready queue BEFORE clearing processor
    add_task(task.clone());
    // now safe to clear processor (window from here to schedule is minimal)
    take_current_task();
    // jump to scheduling cycle
    schedule(task_cx_ptr);
    //added fot reset time accounting
    task.record_time_resume();
}

pub fn sleep_current_and_run_next() {
    crate::fs::stdio::stdout_release_if_owner();
    let task = current_task().unwrap();
    task.record_time_suspend();
    let mut task_inner = task.inner_exclusive_access();
    task_inner.task_status = TaskStatus::Blocked;
    let task_cx_ptr = &mut task_inner.task_cx as *mut KContext;
    drop(task_inner);
    take_current_task();
    schedule(task_cx_ptr);
    task.record_time_resume();
}

pub fn block_current_and_run_next(block_reason: BlockReason) {
    static mut C: u64 = 0;
    crate::fs::stdio::stdout_release_if_owner();
    let task = current_task().unwrap();
    let pid = task.getpid();
    task.record_time_suspend();
    let mut task_inner = task.inner_exclusive_access();
    task_inner.block_reason = Some(block_reason);
    task_inner.task_status = TaskStatus::Blocked;
    let task_cx_ptr = &mut task_inner.task_cx as *mut KContext;
    drop(task_inner);
    take_current_task();
    let mut queue = NET_BLOCK_QUEUE.exclusive_access();
    queue.retain(|queued| queued.getpid() != pid);
    queue.push(task.clone());
    drop(queue);
    schedule(task_cx_ptr);
    task.record_time_resume();
}

pub fn wake_poll_wait_tasks() {
    let mut to_wake = Vec::new();
    {
        let mut queue = NET_BLOCK_QUEUE.exclusive_access();
        let mut i = 0;
        while i < queue.len() {
            let should_wake = {
                let inner = queue[i].inner_exclusive_access();
                matches!(inner.block_reason, Some(BlockReason::PollWait))
            };
            if should_wake {
                to_wake.push(queue.remove(i));
            } else {
                i += 1;
            }
        }
    }
    for task in to_wake {
        let mut inner = task.inner_exclusive_access();
        inner.task_status = TaskStatus::Ready;
        inner.block_reason = None;
        drop(inner);
        add_priority_task(task);
    }
}

pub fn wake_pipe_read_tasks(pipe_id: usize) {
    wake_pipe_tasks(pipe_id, true);
}

pub fn wake_pipe_write_tasks(pipe_id: usize) {
    wake_pipe_tasks(pipe_id, false);
}

fn wake_pipe_tasks(pipe_id: usize, read_waiters: bool) {
    let mut to_wake = Vec::new();
    {
        let mut queue = NET_BLOCK_QUEUE.exclusive_access();
        let mut i = 0;
        while i < queue.len() {
            let should_wake = {
                let inner = queue[i].inner_exclusive_access();
                match inner.block_reason {
                    Some(BlockReason::PipeRead(id)) => read_waiters && id == pipe_id,
                    Some(BlockReason::PipeWrite(id)) => !read_waiters && id == pipe_id,
                    _ => false,
                }
            };
            if should_wake {
                to_wake.push(queue.remove(i));
            } else {
                i += 1;
            }
        }
    }
    for task in to_wake {
        let mut inner = task.inner_exclusive_access();
        inner.task_status = TaskStatus::Ready;
        inner.block_reason = None;
        drop(inner);
        add_priority_task(task);
    }
}

pub fn wake_blocked_task(task: &Arc<TaskControlBlock>) {
    let pid = task.getpid();
    let removed = remove_task_from_block_queues(pid);
    let mut inner = task.inner_exclusive_access();
    let should_wake = inner.task_status == TaskStatus::Blocked || removed;
    if should_wake && inner.task_status != TaskStatus::Zombie {
        inner.task_status = TaskStatus::Ready;
        inner.block_reason = None;
        drop(inner);
        add_priority_task(task.clone());
    }
}

fn notify_parent_sigchld(parent: &Arc<TaskControlBlock>) {
    let should_wake = {
        let mut inner = parent.inner_exclusive_access();
        if inner.signal_mask.contains(SignalFlags::SIGCHLD) {
            inner.signals.insert(SignalFlags::SIGCHLD);
            true
        } else {
            false
        }
    };
    if should_wake {
        wake_blocked_task(parent);
    }
}

fn remove_task_from_block_queues(pid: usize) -> bool {
    let mut removed = false;
    {
        let mut queue = NET_BLOCK_QUEUE.exclusive_access();
        let old_len = queue.len();
        queue.retain(|t| t.getpid() != pid);
        if queue.len() != old_len {
            removed = true;
        }
    }
    {
        let mut queue = WAITPID_QUEUE.exclusive_access();
        let old_len = queue.len();
        queue.retain(|t| t.getpid() != pid);
        if queue.len() != old_len {
            removed = true;
        }
    }
    {
        let mut queue = SLEEP_QUEUE.exclusive_access();
        let old_len = queue.len();
        queue.retain(|sleep_task| sleep_task.task.getpid() != pid);
        if queue.len() != old_len {
            removed = true;
        }
    }
    removed
}

pub fn force_exit_task(task: &Arc<TaskControlBlock>, exit_code: i32) {
    let pid = task.getpid();
    remove_from_ready_queue(pid);
    remove_task_from_block_queues(pid);
    remove_from_pid2task(pid);

    let (parent, orphan_children) = {
        let mut inner = task.inner_exclusive_access();
        if inner.task_status == TaskStatus::Zombie {
            return;
        }
        inner.task_status = TaskStatus::Zombie;
        inner.exit_code = exit_code;
        inner.block_reason = None;
        let parent = inner.parent.as_ref().and_then(|w| w.upgrade());
        let orphan_children = core::mem::take(&mut inner.children);
        inner.memory_set.recycle_data_pages();
        inner.clear_fd_table();
        (parent, orphan_children)
    };

    if let Some(parent) = parent {
        notify_parent_sigchld(&parent);
        let mut queue = WAITPID_QUEUE.exclusive_access();
        if let Some(idx) = queue.iter().position(|t| t.getpid() == parent.getpid()) {
            let parent_task = queue.remove(idx);
            drop(queue);
            let mut pinner = parent_task.inner_exclusive_access();
            pinner.task_status = TaskStatus::Ready;
            drop(pinner);
            add_task(parent_task);
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
    let group_tasks: Vec<Arc<TaskControlBlock>> = {
        let map = PID2TCB.exclusive_access();
        map.values()
            .filter(|other| {
                other.getpid() != pid
                    && other
                        .try_inner_borrow()
                        .map(|inner| inner.tgid == tgid)
                        .unwrap_or(false)
            })
            .cloned()
            .collect()
    };
    for other in group_tasks {
        force_exit_task(&other, exit_code);
    }
}

pub fn dump_task_stats_for_oom(context: &str) {
    let (pid_count, pid_zombies) = manager::pid2task_counts().unwrap_or((usize::MAX, usize::MAX));
    let (ready_count, ready_zombies) =
        manager::ready_queue_counts().unwrap_or((usize::MAX, usize::MAX));
    let net_blocked = NET_BLOCK_QUEUE
        .try_borrow()
        .map(|queue| queue.len())
        .unwrap_or(usize::MAX);
    let waitpid_blocked = WAITPID_QUEUE
        .try_borrow()
        .map(|queue| queue.len())
        .unwrap_or(usize::MAX);
    let sleeping = SLEEP_QUEUE
        .try_borrow()
        .map(|queue| queue.len())
        .unwrap_or(usize::MAX);
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

pub const IDLE_PID: usize = 0;

/// Exit the current 'Running' task and run the next task in task list.
pub fn exit_current_and_run_next(exit_code: i32) -> ! {
    crate::fs::stdio::stdout_release_if_owner();
    trace!("os::task::exit_current_and_run_next");
    // take from Processor
    let task = current_task().unwrap();

    let pid = task.getpid();
    if pid == IDLE_PID {
        println!(
            "[kernel] Idle process exit with exit_code {} ...",
            exit_code
        );
        shutdown();
    }

    // remove from pid2task
    remove_from_ready_queue(task.getpid());
    remove_from_pid2task(task.getpid());
    let (parent, orphan_children) = {
        // **** access current TCB exclusively
        let mut inner = task.inner_exclusive_access();
        // Change status to Zombie
        inner.task_status = TaskStatus::Zombie;
        // Record exit code
        inner.exit_code = exit_code;
        let parent = inner.parent.as_ref().and_then(|w| w.upgrade());
        let orphan_children = core::mem::take(&mut inner.children);
        // deallocate user space
        inner.memory_set.recycle_data_pages();
        // drop file descriptors
        inner.clear_fd_table();
        (parent, orphan_children)
    };
    // **** release current PCB before touching parent/initproc

    // wake parent waiting in waitpid
    if let Some(parent) = parent {
        notify_parent_sigchld(&parent);
        let mut queue = WAITPID_QUEUE.exclusive_access();
        if let Some(idx) = queue.iter().position(|t| t.getpid() == parent.getpid()) {
            let parent_task = queue.remove(idx);
            drop(queue);
            let mut pinner = parent_task.inner_exclusive_access();
            pinner.task_status = TaskStatus::Ready;
            drop(pinner);
            add_task(parent_task);
        }
    }

    // ++++++ access initproc TCB exclusively
    if !orphan_children.is_empty() {
        let mut initproc_inner = INITPROC.inner_exclusive_access();
        for child in orphan_children.iter() {
            child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
            initproc_inner.children.push(child.clone());
        }
    }
    // ++++++ release parent PCB
    // drop task manually to maintain rc correctly
    drop(task);
    // we do not have to save task context
    take_current_task();
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
    task_inner.signals.check_error()
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

    let handler = task_inner.signal_actions.table[sig].handler;
    if handler == 1 {
        task_inner.signals ^= signal;
        return;
    }
    if handler != 0 {
        let action = task_inner.signal_actions.table[sig];

        // handle flag
        let old_mask = task_inner.signal_mask;
        task_inner.signals ^= signal;
        task_inner.handling_sig = sig as isize;
        task_inner.signal_mask_backup = Some(old_mask);
        task_inner.signal_mask |= action.mask | signal;

        // backup trapframe
        let trap_ctx = task_inner.get_trap_cx();
        task_inner.trap_ctx_backup = Some(trap_ctx.clone());
        // modify trapframe
        trap_ctx[TrapFrameArgs::SEPC] = handler;
        trap_ctx[TrapFrameArgs::RA] = if action.restorer != 0 {
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
    }
}

pub fn restore_current_signal_context() -> Option<isize> {
    let task = current_task()?;
    let mut inner = task.inner_exclusive_access();
    inner.handling_sig = -1;
    if let Some(mask) = inner.signal_mask_backup.take() {
        inner.signal_mask = mask;
    }
    let backup = inner.trap_ctx_backup.take()?;
    let ret = backup[TrapFrameArgs::RET] as isize;
    let trap_ctx = inner.get_trap_cx();
    *trap_ctx = backup;
    Some(ret)
}

fn check_pending_signals() {
    trace!("os::task::check_pending_signals");
    let task = match current_task() {
        Some(t) => t,
        None => return,
    };
    for sig in 0..(MAX_SIG + 1) {
        let signal = SignalFlags::from_bits(1 << sig).unwrap();
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
                if !task_inner.signal_actions.table[hs].mask.contains(signal) {
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
    SLEEP_QUEUE
        .exclusive_access()
        .push(SleepTask { wakeup_time, task });
}

pub fn check_and_wake_up_tasks() {
    let now = polyhal::time::Time::now().to_nsec() as usize;
    let mut queue = SLEEP_QUEUE.exclusive_access();

    let mut i = 0;
    while i < queue.len() {
        if queue[i].wakeup_time <= now {
            let sleep_task = queue.remove(i);

            //wake up the task by changing its status to Ready and adding it back to the scheduler
            let mut inner = sleep_task.task.inner_exclusive_access();
            inner.task_status = TaskStatus::Ready;
            drop(inner);
            add_task(sleep_task.task);
        } else {
            i += 1;
        }
    }
}
