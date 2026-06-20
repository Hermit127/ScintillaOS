use crate::fs::stdio::stdout_release_if_owner;
use crate::fs::{open_file, open_file_at, File, OpenFlags};
use crate::mm::{
    frames_alloc, get_elf_interp_file, translated_byte_buffer, translated_ref, translated_refmut,
    translated_str, FrameTracker, MapPermission,
};
use crate::sync::UPSafeCell;
use crate::task::{
    add_task, blank_kcontext, block_current_and_run_next, current_task, current_user_token,
    exit_current_and_run_next, force_exit_task, force_exit_task_group, pid2task, pid_alloc,
    sleep_current_and_run_next, suspend_current_and_run_next, wake_blocked_task, BlockReason,
    KernelStack, SignalAction, SignalFlags, TaskControlBlock, TaskControlBlockInner, TaskStatus,
    MAX_SIG, PID2TCB,
};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use lazy_static::lazy_static;
use log::info;
use polyhal::kcontext::KContextArgs;
use polyhal::time::Time;
use polyhal_trap::trapframe::TrapFrameArgs;

struct ShmSegment {
    key: usize,
    size: usize,
    frames: Vec<FrameTracker>,
}

lazy_static! {
    static ref SHM_SEGMENTS: UPSafeCell<Vec<Option<ShmSegment>>> =
        unsafe { UPSafeCell::new(Vec::new()) };
}

static LAST_USER_TIME_US: AtomicU64 = AtomicU64::new(0);
static LAST_USER_TIMES_TICK: AtomicU64 = AtomicU64::new(0);
const USER_TIME_MIN_STEP_US: u64 = 1;
const USER_CLOCK_TICK_US: u64 = 10_000;
const USER_CLOCK_TICK_NS: usize = USER_CLOCK_TICK_US as usize * 1_000;
const NSEC_PER_SEC: usize = 1_000_000_000;
const NSEC_PER_USEC: usize = 1_000;

fn monotonic_user_time_us() -> u64 {
    let hw_us = (Time::now().to_nsec() / 1000) as u64;
    loop {
        let last = LAST_USER_TIME_US.load(Ordering::Relaxed);
        let next = hw_us.max(last.saturating_add(USER_TIME_MIN_STEP_US));
        if LAST_USER_TIME_US
            .compare_exchange(last, next, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

fn monotonic_user_times_tick() -> u64 {
    let hw_tick = monotonic_user_time_us() / USER_CLOCK_TICK_US;
    loop {
        let last = LAST_USER_TIMES_TICK.load(Ordering::Relaxed);
        let next = hw_tick.max(last.saturating_add(1));
        if LAST_USER_TIMES_TICK
            .compare_exchange(last, next, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

const EXEC_PREFIX_LEN: usize = 512;

fn read_exec_prefix(inode: &Arc<crate::fs::inode::OSInode>) -> Vec<u8> {
    let mut buf = alloc::vec![0u8; EXEC_PREFIX_LEN];
    let n = inode.read_at(0, &mut buf);
    if n <= 0 {
        Vec::new()
    } else {
        buf.truncate(n as usize);
        buf
    }
}

pub fn user_realtime_sec() -> i64 {
    (monotonic_user_time_us() / 1_000_000) as i64
}

pub fn user_realtime_sec_ceil() -> i64 {
    ((monotonic_user_time_us() + 999_999) / 1_000_000) as i64
}

fn nsec_to_timeval(ns: usize) -> TimeVal {
    let usec = if ns == 0 {
        0
    } else {
        (ns + NSEC_PER_USEC - 1) / NSEC_PER_USEC
    };
    TimeVal {
        sec: (usec / 1_000_000) as isize,
        usec: (usec % 1_000_000) as isize,
    }
}

fn nsec_to_clock_ticks(ns: usize) -> isize {
    if ns == 0 {
        0
    } else {
        ((ns + USER_CLOCK_TICK_NS - 1) / USER_CLOCK_TICK_NS) as isize
    }
}

fn looks_like_text_script(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    let sample_len = data.len().min(256);
    let mut has_text = false;
    for &b in data[..sample_len].iter() {
        if b == 0 {
            return false;
        }
        if b == b'\n' || b == b'\r' || b == b'\t' || (b >= 0x20 && b <= 0x7e) {
            if b > b' ' {
                has_text = true;
            }
        } else {
            return false;
        }
    }
    has_text
}

const PROT_NONE: usize = 0;
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const PROT_EXEC: usize = 4;

const MAP_SHARED: usize = 0x01;
const MAP_PRIVATE: usize = 0x02;
const MAP_FIXED: usize = 0x10;
const MAP_ANONYMOUS: usize = 0x20;

const UTSNAME_LEN: usize = 65;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Tms {
    pub tms_utime: isize,
    pub tms_stime: isize,
    pub tms_cutime: isize,
    pub tms_cstime: isize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UtsName {
    pub sysname: [u8; UTSNAME_LEN],
    pub nodename: [u8; UTSNAME_LEN],
    pub release: [u8; UTSNAME_LEN],
    pub version: [u8; UTSNAME_LEN],
    pub machine: [u8; UTSNAME_LEN],
    pub domainname: [u8; UTSNAME_LEN],
}

impl UtsName {
    fn set_str(field: &mut [u8; UTSNAME_LEN], s: &str) {
        let bytes = s.as_bytes();
        let len = bytes.len().min(UTSNAME_LEN - 1);
        field[..len].copy_from_slice(&bytes[..len]);
        field[len] = 0;
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct TimeVal {
    pub sec: isize,
    pub usec: isize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct TimeZone {
    minuteswest: i32,
    dsttime: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RUsage {
    ru_utime: TimeVal,
    ru_stime: TimeVal,
    ru_maxrss: isize,
    ru_ixrss: isize,
    ru_idrss: isize,
    ru_isrss: isize,
    ru_minflt: isize,
    ru_majflt: isize,
    ru_nswap: isize,
    ru_inblock: isize,
    ru_oublock: isize,
    ru_msgsnd: isize,
    ru_msgrcv: isize,
    ru_nsignals: isize,
    ru_nvcsw: isize,
    ru_nivcsw: isize,
}

fn clear_current_tid_ptr() {
    let Some(task) = current_task() else {
        return;
    };
    let tid = {
        let inner = task.inner_exclusive_access();
        inner.tid_ptr
    };
    if tid != 0 {
        *translated_refmut(current_user_token(), tid as *mut i32) = 0;
    }
}

pub fn sys_exit(exit_code: i32) -> ! {
    clear_current_tid_ptr();
    exit_current_and_run_next(exit_code)
}

pub fn sys_yield() -> isize {
    stdout_release_if_owner();
    suspend_current_and_run_next();
    0
}

pub fn sys_get_time(ts_ptr: usize, tz_ptr: usize) -> isize {
    let now_us = monotonic_user_time_us();
    if ts_ptr == 0 && tz_ptr == 0 {
        return (now_us / 1000) as isize;
    }

    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if ts_ptr != 0
        && !inner.memory_set.valid_user_buf(
            ts_ptr as *const u8,
            core::mem::size_of::<TimeVal>(),
            true,
        )
    {
        return -14;
    }
    if tz_ptr != 0
        && !inner.memory_set.valid_user_buf(
            tz_ptr as *const u8,
            core::mem::size_of::<TimeZone>(),
            true,
        )
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);

    if ts_ptr != 0 {
        let ts = translated_refmut(token, ts_ptr as *mut TimeVal);
        ts.sec = (now_us / 1_000_000) as isize;
        ts.usec = (now_us % 1_000_000) as isize;
    }
    if tz_ptr != 0 {
        *translated_refmut(token, tz_ptr as *mut TimeZone) = TimeZone {
            minuteswest: 0,
            dsttime: 0,
        };
    }
    0
}

pub fn sys_getpid() -> isize {
    current_task()
        .map(|t| t.inner_exclusive_access().tgid as isize)
        .unwrap_or(-1)
}

pub fn sys_getppid() -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    match &inner.parent {
        Some(parent_weak) => parent_weak
            .upgrade()
            .map(|p| p.getpid() as isize)
            .unwrap_or(1),
        None => 1,
    }
}

const CLONE_VM: usize = 0x100;
const CLONE_FS: usize = 0x200;
const CLONE_FILES: usize = 0x400;
const CLONE_SIGHAND: usize = 0x800;
const CLONE_THREAD: usize = 0x10000;
const CLONE_SETTLS: usize = 0x80000;
const CLONE_PARENT_SETTID: usize = 0x100000;
const CLONE_CHILD_CLEARTID: usize = 0x200000;
const CLONE_CHILD_SETTID: usize = 0x01000000;

pub fn sys_clone(flags: usize, stack: usize, ptid: usize, tls: usize, ctid: usize) -> isize {
    let current_task = current_task().unwrap();
    let parent_tgid = {
        let inner = current_task.inner_exclusive_access();
        inner.tgid
    };
    let is_thread = flags & CLONE_THREAD != 0;
    let Some(new_task) = current_task.fork() else {
        return -12;
    };
    let new_pid = new_task.pid.0;

    let child_token = {
        let mut inner = new_task.inner_exclusive_access();
        inner.trap_cx[TrapFrameArgs::RET] = 0;
        if stack != 0 {
            inner.trap_cx[TrapFrameArgs::SP] = stack;
        }
        if is_thread {
            inner.tgid = parent_tgid;
        }
        if flags & CLONE_SETTLS != 0 && tls != 0 {
            inner.trap_cx[TrapFrameArgs::TLS] = tls;
        }
        if flags & CLONE_CHILD_CLEARTID != 0 && ctid != 0 {
            inner.tid_ptr = ctid;
        }
        inner.memory_set.token()
    };

    if flags & CLONE_CHILD_SETTID != 0 && ctid != 0 {
        *translated_refmut(child_token, ctid as *mut i32) = new_pid as i32;
    }
    if flags & CLONE_PARENT_SETTID != 0 && ptid != 0 {
        *translated_refmut(current_user_token(), ptid as *mut i32) = new_pid as i32;
    }

    if is_thread {
        current_task
            .inner_exclusive_access()
            .children
            .retain(|child| child.getpid() != new_pid);
    }
    add_task(new_task);
    new_pid as isize
}

pub fn sys_fork() -> isize {
    let current_task = current_task().unwrap();
    let Some(new_task) = current_task.fork() else {
        return -12;
    };
    let new_pid = new_task.pid.0;
    {
        let mut inner = new_task.inner_exclusive_access();
        inner.trap_cx[TrapFrameArgs::RET] = 0;
    }
    add_task(new_task);
    new_pid as isize
}

pub fn sys_exec(path: *const u8, mut args: *const usize, mut envp: *const usize) -> isize {
    let token = current_user_token();
    let path = translated_str(token, path);
    let mut args_vec: Vec<String> = Vec::new();
    if !args.is_null() {
        loop {
            let arg_str_ptr = *translated_ref(token, args);
            if arg_str_ptr == 0 {
                break;
            }
            args_vec.push(translated_str(token, arg_str_ptr as *const u8));
            unsafe {
                args = args.add(1);
            }
        }
    }
    let mut env_vec: Vec<String> = Vec::new();
    if !envp.is_null() {
        loop {
            let env_str_ptr = *translated_ref(token, envp);
            if env_str_ptr == 0 {
                break;
            }
            env_vec.push(translated_str(token, env_str_ptr as *const u8));
            unsafe {
                envp = envp.add(1);
            }
        }
    }
    let (cwd, root) = {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        (inner.cwd.clone(), inner.root.clone())
    };
    let open_exec = |path: &str| {
        if let Some(stripped) = path.strip_prefix('/') {
            open_file_at(Some(root.clone()), stripped, OpenFlags::RDONLY, 0)
        } else {
            open_file_at(Some(cwd.clone()), path, OpenFlags::RDONLY, 0).or_else(|| {
                let bin_path = format!("bin/{}", path);
                open_file_at(Some(root.clone()), bin_path.as_str(), OpenFlags::RDONLY, 0)
            })
        }
    };
    let app_inode = open_exec(path.as_str());
    if let Some(app_inode) = app_inode {
        let prefix = read_exec_prefix(&app_inode);
        if prefix.len() >= 2 && &prefix[..2] == b"#!" {
            let end = prefix
                .iter()
                .position(|&b| b == b'\n')
                .unwrap_or(prefix.len());
            let line = core::str::from_utf8(&prefix[2..end]).unwrap_or("");
            let parts: alloc::vec::Vec<&str> = line.split_whitespace().collect();
            if !parts.is_empty() {
                let ipath = if parts[0].starts_with('/') {
                    alloc::string::String::from(parts[0])
                } else {
                    alloc::format!("/bin/{}", parts[0])
                };
                let mut new_args = alloc::vec![ipath.clone()];
                if parts.len() > 1 {
                    new_args.push(alloc::string::String::from(parts[1]));
                }
                new_args.push(path);
                for a in args_vec.iter().skip(1) {
                    new_args.push(a.clone());
                }
                drop(app_inode);
                if let Some(i2) = open_exec(&ipath) {
                    let interp_inode = get_elf_interp_file(i2.as_ref()).and_then(|interp_path| {
                        open_exec(&interp_path).or_else(|| {
                            let usr_interp = alloc::format!("/usr{}", interp_path);
                            open_exec(&usr_interp)
                        })
                    });
                    let interp_file = interp_inode
                        .as_ref()
                        .map(|inode| inode.as_ref() as &dyn File);
                    if current_task().unwrap().exec_file_with_interp(
                        i2.as_ref(),
                        interp_file,
                        new_args,
                        env_vec,
                    ) {
                        return 0;
                    }
                    return -1;
                }
            }
            return -1;
        }
        let task = current_task().unwrap();
        if prefix.len() < 4 || &prefix[..4] != b"\x7fELF" {
            if looks_like_text_script(prefix.as_slice()) {
                let mut sh_args = alloc::vec![alloc::string::String::from("/bin/sh"), path.clone()];
                for a in args_vec.iter().skip(1) {
                    sh_args.push(a.clone());
                }
                drop(app_inode);
                if let Some(sh_inode) = open_exec("/bin/sh").or_else(|| open_exec("/busybox")) {
                    let sh_prefix = read_exec_prefix(&sh_inode);
                    if sh_prefix.len() >= 4 && &sh_prefix[..4] == b"\x7fELF" {
                        let interp_inode =
                            get_elf_interp_file(sh_inode.as_ref()).and_then(|interp_path| {
                                open_exec(&interp_path).or_else(|| {
                                    let usr_interp = alloc::format!("/usr{}", interp_path);
                                    open_exec(&usr_interp)
                                })
                            });
                        let interp_file = interp_inode
                            .as_ref()
                            .map(|inode| inode.as_ref() as &dyn File);
                        if current_task().unwrap().exec_file_with_interp(
                            sh_inode.as_ref(),
                            interp_file,
                            sh_args,
                            env_vec,
                        ) {
                            return 0;
                        }
                    }
                }
            }
            return -8;
        }
        // Check for dynamically linked ELF (has PT_INTERP)
        if let Some(interp_path) = get_elf_interp_file(app_inode.as_ref()) {
            let interp_inode = open_exec(&interp_path).or_else(|| {
                let usr_interp = alloc::format!("/usr{}", interp_path);
                open_exec(&usr_interp)
            });
            if let Some(interp_inode) = interp_inode {
                if task.exec_file_with_interp(
                    app_inode.as_ref(),
                    Some(interp_inode.as_ref() as &dyn File),
                    args_vec,
                    env_vec,
                ) {
                    return 0;
                }
                return -1;
            }
            return -1;
        }
        if task.exec_file_with_interp(app_inode.as_ref(), None, args_vec, env_vec) {
            0
        } else {
            -1
        }
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -ECHILD.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32, options: usize) -> isize {
    const WNOHANG: usize = 1;
    loop {
        let task = current_task().unwrap();
        let zombie = {
            let mut inner = task.inner_exclusive_access();
            let mut has_child = false;
            let mut zombie_idx = None;
            for (idx, child) in inner.children.iter().enumerate() {
                if pid > 0 && pid as usize != child.getpid() {
                    continue;
                }
                has_child = true;
                if child
                    .try_inner_borrow()
                    .map(|inner| inner.is_zombie())
                    .unwrap_or(false)
                {
                    zombie_idx = Some(idx);
                    break;
                }
            }
            if !has_child {
                return -10;
            }
            zombie_idx.map(|idx| {
                let child = inner.children.remove(idx);
                let found_pid = child.getpid();
                let child_inner = child.inner_exclusive_access();
                let exit_code = child_inner.exit_code;
                inner.cutime = inner
                    .cutime
                    .saturating_add(child_inner.utime)
                    .saturating_add(child_inner.cutime);
                inner.cstime = inner
                    .cstime
                    .saturating_add(child_inner.stime)
                    .saturating_add(child_inner.cstime);
                (found_pid, exit_code, inner.memory_set.token())
            })
        };

        if let Some((found_pid, exit_code, token)) = zombie {
            if !exit_code_ptr.is_null() {
                if exit_code < 0 {
                    *translated_refmut(token, exit_code_ptr) = -exit_code;
                } else {
                    *translated_refmut(token, exit_code_ptr) = (exit_code << 8) as i32;
                }
            }
            return found_pid as isize;
        }
        // No zombie: non-blocking return 0, or sleep
        if options & WNOHANG != 0 {
            return 0;
        }
        let mut queue = crate::task::WAITPID_QUEUE.exclusive_access();
        if !queue.iter().any(|t| t.getpid() == task.getpid()) {
            queue.push(task.clone());
        }
        drop(queue);
        drop(task);
        crate::task::sleep_current_and_run_next();
        // Loop back to check again
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct WaitSigInfo {
    si_signo: i32,
    si_errno: i32,
    si_code: i32,
    _pad0: i32,
    si_pid: i32,
    si_uid: u32,
    si_status: i32,
    _pad1: i32,
    si_utime: isize,
    si_stime: isize,
    _pad: [u8; 128 - 40],
}

fn write_wait_siginfo(info_ptr: *mut u8, pid: i32, exit_code: i32) {
    if info_ptr.is_null() {
        return;
    }
    const SIGCHLD: i32 = 17;
    const CLD_EXITED: i32 = 1;
    const CLD_KILLED: i32 = 2;
    let (code, status) = if exit_code < 0 {
        (CLD_KILLED, -exit_code)
    } else {
        (CLD_EXITED, exit_code)
    };
    let token = current_user_token();
    *translated_refmut(token, info_ptr as *mut WaitSigInfo) = WaitSigInfo {
        si_signo: if pid == 0 { 0 } else { SIGCHLD },
        si_errno: 0,
        si_code: if pid == 0 { 0 } else { code },
        _pad0: 0,
        si_pid: pid,
        si_uid: 0,
        si_status: status,
        _pad1: 0,
        si_utime: 0,
        si_stime: 0,
        _pad: [0; 128 - 40],
    };
}

pub fn sys_waitid(
    which: usize,
    id: usize,
    info_ptr: *mut u8,
    options: usize,
    _rusage: *mut u8,
) -> isize {
    const P_ALL: usize = 0;
    const P_PID: usize = 1;
    const WNOHANG: usize = 1;

    let pid = match which {
        P_ALL => -1,
        P_PID => id as isize,
        _ => return -22,
    };

    loop {
        let task = current_task().unwrap();
        let zombie = {
            let mut inner = task.inner_exclusive_access();
            let mut has_child = false;
            let mut zombie_idx = None;
            for (idx, child) in inner.children.iter().enumerate() {
                if pid > 0 && pid as usize != child.getpid() {
                    continue;
                }
                has_child = true;
                if child
                    .try_inner_borrow()
                    .map(|inner| inner.is_zombie())
                    .unwrap_or(false)
                {
                    zombie_idx = Some(idx);
                    break;
                }
            }
            if !has_child {
                return -10;
            }
            zombie_idx.map(|idx| inner.children.remove(idx))
        };

        if let Some(child) = zombie {
            let found_pid = child.getpid();
            let exit_code = {
                let child_inner = child.inner_exclusive_access();
                let exit_code = child_inner.exit_code;
                let mut parent_inner = task.inner_exclusive_access();
                parent_inner.cutime = parent_inner
                    .cutime
                    .saturating_add(child_inner.utime)
                    .saturating_add(child_inner.cutime);
                parent_inner.cstime = parent_inner
                    .cstime
                    .saturating_add(child_inner.stime)
                    .saturating_add(child_inner.cstime);
                exit_code
            };
            write_wait_siginfo(info_ptr, found_pid as i32, exit_code);
            return 0;
        }

        if options & WNOHANG != 0 {
            write_wait_siginfo(info_ptr, 0, 0);
            return 0;
        }
        let mut queue = crate::task::WAITPID_QUEUE.exclusive_access();
        if !queue.iter().any(|t| t.getpid() == task.getpid()) {
            queue.push(task.clone());
        }
        drop(queue);
        drop(task);
        crate::task::sleep_current_and_run_next();
    }
}

fn signal_task(task: &Arc<TaskControlBlock>, signum: i32) {
    let target = task.getpid();
    if signum == 9 && current_task().map(|t| t.getpid() != target).unwrap_or(true) {
        force_exit_task_group(task, -9);
        force_exit_task(task, -9);
        return;
    }
    if signum < 0 || signum as usize > MAX_SIG {
        return;
    }
    let flag = SignalFlags::from_bits(1u32 << signum as usize).unwrap();
    {
        let mut task_ref = task.inner_exclusive_access();
        if task_ref.signals.contains(flag) {
            return;
        }
        task_ref.signals.insert(flag);
        if flag == SignalFlags::SIGTERM || flag == SignalFlags::SIGKILL {
            task_ref.killed = true;
            task_ref.exit_signal = -(signum as i32);
        } else if task_ref.signal_actions.table[signum as usize].handler == 0 {
            let fatal = flag.intersects(
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
                task_ref.killed = true;
                task_ref.exit_signal = -(signum as i32);
            }
        }
    };
    wake_blocked_task(task);
}

pub fn sys_kill(pid: isize, signum: i32) -> isize {
    const MAX_USER_SIG: usize = 64;
    if signum < 0 || signum as usize > MAX_USER_SIG {
        return -22; // EINVAL
    }
    let targets: Vec<Arc<TaskControlBlock>> = if pid > 0 {
        pid2task(pid as usize).into_iter().collect()
    } else {
        let pgid = if pid == 0 {
            current_task()
                .map(|task| task.inner_exclusive_access().pgid)
                .unwrap_or(0)
        } else if pid == -1 {
            0
        } else {
            (-pid) as usize
        };
        let map = PID2TCB.exclusive_access();
        map.values()
            .filter(|task| {
                if task.getpid() == 0 {
                    return false;
                }
                if pid == -1 {
                    return true;
                }
                task.try_inner_borrow()
                    .map(|inner| inner.pgid == pgid)
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    };
    if targets.is_empty() {
        return 0;
    }
    if signum == 0 {
        return 0;
    }
    if signum as usize > MAX_SIG {
        return 0;
    }
    for task in targets {
        signal_task(&task, signum);
    }
    0
}

pub fn sys_setpgid(pid: usize, pgid: usize) -> isize {
    let target_pid = if pid == 0 {
        current_task().map(|task| task.getpid()).unwrap_or(0)
    } else {
        pid
    };
    let new_pgid = if pgid == 0 { target_pid } else { pgid };
    let Some(task) = pid2task(target_pid) else {
        return -3;
    };
    task.inner_exclusive_access().pgid = new_pgid;
    0
}

pub fn sys_getpgid(pid: usize) -> isize {
    let target_pid = if pid == 0 {
        current_task().map(|task| task.getpid()).unwrap_or(0)
    } else {
        pid
    };
    pid2task(target_pid)
        .map(|task| task.inner_exclusive_access().pgid as isize)
        .unwrap_or(-3)
}

pub fn sys_setsid() -> isize {
    let Some(task) = current_task() else {
        return -3;
    };
    let pid = task.getpid();
    let mut inner = task.inner_exclusive_access();
    inner.sid = pid;
    inner.pgid = pid;
    pid as isize
}

fn user_sigset_to_flags(raw: u64) -> SignalFlags {
    let mut flags = SignalFlags::empty();
    for sig in 1..=MAX_SIG {
        if (raw & (1u64 << (sig - 1))) != 0 {
            if let Some(flag) = SignalFlags::from_bits(1u32 << sig) {
                flags.insert(flag);
            }
        }
    }
    flags
}

fn flags_to_user_sigset(flags: SignalFlags) -> u64 {
    let mut raw = 0u64;
    for sig in 1..=MAX_SIG {
        if let Some(flag) = SignalFlags::from_bits(1u32 << sig) {
            if flags.contains(flag) {
                raw |= 1u64 << (sig - 1);
            }
        }
    }
    raw
}

pub fn sys_sigprocmask(how: i32, set: *const u8, old_set: *mut u8, _sigsetsize: usize) -> isize {
    const SIG_BLOCK: i32 = 0;
    const SIG_UNBLOCK: i32 = 1;
    const SIG_SETMASK: i32 = 2;
    if let Some(task) = current_task() {
        let token = current_user_token();
        let mut inner = task.inner_exclusive_access();
        let old_mask = inner.signal_mask;
        if !old_set.is_null() {
            *translated_refmut(token, old_set as *mut u64) = flags_to_user_sigset(old_mask);
        }
        if set.is_null() {
            return 0;
        }
        let raw_mask = *translated_ref(token, set as *const u64);
        let mut new_mask = user_sigset_to_flags(raw_mask);
        new_mask.remove(SignalFlags::SIGKILL | SignalFlags::SIGSTOP);
        inner.signal_mask = match how {
            SIG_BLOCK => old_mask | new_mask,
            SIG_UNBLOCK => old_mask & !new_mask,
            SIG_SETMASK => new_mask,
            _ => return -22,
        };
        0
    } else {
        -3
    }
}

pub fn sys_sigreturn() -> isize {
    crate::task::restore_current_signal_context().unwrap_or(-1)
}

fn check_sigaction_error(signal: SignalFlags, _action: usize, _old_action: usize) -> bool {
    signal == SignalFlags::SIGKILL || signal == SignalFlags::SIGSTOP
}

pub fn sys_sigaction(
    signum: i32,
    action: *const SignalAction,
    old_action: *mut SignalAction,
) -> isize {
    const MAX_USER_SIG: usize = 64;
    let token = current_user_token();
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if signum <= 0 || signum as usize > MAX_USER_SIG {
        return -1;
    }
    if signum as usize > MAX_SIG {
        if !old_action.is_null() {
            *translated_refmut(token, old_action) = SignalAction::default();
        }
        return 0;
    }
    if let Some(flag) = SignalFlags::from_bits(1 << signum) {
        if check_sigaction_error(flag, action as usize, old_action as usize) {
            return -1;
        }
        if !old_action.is_null() {
            *translated_refmut(token, old_action) = inner.signal_actions.table[signum as usize];
        }
        if !action.is_null() {
            inner.signal_actions.table[signum as usize] = *translated_ref(token, action);
        }
        0
    } else {
        -1
    }
}

pub fn sys_brk(brk_addr: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    if brk_addr == 0 {
        return inner.program_brk as isize;
    }

    let heap_bottom = inner.heap_bottom;
    if brk_addr < heap_bottom {
        return -1;
    }

    let old_brk = inner.program_brk;

    let old_brk_page = (old_brk + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE;
    let new_brk_page = (brk_addr + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE;

    if new_brk_page > old_brk_page {
        let start_va = polyhal::VirtAddr::new(old_brk_page * crate::config::PAGE_SIZE);
        let end_va = polyhal::VirtAddr::new(new_brk_page * crate::config::PAGE_SIZE);

        let map_area = crate::mm::MapArea::new(
            start_va,
            end_va,
            crate::mm::MapType::Framed,
            crate::mm::MapPermission::R | crate::mm::MapPermission::W | crate::mm::MapPermission::U,
        );
        if !inner.memory_set.push(map_area, None, 0) {
            return -1; // ENOMEM
        }
    } else if new_brk_page < old_brk_page {
        let shrink_start = polyhal::VirtAddr::new(new_brk_page * crate::config::PAGE_SIZE);
        let shrink_end = polyhal::VirtAddr::new(old_brk_page * crate::config::PAGE_SIZE);
        inner.memory_set.munmap(shrink_start, shrink_end);
    }
    inner.program_brk = brk_addr;
    brk_addr as isize
}

pub fn sys_sleep(duration: u64) -> isize {
    let wakeup_time = sys_get_time(0, 0) + duration as isize;
    loop {
        if current_task()
            .map(|task| task.inner_exclusive_access().killed)
            .unwrap_or(false)
        {
            return -4;
        }
        let now = sys_get_time(0, 0);
        if now >= wakeup_time {
            break;
        }
        sys_yield();
    }
    0
}

pub fn sys_shutdown() -> isize {
    polyhal::instruction::shutdown();
    0
}

fn get_map_permission(prot: usize) -> crate::mm::MapPermission {
    let mut perm = crate::mm::MapPermission::U;
    if prot & PROT_READ != 0 {
        perm |= crate::mm::MapPermission::R;
    }
    if prot & PROT_WRITE != 0 {
        perm |= crate::mm::MapPermission::W;
    }
    if prot & PROT_EXEC != 0 {
        perm |= crate::mm::MapPermission::X;
    }
    perm
}

pub fn sys_mmap(
    start: usize,
    len: usize,
    prot: usize,
    flags: usize,
    fd: i32,
    offset: usize,
) -> isize {
    if start % crate::config::PAGE_SIZE != 0 || len == 0 {
        return -1;
    }

    let task = crate::task::current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    let permission = get_map_permission(prot);

    let real_start = if start == 0 {
        let max_end: usize = inner.memory_set.max_end_vaddr().into();
        (max_end + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE
            * crate::config::PAGE_SIZE
    } else {
        start
    };

    if (flags & MAP_ANONYMOUS) != 0 {
        let start_addr = polyhal::VirtAddr::from(real_start);
        let end_addr = polyhal::VirtAddr::from(real_start + len);
        if (flags & MAP_FIXED) != 0 && inner.memory_set.is_overlap(start_addr, end_addr) {
            inner.memory_set.munmap(start_addr, end_addr);
        }
        if (flags & MAP_SHARED) != 0 {
            let aligned_len =
                (len + crate::config::PAGE_SIZE - 1) & !(crate::config::PAGE_SIZE - 1);
            let pages = aligned_len / crate::config::PAGE_SIZE;
            let Some(frames) = frames_alloc(pages) else {
                return -12;
            };
            return match inner.memory_set.insert_shared_frames(
                start_addr,
                aligned_len,
                &frames,
                permission,
            ) {
                Some(addr) => addr.raw() as isize,
                None => -12,
            };
        }
        if !inner
            .memory_set
            .insert_framed_area(start_addr, end_addr, permission)
        {
            return -12;
        }
        return real_start as isize;
    } else {
        let start_addr = polyhal::VirtAddr::from(real_start);
        let end_addr = polyhal::VirtAddr::from(real_start + len);
        if (flags & MAP_FIXED) != 0 && inner.memory_set.is_overlap(start_addr, end_addr) {
            inner.memory_set.munmap(start_addr, end_addr);
        }

        if fd < 0 || fd as usize >= inner.fd_table.len() {
            return -1;
        }

        if let Some(file) = inner.fd_table[fd as usize].clone() {
            let ok = inner
                .memory_set
                .insert_file_area(start_addr, end_addr, permission, file, offset);
            if ok {
                return real_start as isize;
            } else {
                return -1;
            }
        } else {
            return -1;
        }
    }
}

pub fn sys_munmap(start: usize, len: usize) -> isize {
    if start % crate::config::PAGE_SIZE != 0 || len == 0 {
        return -1;
    }

    let task = crate::task::current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    let start_addr = polyhal::VirtAddr::from(start);
    let end_addr = polyhal::VirtAddr::from(start + len);

    if inner.memory_set.munmap(start_addr, end_addr) {
        0
    } else {
        -1
    }
}

pub fn sys_msync(start: usize, len: usize, flags: usize) -> isize {
    if (len > 0 && start as u64 + len as u64 > usize::MAX as u64)
        || start % crate::config::PAGE_SIZE != 0
    {
        return -22;
    }
    let task = crate::task::current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let start_addr = polyhal::VirtAddr::from(start);
    inner.memory_set.msync(start_addr, len, flags)
}

pub fn sys_shmget(key: usize, size: usize, flags: usize) -> isize {
    const IPC_PRIVATE: usize = 0;
    const IPC_CREAT: usize = 0o1000;
    const IPC_EXCL: usize = 0o2000;
    let mut segments = SHM_SEGMENTS.exclusive_access();

    if key != IPC_PRIVATE {
        if let Some((idx, _)) = segments
            .iter()
            .enumerate()
            .find(|(_, seg)| seg.as_ref().is_some_and(|seg| seg.key == key))
        {
            if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                return -17;
            }
            return (idx + 1) as isize;
        }
        if flags & IPC_CREAT == 0 {
            return -2;
        }
    }

    if size == 0 {
        return -22;
    }
    let aligned_size = (size + crate::config::PAGE_SIZE - 1) & !(crate::config::PAGE_SIZE - 1);
    let pages = aligned_size / crate::config::PAGE_SIZE;
    let Some(frames) = frames_alloc(pages) else {
        return -12;
    };
    let segment = ShmSegment {
        key,
        size: aligned_size,
        frames,
    };
    if let Some(idx) = segments.iter().position(|seg| seg.is_none()) {
        segments[idx] = Some(segment);
        (idx + 1) as isize
    } else {
        segments.push(Some(segment));
        segments.len() as isize
    }
}

pub fn sys_shmat(shmid: usize, shmaddr: usize, shmflg: usize) -> isize {
    const SHM_RDONLY: usize = 0o10000;
    if shmid == 0 {
        return -22;
    }
    let segments = SHM_SEGMENTS.exclusive_access();
    let Some(Some(segment)) = segments.get(shmid - 1) else {
        return -22;
    };
    let mut permission = MapPermission::R | MapPermission::U;
    if shmflg & SHM_RDONLY == 0 {
        permission |= MapPermission::W;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let addr = inner.memory_set.insert_shared_frames(
        polyhal::VirtAddr::from(shmaddr),
        segment.size,
        &segment.frames,
        permission,
    );
    match addr {
        Some(addr) => addr.raw() as isize,
        None => -12,
    }
}

pub fn sys_shmdt(_shmaddr: usize) -> isize {
    0
}

pub fn sys_shmctl(shmid: usize, _cmd: usize, _buf: *mut u8) -> isize {
    if shmid == 0 {
        return -22;
    }
    let segments = SHM_SEGMENTS.exclusive_access();
    if shmid - 1 >= segments.len() || segments[shmid - 1].is_none() {
        return -22;
    }
    0
}

pub fn sys_times(tms_ptr: usize) -> isize {
    let task = crate::task::current_task().unwrap();

    task.record_time_suspend();
    task.record_time_resume();

    if tms_ptr != 0 {
        let inner = task.inner_exclusive_access();
        let tms = Tms {
            tms_utime: nsec_to_clock_ticks(inner.utime),
            tms_stime: nsec_to_clock_ticks(inner.stime),
            tms_cutime: nsec_to_clock_ticks(inner.cutime),
            tms_cstime: nsec_to_clock_ticks(inner.cstime),
        };
        let token = inner.get_user_token();
        drop(inner);
        *crate::mm::translated_refmut(token, tms_ptr as *mut Tms) = tms;
    }

    monotonic_user_times_tick() as isize
}

pub fn sys_uname(uts_ptr: usize) -> isize {
    if uts_ptr == 0 {
        return -1;
    }

    let task = crate::task::current_task().unwrap();
    let token = task.inner_exclusive_access().get_user_token();
    let uts = crate::mm::translated_refmut(token, uts_ptr as *mut UtsName);

    *uts = UtsName {
        sysname: [0; UTSNAME_LEN],
        nodename: [0; UTSNAME_LEN],
        release: [0; UTSNAME_LEN],
        version: [0; UTSNAME_LEN],
        machine: [0; UTSNAME_LEN],
        domainname: [0; UTSNAME_LEN],
    };

    UtsName::set_str(&mut uts.sysname, "Linux");
    UtsName::set_str(&mut uts.nodename, "rCore-Tutorial");
    UtsName::set_str(&mut uts.release, "5.10.0");
    UtsName::set_str(&mut uts.version, "v0.1.0");

    #[cfg(target_arch = "riscv64")]
    UtsName::set_str(&mut uts.machine, "riscv64");
    #[cfg(target_arch = "x86_64")]
    UtsName::set_str(&mut uts.machine, "x86_64");
    #[cfg(target_arch = "loongarch64")]
    UtsName::set_str(&mut uts.machine, "loongarch64");

    UtsName::set_str(&mut uts.domainname, "localdomain");

    0
}

pub fn sys_syslog(action: usize, buf: *mut u8, len: usize) -> isize {
    match action {
        0 | 1 => 0, // open/close log
        5 => {
            // clear
            crate::logging::klog_clear();
            0
        }
        2 | 3 | 4 => {
            // read / read all / read+clear
            if buf.is_null() || len == 0 {
                return 0;
            }
            let (part1, part2) = crate::logging::klog_bytes();
            let total = part1.len() + part2.len();
            if total == 0 {
                return 0;
            }
            let copy_len = total.min(len);
            let slice = unsafe { core::slice::from_raw_parts_mut(buf, copy_len) };
            let from_p1 = part1.len().min(copy_len);
            slice[..from_p1].copy_from_slice(&part1[..from_p1]);
            if copy_len > from_p1 {
                let from_p2 = copy_len - from_p1;
                slice[from_p1..].copy_from_slice(&part2[..from_p2]);
            }
            if action == 4 {
                crate::logging::klog_clear();
            }
            copy_len as isize
        }
        9 => {
            // number of unread chars
            let (part1, part2) = crate::logging::klog_bytes();
            (part1.len() + part2.len()) as isize
        }
        _ => -22, // EINVAL
    }
}

pub fn sys_nanosleep(req_ptr: usize, rem_ptr: usize) -> isize {
    let task = crate::task::current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let ts_size = core::mem::size_of::<crate::task::TimeSpec>();
    if !inner
        .memory_set
        .valid_user_buf(req_ptr as *const u8, ts_size, false)
    {
        return -14;
    }
    if rem_ptr != 0
        && !inner
            .memory_set
            .valid_user_buf(rem_ptr as *const u8, ts_size, true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let req = *crate::mm::translated_refmut(token, req_ptr as *mut crate::task::TimeSpec);
    if req.tv_nsec >= 1_000_000_000 {
        return -22;
    }
    let duration_ns = req.tv_sec * 1_000_000_000 + req.tv_nsec as usize;
    let wakeup_time = polyhal::time::Time::now().to_nsec() as u64 + duration_ns as u64;
    loop {
        if task.inner_exclusive_access().killed {
            return -4;
        }
        {
            let inner = task.inner_exclusive_access();
            if !inner.signals.is_empty() && !inner.signal_mask.contains(inner.signals) {
                return -4;
            }
        }
        if polyhal::time::Time::now().to_nsec() as u64 >= wakeup_time {
            break;
        }
        stdout_release_if_owner();
        crate::task::suspend_current_and_run_next();
    }
    if rem_ptr != 0 {
        let rem = crate::mm::translated_refmut(token, rem_ptr as *mut crate::task::TimeSpec);
        rem.tv_sec = 0;
        rem.tv_nsec = 0;
    }
    0
}

pub fn sys_set_tid_address(tidptr: *mut i32) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    inner.tid_ptr = tidptr as usize;
    task.getpid() as isize
}

pub fn sys_exit_group(status: i32) -> ! {
    let task = current_task().unwrap();
    force_exit_task_group(&task, status);
    let tid = {
        let inner = task.inner_exclusive_access();
        inner.tid_ptr
    };
    if tid != 0 {
        *translated_refmut(current_user_token(), tid as *mut i32) = 0;
    }

    exit_current_and_run_next(status)
}

pub fn sys_getuid() -> isize {
    current_task()
        .map(|t| t.inner_exclusive_access().uid as isize)
        .unwrap_or(-1)
}

pub fn sys_geteuid() -> isize {
    current_task()
        .map(|t| t.inner_exclusive_access().euid as isize)
        .unwrap_or(-1)
}

pub fn sys_getgid() -> isize {
    current_task()
        .map(|t| t.inner_exclusive_access().gid as isize)
        .unwrap_or(-1)
}

pub fn sys_getegid() -> isize {
    current_task()
        .map(|t| t.inner_exclusive_access().egid as isize)
        .unwrap_or(-1)
}

pub fn sys_gettid() -> isize {
    current_task().map(|t| t.getpid() as isize).unwrap_or(-1)
}

pub fn sys_setuid(uid: u32) -> isize {
    if let Some(task) = current_task() {
        let mut inner = task.inner_exclusive_access();
        if inner.euid == 0 {
            inner.uid = uid;
            inner.euid = uid;
            return 0;
        }
        if uid == inner.uid || uid == inner.euid {
            inner.euid = uid;
            return 0;
        }
        -1
    } else {
        -1
    }
}

pub fn sys_setgid(gid: u32) -> isize {
    if let Some(task) = current_task() {
        let mut inner = task.inner_exclusive_access();
        if inner.egid == 0 {
            inner.gid = gid;
            inner.egid = gid;
            return 0;
        }
        if gid == inner.gid || gid == inner.egid {
            inner.egid = gid;
            return 0;
        }
        -1
    } else {
        -1
    }
}

pub fn sys_umask(mask: u32) -> isize {
    current_task()
        .map(|t| {
            let mut inner = t.inner_exclusive_access();
            let old = inner.umask;
            inner.umask = mask;
            old as isize
        })
        .unwrap_or(-1)
}

pub fn sys_getrandom(buf: *mut u8, len: usize, _flags: usize) -> isize {
    if buf.is_null() || len == 0 {
        return -1;
    }
    let token = current_user_token();
    let user_buf = crate::mm::translated_byte_buffer(token, buf, len);
    let seed = polyhal::time::Time::now().to_msec() as u64;
    for (i, byte) in user_buf.iter_mut().enumerate() {
        *byte = ((seed >> ((i % 8) * 8)) & 0xFF) as u8;
    }
    len as isize
}

// ---- poll / ppoll ----

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

const POLLIN: i16 = 0x001;
const POLLPRI: i16 = 0x002;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;
const POLLNVAL: i16 = 0x020;

pub fn sys_ppoll(
    fds: *mut u8,
    nfds: usize,
    tmo_p: *const u8,
    _sigmask: *const u8,
    _sigsetsize: usize,
) -> isize {
    let token = current_user_token();
    let buf = translated_byte_buffer(token, fds, nfds * core::mem::size_of::<PollFd>());
    let pollfds: &mut [PollFd] =
        unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut PollFd, nfds) };

    let start_ms = polyhal::time::Time::now().to_msec() as usize;

    loop {
        crate::net::net_poll();
        crate::net::net_poll();
        let mut ready = 0i32;
        let task = current_task().unwrap();
        let mut inner = task.inner_exclusive_access();

        for pfd in pollfds.iter_mut() {
            pfd.revents = 0;
            if pfd.fd < 0 {
                continue;
            }
            let fd = pfd.fd as usize;

            if fd < inner.fd_table.len() {
                if let Some(file) = &inner.fd_table[fd] {
                    let file = file.clone();
                    drop(inner);
                    let (readable, writable, error) = file.poll();
                    if (pfd.events & POLLIN) != 0 && readable {
                        pfd.revents |= POLLIN;
                    }
                    if (pfd.events & POLLOUT) != 0 && writable {
                        pfd.revents |= POLLOUT;
                    }
                    if error {
                        pfd.revents |= POLLHUP;
                    }
                    if pfd.revents != 0 {
                        ready += 1;
                    }
                    inner = task.inner_exclusive_access();
                    continue;
                }
            }

            pfd.revents |= POLLERR;
            ready += 1;
        }
        drop(inner);
        drop(task);

        if ready > 0 {
            stdout_release_if_owner();
            suspend_current_and_run_next();
            return ready as isize;
        }

        if !tmo_p.is_null() {
            let tmo: &crate::task::TimeSpec =
                translated_ref(token, tmo_p as *const crate::task::TimeSpec);
            let timeout_ms = tmo.tv_sec * 1000 + tmo.tv_nsec / 1_000_000;
            if timeout_ms == 0 {
                return 0;
            }
            let now_ms = polyhal::time::Time::now().to_msec() as usize;
            if now_ms - start_ms >= timeout_ms {
                return 0;
            }
        }

        stdout_release_if_owner();
        suspend_current_and_run_next();
    }
}

pub fn sys_mprotect(start: usize, len: usize, prot: usize) -> isize {
    if start % crate::config::PAGE_SIZE != 0 {
        return -22;
    }
    if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return -22;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    inner.memory_set.mprotect(
        polyhal::VirtAddr::from(start),
        len,
        get_map_permission(prot),
    )
}

// ---- mremap (216) ----

const MREMAP_MAYMOVE: usize = 1;

pub fn sys_mremap(
    old_addr: usize,
    old_size: usize,
    new_size: usize,
    flags: usize,
    new_addr: usize,
) -> isize {
    if old_addr == 0 || old_addr % crate::config::PAGE_SIZE != 0 || new_size == 0 {
        return -22;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    let round_old = (old_size + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE
        * crate::config::PAGE_SIZE;
    let round_new = (new_size + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE
        * crate::config::PAGE_SIZE;
    let old_end = old_addr + round_old;

    let target_addr = if (flags & MREMAP_MAYMOVE) != 0 && round_new > round_old {
        if new_addr != 0 {
            new_addr
        } else {
            let free = inner.memory_set.find_free_area(round_new);
            let max_end: usize = inner.memory_set.max_end_vaddr().into();
            (max_end + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE
                * crate::config::PAGE_SIZE
        }
    } else {
        old_addr
    };

    let new_end = target_addr + round_new;

    if (flags & MREMAP_MAYMOVE) != 0 && target_addr != old_addr {
        if inner.memory_set.is_overlap(
            polyhal::VirtAddr::from(target_addr),
            polyhal::VirtAddr::from(new_end),
        ) {
            return -12;
        }
    }

    inner.memory_set.munmap(
        polyhal::VirtAddr::from(old_addr),
        polyhal::VirtAddr::from(old_end),
    );

    let perm =
        crate::mm::MapPermission::R | crate::mm::MapPermission::W | crate::mm::MapPermission::U;
    inner.memory_set.insert_framed_area(
        polyhal::VirtAddr::from(target_addr),
        polyhal::VirtAddr::from(new_end),
        perm,
    );
    target_addr as isize
}

// ---- mincore (232) ----

pub fn sys_mincore(start: usize, len: usize, vec: *mut u8) -> isize {
    if start % crate::config::PAGE_SIZE != 0 || len == 0 {
        return -22;
    }
    let npages = (len + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE;
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    // Validate vec pointer
    if !inner.memory_set.valid_user_buf(vec, npages, true) {
        return -14;
    }
    // Check that the address range is mapped
    let mut vpn = start;
    let end = start + len;
    if end < start {
        return -22;
    }
    while vpn < end {
        if inner
            .memory_set
            .translate(polyhal::VirtAddr::from(vpn))
            .is_none()
        {
            return -12;
        }
        vpn += crate::config::PAGE_SIZE;
    }
    drop(inner);
    let token = current_user_token();
    let buf = crate::mm::translated_byte_buffer(token, vec, npages);
    for b in buf.iter_mut() {
        *b = 1;
    }
    0
}

// ---- mlock/mlock2/mlockall/munlock/munlockall (228-231) ----

pub fn sys_mlock(_start: usize, _len: usize) -> isize {
    0
}

pub fn sys_munlock(_start: usize, _len: usize) -> isize {
    0
}

pub fn sys_mlockall(_flags: usize) -> isize {
    0
}

pub fn sys_munlockall() -> isize {
    0
}

pub fn sys_mlock2(_start: usize, _len: usize, _flags: usize) -> isize {
    0
}

// ---- getrlimit/setrlimit (163/164) ----

const RLIMIT_NOFILE: usize = 7;
const RLIMIT_STACK: usize = 3;
const RLIMIT_AS: usize = 9;
const RLIM_NLIMITS: usize = 16;
const RLIM_INFINITY: usize = usize::MAX;

#[repr(C)]
struct RLimit {
    rlim_cur: usize,
    rlim_max: usize,
}

pub fn sys_getrlimit(resource: usize, rlim_ptr: *mut u8) -> isize {
    if rlim_ptr.is_null() || resource >= RLIM_NLIMITS {
        return -22;
    }
    let token = current_user_token();
    let rlim = crate::mm::translated_refmut(token, rlim_ptr as *mut RLimit);
    if resource == RLIMIT_NOFILE {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        rlim.rlim_cur = inner.rlimit_nofile;
        rlim.rlim_max = inner.rlimit_nofile_max;
    } else {
        rlim.rlim_cur = RLIM_INFINITY;
        rlim.rlim_max = RLIM_INFINITY;
    }
    0
}

pub fn sys_setrlimit(resource: usize, rlim_ptr: *const u8) -> isize {
    if rlim_ptr.is_null() || resource >= RLIM_NLIMITS {
        return -22;
    }
    if resource == RLIMIT_NOFILE {
        let token = current_user_token();
        let rlim = translated_ref(token, rlim_ptr as *const RLimit);
        let task = current_task().unwrap();
        let mut inner = task.inner_exclusive_access();
        inner.rlimit_nofile = rlim.rlim_cur.min(1024);
        inner.rlimit_nofile_max = rlim.rlim_max.min(1024);
    }
    0
}

pub fn sys_prlimit64(
    pid: usize,
    resource: usize,
    new_limit: *const u8,
    old_limit: *mut u8,
) -> isize {
    if resource >= RLIM_NLIMITS {
        return -22;
    }
    let task = if pid == 0 {
        current_task().unwrap()
    } else {
        match pid2task(pid) {
            Some(task) => task,
            None => return -3,
        }
    };
    let token = current_user_token();
    if !old_limit.is_null() {
        let old = translated_refmut(token, old_limit as *mut RLimit);
        if resource == RLIMIT_NOFILE {
            let inner = task.inner_exclusive_access();
            old.rlim_cur = inner.rlimit_nofile;
            old.rlim_max = inner.rlimit_nofile_max;
        } else {
            old.rlim_cur = RLIM_INFINITY;
            old.rlim_max = RLIM_INFINITY;
        }
    }
    if !new_limit.is_null() && resource == RLIMIT_NOFILE {
        let new = translated_ref(token, new_limit as *const RLimit);
        let mut inner = task.inner_exclusive_access();
        inner.rlimit_nofile = new.rlim_cur.min(1024);
        inner.rlimit_nofile_max = new.rlim_max.min(1024);
    }
    0
}

// ---- sysinfo (179) ----

const SI_LOAD_SHIFT: usize = 16;

#[repr(C)]
struct SysInfo {
    uptime: isize,
    loads: [usize; 3],
    totalram: usize,
    freeram: usize,
    sharedram: usize,
    bufferram: usize,
    totalswap: usize,
    freeswap: usize,
    procs: u16,
    pad: u16,
    totalhigh: usize,
    freehigh: usize,
    mem_unit: u32,
    _f: [u8; 0],
}

pub fn sys_sysinfo(info_ptr: *mut u8) -> isize {
    if info_ptr.is_null() {
        return -14;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(info_ptr, core::mem::size_of::<SysInfo>(), true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let info = crate::mm::translated_refmut(token, info_ptr as *mut SysInfo);
    let now = polyhal::time::Time::now().to_msec() as isize / 1000;
    info.uptime = now;
    info.loads = [0, 0, 0];
    info.totalram = 256 * 1024 * 1024;
    info.freeram = 128 * 1024 * 1024;
    info.sharedram = 0;
    info.bufferram = 0;
    info.totalswap = 0;
    info.freeswap = 0;
    info.procs = 0;
    info.pad = 0;
    info.totalhigh = 0;
    info.freehigh = 0;
    info.mem_unit = 1;
    0
}

// ---- sigaltstack (132) ----

const SS_DISABLE: u32 = 2;
const SS_ONSTACK: u32 = 1;

#[repr(C)]
struct StackT {
    ss_sp: usize,
    ss_flags: i32,
    ss_size: usize,
}

pub fn sys_sigaltstack(ss: *const u8, old_ss: *mut u8) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    if !old_ss.is_null() {
        let old = crate::mm::translated_refmut(token, old_ss as *mut StackT);
        old.ss_sp = inner.alt_stack;
        old.ss_size = inner.alt_stack_size;
        old.ss_flags = inner.alt_stack_flags as i32;
        if inner.alt_stack_flags & SS_ONSTACK != 0 {
            old.ss_flags = SS_ONSTACK as i32;
        }
    }

    if ss.is_null() {
        return 0;
    }

    let new = crate::mm::translated_ref(token, ss as *const StackT);
    if (new.ss_flags as u32 & SS_DISABLE) != 0 {
        inner.alt_stack_flags = SS_DISABLE;
        inner.alt_stack = 0;
        inner.alt_stack_size = 0;
    } else {
        if new.ss_size < 2048 {
            return -22;
        }
        inner.alt_stack = new.ss_sp;
        inner.alt_stack_size = new.ss_size;
        inner.alt_stack_flags = 0;
    }
    0
}

// ---- sigpending / rt_sigpending (136) ----

pub fn sys_rt_sigpending(set: *mut u8, _sigsize: usize) -> isize {
    if set.is_null() {
        return -14;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let dst = crate::mm::translated_refmut(token, set as *mut u64);
    *dst = flags_to_user_sigset(inner.signals);
    0
}

// ---- sigsuspend / rt_sigsuspend (133) ----

pub fn sys_rt_sigsuspend(mask: *const u8, _sigsize: usize) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    let saved_mask = inner.signal_mask;
    let new_mask_u64 = *crate::mm::translated_ref(token, mask as *const u64);
    inner.signal_mask = user_sigset_to_flags(new_mask_u64);
    drop(inner);

    loop {
        stdout_release_if_owner();
        crate::task::suspend_current_and_run_next();

        let mut inner2 = task.inner_exclusive_access();
        if inner2.killed {
            inner2.signal_mask = saved_mask;
            return -4;
        }
        if !inner2.signals.is_empty() {
            inner2.signal_mask = saved_mask;
            return -4;
        }
    }
}

fn signal_number(signal: SignalFlags) -> i32 {
    for sig in 1..=MAX_SIG {
        if let Some(flag) = SignalFlags::from_bits(1u32 << sig) {
            if flag == signal {
                return sig as i32;
            }
        }
    }
    0
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RtSignalInfo {
    si_signo: i32,
    si_errno: i32,
    si_code: i32,
    _pad0: i32,
    si_pid: i32,
    si_uid: u32,
    si_status: i32,
    _pad1: i32,
    si_utime: isize,
    si_stime: isize,
    _pad: [u8; 128 - 40],
}

fn write_signal_siginfo(info_ptr: *mut u8, signum: i32) {
    if info_ptr.is_null() {
        return;
    }
    let token = current_user_token();
    *translated_refmut(token, info_ptr as *mut RtSignalInfo) = RtSignalInfo {
        si_signo: signum,
        si_errno: 0,
        si_code: 0,
        _pad0: 0,
        si_pid: 0,
        si_uid: 0,
        si_status: 0,
        _pad1: 0,
        si_utime: 0,
        si_stime: 0,
        _pad: [0; 128 - 40],
    };
}

pub fn sys_rt_sigtimedwait(
    set: *const u8,
    info: *mut u8,
    timeout: *const u8,
    _sigsetsize: usize,
) -> isize {
    if set.is_null() {
        return -14;
    }
    let token = current_user_token();
    let raw_set = *translated_ref(token, set as *const u64);
    let mut wanted = user_sigset_to_flags(raw_set);
    wanted.remove(SignalFlags::SIGKILL | SignalFlags::SIGSTOP);
    if wanted.is_empty() {
        return -22;
    }

    let timeout_ns = if timeout.is_null() {
        None
    } else {
        let ts = *translated_ref(token, timeout as *const crate::task::TimeSpec);
        if ts.tv_nsec >= 1_000_000_000 {
            return -22;
        }
        Some(
            ts.tv_sec
                .saturating_mul(1_000_000_000)
                .saturating_add(ts.tv_nsec),
        )
    };
    let start_ns = Time::now().to_nsec() as usize;

    loop {
        let task = current_task().unwrap();
        let found = {
            let mut inner = task.inner_exclusive_access();
            if inner.killed {
                return -4;
            }
            let pending = inner.signals & wanted;
            let mut selected = None;
            for sig in 1..=MAX_SIG {
                if let Some(flag) = SignalFlags::from_bits(1u32 << sig) {
                    if pending.contains(flag) {
                        inner.signals.remove(flag);
                        selected = Some(flag);
                        break;
                    }
                }
            }
            selected
        };

        if let Some(signal) = found {
            let signum = signal_number(signal);
            write_signal_siginfo(info, signum);
            return signum as isize;
        }

        if let Some(limit_ns) = timeout_ns {
            if limit_ns == 0 {
                return -11;
            }
            let now_ns = Time::now().to_nsec() as usize;
            if now_ns.saturating_sub(start_ns) >= limit_ns {
                return -11;
            }
        }

        stdout_release_if_owner();
        suspend_current_and_run_next();
    }
}

// ---- alarm (no separate syscall on RISC-V, musl uses setitimer) ----

// ---- clock_getres (114) ----

pub fn sys_clock_getres(_clk_id: usize, res: *mut u8) -> isize {
    if res.is_null() {
        return -14;
    }
    let token = current_user_token();
    let ts = crate::mm::translated_refmut(token, res as *mut crate::task::TimeSpec);
    ts.tv_sec = 0;
    ts.tv_nsec = 1_000_000; // 1ms resolution
    0
}

// ---- getgroups/setgroups (158/159) ----

pub fn sys_getgroups(_size: usize, _list: *mut u32) -> isize {
    0
}

pub fn sys_setgroups(_size: usize, _list: *const u32) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if inner.euid != 0 {
        return -1;
    }
    0
}

// ---- UID/GID: getresuid(117)/getresgid(120)/setresuid(147)/setresgid(150)/setreuid(145)/setregid(143) ----

pub fn sys_getresuid(ruid: *mut u32, euid: *mut u32, suid: *mut u32) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    *crate::mm::translated_refmut(token, ruid) = inner.uid;
    *crate::mm::translated_refmut(token, euid) = inner.euid;
    *crate::mm::translated_refmut(token, suid) = inner.euid;
    0
}

pub fn sys_getresgid(rgid: *mut u32, egid: *mut u32, sgid: *mut u32) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    *crate::mm::translated_refmut(token, rgid) = inner.gid;
    *crate::mm::translated_refmut(token, egid) = inner.egid;
    *crate::mm::translated_refmut(token, sgid) = inner.egid;
    0
}

pub fn sys_setresuid(ruid: usize, euid: usize, suid: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let old_euid = inner.euid;
    if old_euid == 0 {
    } else if ruid != usize::MAX && ruid != inner.uid as usize && ruid != inner.euid as usize {
        return -1;
    } else if euid != usize::MAX && euid != inner.uid as usize && euid != inner.euid as usize {
        return -1;
    } else if suid != usize::MAX && suid != inner.uid as usize && suid != inner.euid as usize {
        return -1;
    }
    if ruid != usize::MAX {
        inner.uid = ruid as u32;
    }
    if euid != usize::MAX {
        inner.euid = euid as u32;
    }
    0
}

pub fn sys_setresgid(rgid: usize, egid: usize, sgid: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let old_egid = inner.egid;
    if old_egid == 0 {
    } else if rgid != usize::MAX && rgid != inner.gid as usize && rgid != inner.egid as usize {
        return -1;
    } else if egid != usize::MAX && egid != inner.gid as usize && egid != inner.egid as usize {
        return -1;
    } else if sgid != usize::MAX && sgid != inner.gid as usize && sgid != inner.egid as usize {
        return -1;
    }
    if rgid != usize::MAX {
        inner.gid = rgid as u32;
    }
    if egid != usize::MAX {
        inner.egid = egid as u32;
    }
    0
}

pub fn sys_setreuid(ruid: usize, euid: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let old_euid = inner.euid;
    if old_euid != 0 {
        if ruid != usize::MAX && ruid != inner.uid as usize && ruid != inner.euid as usize {
            return -1;
        }
        if euid != usize::MAX && euid != inner.uid as usize && euid != inner.euid as usize {
            return -1;
        }
    }
    if ruid != usize::MAX {
        inner.uid = ruid as u32;
        if euid == usize::MAX {
            inner.euid = ruid as u32;
        }
    }
    if euid != usize::MAX {
        inner.euid = euid as u32;
    }
    0
}

pub fn sys_setregid(rgid: usize, egid: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let old_egid = inner.egid;
    if old_egid != 0 {
        if rgid != usize::MAX && rgid != inner.gid as usize && rgid != inner.egid as usize {
            return -1;
        }
        if egid != usize::MAX && egid != inner.gid as usize && egid != inner.egid as usize {
            return -1;
        }
    }
    if rgid != usize::MAX {
        inner.gid = rgid as u32;
        if egid == usize::MAX {
            inner.egid = rgid as u32;
        }
    }
    if egid != usize::MAX {
        inner.egid = egid as u32;
    }
    0
}

// ---- sched_setaffinity/sched_getaffinity (122/123) ----

pub fn sys_sched_setaffinity(_pid: usize, _len: usize, _mask: *const u8) -> isize {
    0
}

pub fn sys_sched_getaffinity(_pid: usize, _len: usize, _mask: *mut u8) -> isize {
    0
}

// ---- getpriority/setpriority (140/141) ----

pub fn sys_getpriority(_which: usize, _who: usize) -> isize {
    20
}

pub fn sys_setpriority(_which: usize, _who: usize, _prio: isize) -> isize {
    0
}

// ---- pselect6 ----

const FD_SETSIZE: usize = 1024;
const FD_SET_LONGS: usize = FD_SETSIZE / 64;
type FdSet = [u64; FD_SET_LONGS];

fn fd_isset(set: &FdSet, fd: usize) -> bool {
    if fd >= FD_SETSIZE {
        return false;
    }
    (set[fd / 64] & (1u64 << (fd % 64))) != 0
}

fn fd_set(set: &mut FdSet, fd: usize) {
    if fd < FD_SETSIZE {
        set[fd / 64] |= 1u64 << (fd % 64);
    }
}

fn fd_clr(set: &mut FdSet, fd: usize) {
    if fd < FD_SETSIZE {
        set[fd / 64] &= !(1u64 << (fd % 64));
    }
}

fn copy_fdset(dst: &mut FdSet, src_ptr: *const u8) {
    let token = current_user_token();
    let src: &FdSet = translated_ref(token, src_ptr as *const FdSet);
    *dst = *src;
}

fn write_fdset(dst_ptr: *mut u8, src: &FdSet) {
    let token = current_user_token();
    *translated_refmut(token, dst_ptr as *mut FdSet) = *src;
}

pub fn sys_pselect6(
    nfds: usize,
    readfds: *mut u8,
    writefds: *mut u8,
    exceptfds: *mut u8,
    timeout: *const u8,
    _sigmask: *const u8,
) -> isize {
    if nfds > FD_SETSIZE {
        return -22;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let fdset_size = core::mem::size_of::<FdSet>();
    if !readfds.is_null() && !inner.memory_set.valid_user_buf(readfds, fdset_size, true) {
        return -14;
    }
    if !writefds.is_null() && !inner.memory_set.valid_user_buf(writefds, fdset_size, true) {
        return -14;
    }
    if !exceptfds.is_null() && !inner.memory_set.valid_user_buf(exceptfds, fdset_size, true) {
        return -14;
    }
    if !timeout.is_null()
        && !inner.memory_set.valid_user_buf(
            timeout,
            core::mem::size_of::<crate::task::TimeSpec>(),
            false,
        )
    {
        return -14;
    }
    drop(inner);

    let token = current_user_token();
    let mut rfds = [0u64; FD_SET_LONGS];
    let mut wfds = [0u64; FD_SET_LONGS];
    let mut efds = [0u64; FD_SET_LONGS];

    if !readfds.is_null() {
        copy_fdset(&mut rfds, readfds);
    }
    if !writefds.is_null() {
        copy_fdset(&mut wfds, writefds);
    }
    if !exceptfds.is_null() {
        copy_fdset(&mut efds, exceptfds);
    }
    let orig_rfds = rfds;
    let orig_wfds = wfds;
    let orig_efds = efds;

    let timeout_ms = if !timeout.is_null() {
        let ts: &crate::task::TimeSpec =
            translated_ref(token, timeout as *const crate::task::TimeSpec);
        ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
    } else {
        usize::MAX
    };

    let start_ms = polyhal::time::Time::now().to_msec() as usize;

    loop {
        rfds = orig_rfds;
        wfds = orig_wfds;
        efds = orig_efds;
        crate::net::net_poll();
        crate::net::net_poll();
        let mut ready = 0i32;
        let n = nfds.min(FD_SETSIZE);

        let task = current_task().unwrap();
        let mut inner = task.inner_exclusive_access();

        let mut fd = 0usize;
        while fd < n {
            let check_read = fd_isset(&rfds, fd);
            let check_write = fd_isset(&wfds, fd);
            let check_except = fd_isset(&efds, fd);

            if !check_read && !check_write && !check_except {
                fd += 1;
                continue;
            }

            let mut readable = false;
            let mut writable = false;
            let mut error = false;

            if fd < inner.fd_table.len() {
                if let Some(file) = &inner.fd_table[fd] {
                    let file = file.clone();
                    drop(inner);
                    let (r, w, e) = file.poll();
                    readable = r;
                    writable = w;
                    error = e;
                    inner = task.inner_exclusive_access();
                } else {
                    drop(inner);
                    return -9;
                }
            } else {
                drop(inner);
                return -9;
            }

            if check_read && readable {
                fd_set(&mut rfds, fd);
                ready += 1;
            } else {
                fd_clr(&mut rfds, fd);
            }
            if check_write && writable {
                fd_set(&mut wfds, fd);
                ready += 1;
            } else {
                fd_clr(&mut wfds, fd);
            }
            if check_except && error {
                fd_set(&mut efds, fd);
                ready += 1;
            } else {
                fd_clr(&mut efds, fd);
            }

            fd += 1;
        }

        drop(inner);
        drop(task);

        if ready > 0 {
            if !readfds.is_null() {
                write_fdset(readfds, &rfds);
            }
            if !writefds.is_null() {
                write_fdset(writefds, &wfds);
            }
            if !exceptfds.is_null() {
                write_fdset(exceptfds, &efds);
            }
            stdout_release_if_owner();
            suspend_current_and_run_next();
            return ready as isize;
        }

        if timeout_ms == 0 {
            if !readfds.is_null() {
                write_fdset(readfds, &rfds);
            }
            if !writefds.is_null() {
                write_fdset(writefds, &wfds);
            }
            if !exceptfds.is_null() {
                write_fdset(exceptfds, &efds);
            }
            return 0;
        }
        if timeout_ms != usize::MAX {
            let now_ms = polyhal::time::Time::now().to_msec() as usize;
            if now_ms - start_ms >= timeout_ms {
                if !readfds.is_null() {
                    write_fdset(readfds, &rfds);
                }
                if !writefds.is_null() {
                    write_fdset(writefds, &wfds);
                }
                if !exceptfds.is_null() {
                    write_fdset(exceptfds, &efds);
                }
                return 0;
            }
        }

        stdout_release_if_owner();
        suspend_current_and_run_next();
    }
}

pub fn sys_futex(
    uaddr: *mut u32,
    futex_op: usize,
    val: usize,
    _val2: usize,
    _uaddr2: *mut u32,
    _val3: usize,
) -> isize {
    let token = current_user_token();
    let cur = *translated_ref(token, uaddr);
    let op = futex_op & 0x0F;
    match op {
        0 | 9 => {
            if cur == val as u32 {
                stdout_release_if_owner();
                suspend_current_and_run_next();
                0
            } else {
                -11
            }
        }
        1 | 10 => 0,
        6..=8 | 11..=13 => 0,
        _ => -38,
    }
}

pub fn sys_madvise(_start: usize, _len: usize, _advice: usize) -> isize {
    0
}

const ITIMER_REAL: usize = 0;
const ITIMER_VIRTUAL: usize = 1;
const ITIMER_PROF: usize = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct ITimerVal {
    it_interval: TimeVal,
    it_value: TimeVal,
}

pub fn sys_getitimer(which: usize, curr_value_ptr: *mut u8) -> isize {
    if which != ITIMER_REAL {
        return -22;
    }
    if curr_value_ptr.is_null() {
        return -14;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(curr_value_ptr, core::mem::size_of::<ITimerVal>(), true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    let now = polyhal::time::Time::now().to_usec() as u64;
    let remaining_usec = if inner.itimer_expire > now {
        inner.itimer_expire - now
    } else {
        0
    };

    let itv = if inner.itimer_expire == 0 {
        ITimerVal {
            it_interval: TimeVal { sec: 0, usec: 0 },
            it_value: TimeVal { sec: 0, usec: 0 },
        }
    } else {
        let old_interval_sec = (inner.itimer_interval_us / 1_000_000) as isize;
        let old_interval_usec = (inner.itimer_interval_us % 1_000_000) as isize;
        let old_rem_sec = (remaining_usec / 1_000_000) as isize;
        let old_rem_usec = (remaining_usec % 1_000_000) as isize;
        ITimerVal {
            it_interval: TimeVal {
                sec: old_interval_sec,
                usec: old_interval_usec,
            },
            it_value: TimeVal {
                sec: old_rem_sec,
                usec: old_rem_usec,
            },
        }
    };
    drop(inner);
    *translated_refmut(token, curr_value_ptr as *mut ITimerVal) = itv;
    0
}

pub fn sys_setitimer(which: usize, new_value: *const u8, old_value: *mut u8) -> isize {
    if which > ITIMER_PROF {
        return -22;
    }

    let token = current_user_token();
    let task = current_task().unwrap();
    let now = polyhal::time::Time::now().to_usec() as u64;

    if !old_value.is_null() {
        let mut inner = task.inner_exclusive_access();
        let remaining_usec = if inner.itimer_expire > now {
            inner.itimer_expire - now
        } else {
            0
        };
        let old_interval_sec = (inner.itimer_interval_us / 1_000_000) as isize;
        let old_interval_usec = (inner.itimer_interval_us % 1_000_000) as isize;
        let old_rem_sec = (remaining_usec / 1_000_000) as isize;
        let old_rem_usec = (remaining_usec % 1_000_000) as isize;
        let old_itv = if inner.itimer_expire == 0 {
            ITimerVal {
                it_interval: TimeVal { sec: 0, usec: 0 },
                it_value: TimeVal { sec: 0, usec: 0 },
            }
        } else {
            ITimerVal {
                it_interval: TimeVal {
                    sec: old_interval_sec,
                    usec: old_interval_usec,
                },
                it_value: TimeVal {
                    sec: old_rem_sec,
                    usec: old_rem_usec,
                },
            }
        };
        drop(inner);
        *translated_refmut(token, old_value as *mut ITimerVal) = old_itv;
    }

    if new_value.is_null() {
        return 0;
    }

    let new_itv: &ITimerVal = translated_ref(token, new_value as *const ITimerVal);
    let new_value_usec = new_itv.it_value.sec * 1_000_000 + new_itv.it_value.usec;
    let new_interval_usec = new_itv.it_interval.sec * 1_000_000 + new_itv.it_interval.usec;

    let mut inner = task.inner_exclusive_access();
    if new_value_usec == 0 {
        inner.itimer_expire = 0;
    } else {
        inner.itimer_expire = now + new_value_usec as u64;
    }
    inner.itimer_interval_us = new_interval_usec as u64;
    0
}

pub fn sys_clock_gettime(clk_id: usize, ts: *mut u8) -> isize {
    const CLOCK_REALTIME: usize = 0;
    const CLOCK_MONOTONIC: usize = 1;

    if ts.is_null() || (clk_id != CLOCK_REALTIME && clk_id != CLOCK_MONOTONIC) {
        return -22;
    }

    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(ts, core::mem::size_of::<crate::task::TimeSpec>(), true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let now_us = monotonic_user_time_us();
    let ts_out: &mut crate::task::TimeSpec =
        translated_refmut(token, ts as *mut crate::task::TimeSpec);
    ts_out.tv_sec = (now_us / 1_000_000) as usize;
    ts_out.tv_nsec = ((now_us % 1_000_000) * 1000) as usize;
    0
}

pub fn sys_clock_nanosleep(_clk_id: usize, _flags: usize, req: usize, rem: usize) -> isize {
    sys_nanosleep(req, rem)
}

pub fn sys_readv(fd: usize, iov: *const u8, iovcnt: usize) -> isize {
    use crate::mm::translated_ref;
    let token = current_user_token();
    #[repr(C)]
    struct IoVec {
        iov_base: usize,
        iov_len: usize,
    }
    let mut total = 0isize;
    for i in 0..iovcnt {
        let iov_ptr = unsafe { iov.add(i * core::mem::size_of::<IoVec>()) };
        let iov: &IoVec = translated_ref(token, iov_ptr as *const IoVec);
        if iov.iov_len == 0 {
            continue;
        }
        let buf = crate::mm::translated_byte_buffer(token, iov.iov_base as *mut u8, iov.iov_len);
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
            return -1;
        }
        let file = inner.fd_table[fd].as_ref().unwrap().clone();
        drop(inner);
        let n = file.read(buf);
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        total += n as isize;
        if (n as usize) < iov.iov_len {
            break;
        }
    }
    total
}

pub fn sys_writev(fd: usize, iov: *const u8, iovcnt: usize) -> isize {
    use crate::mm::translated_ref;
    let token = current_user_token();
    #[repr(C)]
    struct IoVec {
        iov_base: usize,
        iov_len: usize,
    }
    let mut total = 0isize;
    for i in 0..iovcnt {
        let iov_ptr = unsafe { iov.add(i * core::mem::size_of::<IoVec>()) };
        let iov: &IoVec = translated_ref(token, iov_ptr as *const IoVec);
        if iov.iov_len == 0 {
            continue;
        }
        let buf = crate::mm::translated_byte_buffer(token, iov.iov_base as *mut u8, iov.iov_len);
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
            return -1;
        }
        let file = inner.fd_table[fd].as_ref().unwrap().clone();
        drop(inner);
        let n = file.write(buf);
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        total += n as isize;
        if (n as usize) < iov.iov_len {
            break;
        }
    }
    total
}

pub fn sys_sendfile(out_fd: usize, in_fd: usize, offset_ptr: *mut usize, count: usize) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    if in_fd >= inner.fd_table.len() || inner.fd_table[in_fd].is_none() {
        return -9;
    }
    if out_fd >= inner.fd_table.len() || inner.fd_table[out_fd].is_none() {
        return -9;
    }

    let in_file = inner.fd_table[in_fd].as_ref().unwrap().clone();
    let out_file = inner.fd_table[out_fd].as_ref().unwrap().clone();

    if !in_file.readable() || !out_file.writable() {
        return -9;
    }

    let saved_in_off = if !offset_ptr.is_null() {
        let off = in_file.get_offset();
        let user_off = *translated_ref(token, offset_ptr);
        in_file.seek(user_off);
        Some(off)
    } else {
        None
    };

    drop(inner);

    let mut total: isize = 0;
    let mut buf = [0u8; 4096];
    while (total as usize) < count {
        let chunk = (count - total as usize).min(buf.len());
        let n = in_file.read(&mut buf[..chunk]);
        if n <= 0 {
            break;
        }
        let written = out_file.write(&mut buf[..n as usize]);
        if written <= 0 {
            break;
        }
        total += written;
        if written < n {
            break;
        }
    }

    if let Some(off) = saved_in_off {
        in_file.seek(off);
    }

    total
}

pub fn sys_getrusage(who: usize, usage: *mut u8) -> isize {
    const RUSAGE_SELF: usize = 0;
    const RUSAGE_CHILDREN: usize = usize::MAX;
    if who != RUSAGE_SELF && who != RUSAGE_CHILDREN {
        return -22;
    }
    if usage.is_null() {
        return -14;
    }
    let task = current_task().unwrap();
    task.record_time_suspend();
    task.record_time_resume();
    let inner = task.inner_exclusive_access();
    let (utime, stime) = if who == RUSAGE_SELF {
        (inner.utime, inner.stime)
    } else {
        (inner.cutime, inner.cstime)
    };
    let token = inner.get_user_token();
    drop(inner);
    let rusage = RUsage {
        ru_utime: nsec_to_timeval(utime),
        ru_stime: nsec_to_timeval(stime),
        ru_maxrss: 0,
        ru_ixrss: 0,
        ru_idrss: 0,
        ru_isrss: 0,
        ru_minflt: 0,
        ru_majflt: 0,
        ru_nswap: 0,
        ru_inblock: 0,
        ru_oublock: 0,
        ru_msgsnd: 0,
        ru_msgrcv: 0,
        ru_nsignals: 0,
        ru_nvcsw: 0,
        ru_nivcsw: 0,
    };
    *translated_refmut(token, usage as *mut RUsage) = rusage;
    0
}
