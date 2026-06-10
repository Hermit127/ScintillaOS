mod action;
mod manager;
mod pid;
mod processor;
mod signal;
#[allow(clippy::module_inception)]
mod task;

use crate::fs::{open_file, OpenFlags};
use crate::mm::translated_refmut;
use crate::mm::translated_byte_buffer;
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;
use log::*;
use manager::fetch_task;
use manager::remove_from_pid2task;
use polyhal::instruction::shutdown;
use polyhal::kcontext::KContext;
use polyhal_trap::trapframe::TrapFrameArgs;

pub use action::{SignalAction, SignalActions};
pub use manager::{add_priority_task, add_task, pid2task, ready_queue_len, PID2TCB};
pub use pid::{pid_alloc, PidHandle};
pub use processor::{
    current_task, current_user_token, init_kernel_page, run_tasks, schedule, take_current_task,
};
pub use signal::{SignalFlags, MAX_SIG};
pub use task::{BlockReason, KernelStack, TaskControlBlock, TaskControlBlockInner, TaskStatus, blank_kcontext};

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
    pub static ref FUTEX_QUEUE: UPSafeCell<Vec<(usize, Arc<TaskControlBlock>)>> = unsafe {
        UPSafeCell::new_named(Vec::new(), "FutexQueue")
    };
}

pub fn wake_futex(addr: usize, count: usize) {
    let mut queue = FUTEX_QUEUE.exclusive_access();
    let mut woken = 0;
    let mut remaining = Vec::new();
    for (a, task) in queue.drain(..) {
        if woken < count && a == addr {
            let mut inner = task.inner_exclusive_access();
            inner.task_status = TaskStatus::Ready;
            drop(inner);
            add_task(task);
            woken += 1;
        } else {
            remaining.push((a, task));
        }
    }
    *queue = remaining;
}

pub fn wake_futex_for_task(target: &Arc<TaskControlBlock>) {
    let mut queue = FUTEX_QUEUE.exclusive_access();
    let mut remaining = Vec::new();
    for (a, task) in queue.drain(..) {
        if Arc::ptr_eq(&task, target) {
            let mut inner = task.inner_exclusive_access();
            inner.task_status = TaskStatus::Ready;
            drop(inner);
            add_task(task);
        } else {
            remaining.push((a, task));
        }
    }
    *queue = remaining;
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
    task.record_time_suspend();
    let mut task_inner = task.inner_exclusive_access();
    task_inner.block_reason = Some(block_reason);
    let task_cx_ptr = &mut task_inner.task_cx as *mut KContext;
    drop(task_inner);
    take_current_task();
    NET_BLOCK_QUEUE.exclusive_access().push(task.clone());
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
    remove_from_pid2task(task.getpid());
    let (parent, orphan_children, is_thread, tid_ptr, token) = {
        // **** access current TCB exclusively
        let mut inner = task.inner_exclusive_access();
        // Change status to Zombie
        inner.task_status = TaskStatus::Zombie;
        // Record exit code
        inner.exit_code = exit_code;
        let parent = inner.parent.as_ref().and_then(|w| w.upgrade());
        let orphan_children = core::mem::take(&mut inner.children);
        let tid_ptr = inner.tid_ptr;
        let is_thread = inner.clone_vm;
        let token = inner.memory_set.token();
        // deallocate user space (skip for threads sharing vm)
        if !is_thread {
            inner.memory_set.recycle_data_pages();
        }
        // drop file descriptors
        inner.clear_fd_table();
        (parent, orphan_children, is_thread, tid_ptr, token)
    };
    // **** release current PCB before touching parent/initproc

    // wake parent waiting in waitpid and send SIGCHLD
    if let Some(parent) = parent {
        let mut queue = WAITPID_QUEUE.exclusive_access();
        if let Some(idx) = queue.iter().position(|t| t.getpid() == parent.getpid()) {
            let parent_task = queue.remove(idx);
            drop(queue);
            let mut pinner = parent_task.inner_exclusive_access();
            pinner.task_status = TaskStatus::Ready;
            drop(pinner);
            add_task(parent_task);
        } else {
            drop(queue);
        }
        // Send SIGCHLD to parent
        {
            let mut pinner = parent.inner_exclusive_access();
            if !pinner.signal_mask.contains(SignalFlags::SIGCHLD) {
                pinner.signals.insert(SignalFlags::SIGCHLD);
            }
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
    // Thread exit: clear child tid and wake futex waiter
    if is_thread && tid_ptr != 0 {
        *translated_refmut(token, tid_ptr as *mut i32) = 0;
        wake_futex(tid_ptr, 1);
    }
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
    pub static ref INITPROC: Arc<TaskControlBlock> = Arc::new({
        println!("[kernel] loading initproc binary...");
        let inode = match open_file("/bin/contest_runner", OpenFlags::RDONLY) {
            Some(f) => f,
            None => panic!("initproc not found!"),
        };
        let v = inode.read_all();
        println!("[kernel] initproc binary size: {} bytes", v.len());
        let tcb = TaskControlBlock::new(v.as_slice());
        println!("[kernel] initproc TCB created, pid={}", tcb.getpid());
        tcb
    });
}

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
        SignalFlags::SIGCHLD | SignalFlags::SIGURG | SignalFlags::SIGWINCH
        | SignalFlags::SIGIO => {
            task_inner.signals ^= signal;
        }
        SignalFlags::SIGTSTP | SignalFlags::SIGTTIN | SignalFlags::SIGTTOU => {
            task_inner.frozen = true;
            task_inner.signals ^= signal;
        }
        _ => {
            task_inner.killed = true;
            task_inner.signals ^= signal;
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

        // SA_NODEFER: block this signal + sa_mask during handler
        const SA_NODEFER: usize = 0x40000000;
        if action.flags & SA_NODEFER == 0 {
            task_inner.signal_mask |= signal;
            task_inner.signal_mask |= action.mask_flags();
        }
        task_inner.handling_sig = sig as isize;
        task_inner.signals ^= signal;

        // backup trapframe
        let trap_ctx = task_inner.get_trap_cx();
        task_inner.trap_ctx_backup = Some(trap_ctx.clone());
        let saved_sepc = trap_ctx[TrapFrameArgs::SEPC];

        // modify trapframe
        trap_ctx[TrapFrameArgs::SEPC] = handler;
        trap_ctx[TrapFrameArgs::RA] = if action.restorer != 0 {
            action.restorer
        } else {
            saved_sepc
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
                if !task_inner.signal_actions.table[hs].mask_flags().contains(signal) {
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
            let exit_code = {
                let task = current_task().unwrap();
                let inner = task.inner_exclusive_access();
                let sig = inner.exit_signal;
                if sig != 0 { sig } else { -9 }
            };
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

/// Check if current task has pending non-masked signals that should interrupt blocking syscalls
pub fn has_pending_signals() -> bool {
    let Some(task) = current_task() else { return false };
    let Some(inner) = task.try_inner_borrow() else { return false };
    let pending = inner.signals.bits() & !inner.signal_mask.bits();
    pending != 0
}
