// 这里负责操作系统的进程管理和调度：汇总任务模块，并实现任务阻塞、唤醒、退出和信号递送的高层逻辑。
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
    // 定时器中断会扫描这个睡眠队列。
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

// 当前任务主动让出 CPU，状态改回 Ready 后重新入队。
// 执行时先记录当前任务状态和等待原因，再切回调度器，等条件满足后继续运行。
pub fn suspend_current_and_run_next() {
    let task = current_task().unwrap();

    // 任务离开 CPU 前先记录时间，后面重新调度回来再补恢复时间。
    task.record_time_suspend();
    // 修改任务状态和上下文指针需要独占访问 TCB。
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut KContext;
    // 主动让出 CPU 的任务仍然可运行，所以放回 Ready 队列。
    task_inner.task_status = TaskStatus::Ready;
    drop(task_inner);

    // 先入队再清空 processor.current，避免调度窗口里丢失当前任务。
    add_task(task.clone());
    // 当前 CPU 不再持有这个任务，后续从 idle 上下文重新选任务。
    take_current_task();
    // 切到 idle 调度循环，等再次选中该任务时从这里继续。
    schedule(task_cx_ptr);
    // 任务重新运行后重置计时起点，避免把睡眠时间算进内核态。
    task.record_time_resume();
}

// 当前任务进入普通阻塞态，不重新放回就绪队列。
// 执行时先记录当前任务状态和等待原因，再切回调度器，等条件满足后继续运行。
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

// 当前任务按给定原因阻塞，并挂到统一阻塞队列里。
// 执行时先记录当前任务状态和等待原因，再切回调度器，等条件满足后继续运行。
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

// 唤醒所有等 poll/select 结果的任务。
// 执行时先从等待队列里找满足条件的任务，再把它们改回 Ready，最后交给调度器运行。
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

// 唤醒等待指定管道可读的任务。
// 执行时先从等待队列里找满足条件的任务，再把它们改回 Ready，最后交给调度器运行。
pub fn wake_pipe_read_tasks(pipe_id: usize) {
    wake_pipe_tasks(pipe_id, true);
}

// 唤醒等待指定管道可写的任务。
// 执行时先从等待队列里找满足条件的任务，再把它们改回 Ready，最后交给调度器运行。
pub fn wake_pipe_write_tasks(pipe_id: usize) {
    wake_pipe_tasks(pipe_id, false);
}

// 从阻塞队列里挑出同一根管道对应方向的等待者。
// 执行时先从等待队列里找满足条件的任务，再把它们改回 Ready，最后交给调度器运行。
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

// 把一个阻塞任务移出等待队列并放回就绪队列。
// 执行时先从等待队列里找满足条件的任务，再把它们改回 Ready，最后交给调度器运行。
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

// 子进程退出时给父进程投递 SIGCHLD，并唤醒可能阻塞的父进程。
// 执行时先读取或更新任务信号状态，再决定是否改写用户上下文或唤醒任务。
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

// 从所有阻塞队列里移除指定 pid，返回是否真的删到了。
// 执行时先记录当前任务状态和等待原因，再切回调度器，等条件满足后继续运行。
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

// 强制结束指定任务，清理资源并把孤儿子进程交给 initproc。
// 执行时从阻塞队列和 pid 表移除目标，标记 Zombie，释放地址空间/fd，并唤醒等待它的父进程。
pub fn force_exit_task(task: &Arc<TaskControlBlock>, exit_code: i32) {
    let pid = task.getpid();
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

pub const IDLE_PID: usize = 0;

// 退出当前正在运行的任务，并切回调度循环。
// 执行时把当前任务标记为 Zombie，清理资源，转交孤儿子进程，最后切回 idle 调度循环且不再返回。
pub fn exit_current_and_run_next(exit_code: i32) -> ! {
    crate::fs::stdio::stdout_release_if_owner();
    trace!("os::task::exit_current_and_run_next");
    // 退出只能发生在当前运行任务上，先从 processor 拿到它。
    let task = current_task().unwrap();

    let pid = task.getpid();
    if pid == IDLE_PID {
        println!(
            "[kernel] Idle process exit with exit_code {} ...",
            exit_code
        );
        shutdown();
    }

    // 退出后不能再通过 pid 查到这个 TCB。
    remove_from_pid2task(task.getpid());
    let (parent, orphan_children) = {
        // 退出路径要修改状态、子进程和资源表，因此需要独占 TCB。
        let mut inner = task.inner_exclusive_access();
        // 标成 Zombie 后父进程 wait 才能回收退出码。
        inner.task_status = TaskStatus::Zombie;
        // exit_code 保存在 TCB 里，等待父进程 waitpid/write back。
        inner.exit_code = exit_code;
        let parent = inner.parent.as_ref().and_then(|w| w.upgrade());
        let orphan_children = core::mem::take(&mut inner.children);
        // 用户地址空间不再运行，可以立即回收数据页。
        inner.memory_set.recycle_data_pages();
        // 关闭 fd 表会递减文件、管道和 socket 的引用计数。
        inner.clear_fd_table();
        (parent, orphan_children)
    };

    // 父进程可能正在 waitpid 队列里睡眠，子进程退出后要把它叫醒。
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

    // 孤儿子进程交给 initproc 接管，保持进程树可回收。
    if !orphan_children.is_empty() {
        let mut initproc_inner = INITPROC.inner_exclusive_access();
        for child in orphan_children.iter() {
            child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
            initproc_inner.children.push(child.clone());
        }
    }
    drop(task);
    // 当前任务不会再回来运行，直接切到 idle 调度循环。
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

// 把内嵌的 initproc 加入任务队列。
// 执行时克隆全局 INITPROC 引用并加入就绪队列，启动第一个用户进程。
pub fn add_initproc() {
    trace!("os::task::add_initproc");
    add_task(INITPROC.clone());
}

// 检查当前任务是否有需要立即报错退出的信号。
// 执行时读取当前任务 pending 信号，并交给 SignalFlags::check_error 判断是否有致命信号。
pub fn check_signals_error_of_current() -> Option<(i32, &'static str)> {
    trace!("os::task::check_signals_error_of_current");
    let task = current_task()?;
    let task_inner = task.try_inner_exclusive_access()?;
    task_inner.signals.check_error()
}

// 给当前任务挂上一个待处理信号。
// 执行时尝试取得当前任务 inner，把信号位加入 pending 集合。
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

// 处理内核自己接管的信号，比如 kill/stop/cont。
// 执行时直接修改 frozen/killed 等任务状态，并从 pending 集合移除已经处理的内核信号。
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

// 准备跳到用户注册的信号处理函数，并保存原 trap 上下文。
// 执行时保存原 trap frame 和信号 mask，把用户 PC 改到 handler，返回地址改到 sigreturn trampoline。
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

        // 用户处理函数按照 ABI 从 a0 接收信号编号。
        trap_ctx[TrapFrameArgs::ARG0] = sig;
    } else {
        // 没注册用户处理函数时，fatal 信号直接杀进程，其它信号忽略。
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

// 用户态 signal handler 返回时恢复之前保存的 trap 上下文。
// 执行时恢复 signal mask 和备份 trap frame，并把原系统调用返回值交回 sigreturn。
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

// 扫描当前任务的待处理信号，选择一个可以递送的信号处理。
// 执行时按信号编号遍历 pending 集合，跳过被 mask 的信号，内核信号直接处理，用户信号改写 trap 上下文。
fn check_pending_signals() {
    trace!("os::task::check_pending_signals");
    let task = match current_task() {
        Some(t) => t,
        None => return,
    };
    for sig in 0..(MAX_SIG + 1) {
        let Some(signal) = SignalFlags::from_bits(1u64 << sig) else {
            continue;
        };
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

// trap 返回用户态前统一处理信号、冻结和被杀状态。
// 执行时反复递送可处理信号；若任务被杀就退出，若被冻结就让出 CPU 等 SIGCONT。
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
                if sig != 0 {
                    sig
                } else {
                    -9
                }
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

// 把任务加入睡眠队列，等定时器到点唤醒。
// 执行时先记录当前任务状态和等待原因，再切回调度器，等条件满足后继续运行。
pub fn add_sleep_task(wakeup_time: usize, task: Arc<TaskControlBlock>) {
    SLEEP_QUEUE
        .exclusive_access()
        .push(SleepTask { wakeup_time, task });
}

// 定时器里扫描睡眠队列，把到期任务放回就绪队列。
// 执行时先从等待队列里找满足条件的任务，再把它们改回 Ready，最后交给调度器运行。
pub fn check_and_wake_up_tasks() {
    let now = polyhal::time::Time::now().to_nsec() as usize;
    let mut queue = SLEEP_QUEUE.exclusive_access();

    let mut i = 0;
    while i < queue.len() {
        if queue[i].wakeup_time <= now {
            let sleep_task = queue.remove(i);

            // 睡眠时间到后把任务改回 Ready，再放回调度队列。
            let mut inner = sleep_task.task.inner_exclusive_access();
            inner.task_status = TaskStatus::Ready;
            drop(inner);
            add_task(sleep_task.task);
        } else {
            i += 1;
        }
    }
}
