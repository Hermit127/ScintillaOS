mod action;
mod manager;
mod pid;
mod processor;
mod signal;
#[allow(clippy::module_inception)]
mod task;

use crate::fs::{open_file, OpenFlags};
use crate::mm::translated_refmut;
use alloc::sync::Arc;
use lazy_static::*;
use log::*;
use manager::fetch_task;
use manager::remove_from_pid2task;
use polyhal::instruction::shutdown;
use polyhal::kcontext::KContext;
use polyhal_trap::trapframe::TrapFrameArgs;
use crate::sync::UPSafeCell;
use alloc::vec::Vec;

pub use action::{SignalAction, SignalActions};
pub use manager::{add_task, pid2task, PID2TCB, ready_queue_len, add_priority_task};
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
        UPSafeCell::new(Vec::new())
    };
    pub static ref NET_BLOCK_QUEUE: UPSafeCell<Vec<Arc<TaskControlBlock>>> = unsafe {
        UPSafeCell::new(Vec::new())
    };
    pub static ref WAITPID_QUEUE: UPSafeCell<Vec<Arc<TaskControlBlock>>> = unsafe {
        UPSafeCell::new(Vec::new())
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
    let task_cx_ptr = &mut task_inner.task_cx as *mut KContext;
    drop(task_inner);
    take_current_task();
    schedule(task_cx_ptr);
    task.record_time_resume();
}

pub fn block_current_and_run_next(block_reason: BlockReason) {
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

pub const IDLE_PID: usize = 0;

/// Exit the current 'Running' task and run the next task in task list.
pub fn exit_current_and_run_next(exit_code: i32) {
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
    // **** access current TCB exclusively
    let mut inner = task.inner_exclusive_access();
    // Change status to Zombie
    inner.task_status = TaskStatus::Zombie;
    // Record exit code
    inner.exit_code = exit_code;
    // do not move to its parent but under initproc

    // wake parent waiting in waitpid
    if let Some(parent) = inner.parent.as_ref().and_then(|w| w.upgrade()) {
        let mut queue = WAITPID_QUEUE.exclusive_access();
        if let Some(idx) = queue.iter().position(|t| t.getpid() == parent.getpid()) {
            let parent_task = queue.remove(idx);
            crate::println!("[exit] wake parent pid={}", parent_task.getpid());
            log::error!("[exit] wake parent pid={}", parent_task.getpid());
            drop(queue);
            let mut pinner = parent_task.inner_exclusive_access();
            pinner.task_status = TaskStatus::Ready;
            drop(pinner);
            add_task(parent_task);
        }
    }

    // ++++++ access initproc TCB exclusively
    {
        let mut initproc_inner = INITPROC.inner_exclusive_access();
        for child in inner.children.iter() {
            child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
            initproc_inner.children.push(child.clone());
        }
    }
    // ++++++ release parent PCB

    inner.children.clear();
    // deallocate user space
    inner.memory_set.recycle_data_pages();
    // drop file descriptors
    inner.fd_table.clear();
    drop(inner);
    // **** release current PCB
    // drop task manually to maintain rc correctly
    drop(task);
    // we do not have to save task context
    take_current_task();
    let mut _unused = KContext::blank();
    schedule(&mut _unused as *mut _);
}

lazy_static! {
    pub static ref INITPROC: Arc<TaskControlBlock> = Arc::new({
        let inode = open_file("/bin/contest_runner", OpenFlags::RDONLY).unwrap();
        let v = inode.read_all();
        TaskControlBlock::new(v.as_slice())
    });
}

pub fn add_initproc() {
    trace!("os::task::add_initproc");
    add_task(INITPROC.clone());
}

pub fn check_signals_error_of_current() -> Option<(i32, &'static str)> {
    trace!("os::task::check_signals_error_of_current");
    let task = current_task()?;
    let task_inner = task.inner_exclusive_access();
    // println!(
    //     "[K] check_signals_error_of_current {:?}",
    //     task_inner.signals
    // );
    task_inner.signals.check_error()
}

pub fn current_add_signal(signal: SignalFlags) {
    trace!("os::task::current_add_signal");
    let task = current_task().unwrap();
    let mut task_inner = task.inner_exclusive_access();
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
    if handler != 0 {
        // user handler

        // handle flag
        task_inner.handling_sig = sig as isize;
        task_inner.signals ^= signal;

        // backup trapframe
        let trap_ctx = task_inner.get_trap_cx();
        task_inner.trap_ctx_backup = Some(trap_ctx.clone());

        // modify trapframe
        trap_ctx[TrapFrameArgs::SEPC] = handler;

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
                | SignalFlags::SIGSYS,
        );
        if fatal {
            task_inner.killed = true;
            task_inner.signals ^= signal;
        }
    }
}

fn check_pending_signals() {
    trace!("os::task::check_pending_signals");
    for sig in 0..(MAX_SIG + 1) {
        let task = current_task().unwrap();
        let task_inner = task.inner_exclusive_access();
        let signal = SignalFlags::from_bits(1 << sig).unwrap();
        if task_inner.signals.contains(signal) && (!task_inner.signal_mask.contains(signal)) {
            let mut masked = true;
            let handling_sig = task_inner.handling_sig;
            if handling_sig == -1 {
                masked = false;
            } else {
                let handling_sig = handling_sig as usize;
                if !task_inner.signal_actions.table[handling_sig]
                    .mask
                    .contains(signal)
                {
                    masked = false;
                }
            }
            if !masked {
                drop(task_inner);
                drop(task);
                if signal == SignalFlags::SIGKILL
                    || signal == SignalFlags::SIGTERM
                    || signal == SignalFlags::SIGSTOP
                    || signal == SignalFlags::SIGCONT
                    || signal == SignalFlags::SIGDEF
                {
                    // signal is a kernel signal
                    call_kernel_signal_handler(signal);
                } else {
                    // signal is a user signal
                    call_user_signal_handler(sig, signal);
                    return;
                }
            }
        }
    }
}

pub fn handle_signals() {
    trace!("os::task::handle_signals");
    loop {
        check_pending_signals();
        let (frozen, killed) = {
            let task = current_task().unwrap();
            let task_inner = task.inner_exclusive_access();
            (task_inner.frozen, task_inner.killed)
        };
        if killed {
            exit_current_and_run_next(-9); // SIGKILL
        }
        if !frozen {
            break;
        }
        crate::fs::stdio::stdout_release_if_owner();
        suspend_current_and_run_next();
    }
}

pub fn add_sleep_task(wakeup_time: usize, task: Arc<TaskControlBlock>) {
    SLEEP_QUEUE.exclusive_access().push(SleepTask { wakeup_time, task });
}

pub fn check_and_wake_up_tasks() {
    let now = polyhal::time::Time::now().to_msec() as usize;
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