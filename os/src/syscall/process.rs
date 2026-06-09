use crate::fs::{open_file, OpenFlags, File};
use crate::fs::stdio::stdout_release_if_owner;
use crate::mm::{
    translated_byte_buffer, translated_ref, translated_refmut, translated_str, MapPermission,
    MapType,
};
use crate::task::{
    add_task, block_current_and_run_next, current_task, current_user_token,
    exit_current_and_run_next, has_pending_signals, pid2task, pid_alloc, sleep_current_and_run_next, suspend_current_and_run_next,
    BlockReason, SignalAction, SignalFlags, MAX_SIG,
    blank_kcontext, KernelStack, TaskControlBlock, TaskControlBlockInner, TaskStatus,
};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use log::info;
use polyhal::kcontext::KContextArgs;
use polyhal::time::Time;
use polyhal_trap::trapframe::TrapFrameArgs;

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
    pub sec: u64,
    pub usec: u64,
}

pub fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

pub fn sys_yield() -> isize {
    suspend_current_and_run_next();
    0
}

pub fn sys_get_time(ts_ptr: usize, _tz: usize) -> isize {
    let now_ms = Time::now().to_msec() as u64;
    if ts_ptr != 0 {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        if !inner.memory_set.valid_user_buf(ts_ptr as *const u8, core::mem::size_of::<TimeVal>(), true) {
            return -14;
        }
        let token = inner.get_user_token();
        drop(inner);
        let ts = crate::mm::translated_refmut(token, ts_ptr as *mut TimeVal);
        ts.sec = now_ms / 1000;
        ts.usec = (now_ms % 1000) * 1000;
        0
    } else {
        now_ms as isize
    }
}

pub fn sys_getpid() -> isize {
    current_task().unwrap().pid.0 as isize
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

pub fn sys_setsid() -> isize {
    current_task().map(|t| t.getpid() as isize).unwrap_or(-1)
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
        if flags & CLONE_CHILD_CLEARTID != 0 && ctid != 0 {
            inner.tid_ptr = ctid;
        }
        inner.memory_set.token()
    };

    if flags & CLONE_CHILD_SETTID != 0 && ctid != 0 {
        *translated_refmut(child_token, ctid as *mut i32) = new_pid as i32;
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

pub fn sys_exec(path: *const u8, mut args: *const usize) -> isize {
    let token = current_user_token();
    let path = translated_str(token, path);
    let mut args_vec: Vec<String> = Vec::new();
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
    let app_inode = open_file(path.as_str(), OpenFlags::RDONLY).or_else(|| {
        if path.starts_with('/') {
            None
        } else {
            open_file(format!("/bin/{}", path).as_str(), OpenFlags::RDONLY)
        }
    });
    if let Some(app_inode) = app_inode {
        let all_data = app_inode.read_all();
        let task = current_task().unwrap();
        task.exec(all_data.as_slice(), args_vec);
        0
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -1.
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
            zombie_idx.map(|idx| (inner.children.remove(idx), inner.memory_set.token()))
        };

        if let Some((child, token)) = zombie {
            let found_pid = child.getpid();
            let exit_code = {
                let inner = child.inner_exclusive_access();
                inner.exit_code
            };
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
        if has_pending_signals() {
            return -4;
        }
        crate::task::sleep_current_and_run_next();
        // Loop back to check again
    }
}

pub fn sys_kill(pid: usize, signum: i32) -> isize {
    if signum < 0 || signum as usize > MAX_SIG {
        return -22; // EINVAL
    }
    let target = if pid == 0 || pid == usize::MAX {
        current_task().map(|t| t.getpid()).unwrap_or(0)
    } else if (pid as isize) < 0 {
        (-(pid as isize)) as usize
    } else {
        pid
    };
    if let Some(task) = pid2task(target) {
        if signum == 0 {
            return 0;
        }
        let flag = SignalFlags::from_bits(1u32 << signum as usize).unwrap();
        let mut task_ref = task.inner_exclusive_access();
        if task_ref.signals.contains(flag) {
            return 0;
        }
        task_ref.signals.insert(flag);
        if flag == SignalFlags::SIGTERM || flag == SignalFlags::SIGKILL {
            task_ref.killed = true;
            task_ref.exit_signal = -(signum as i32);
        } else if task_ref.signal_actions.table[signum as usize].handler == 0 {
            let fatal = flag.intersects(
                SignalFlags::SIGSEGV | SignalFlags::SIGABRT | SignalFlags::SIGILL
                | SignalFlags::SIGFPE | SignalFlags::SIGBUS | SignalFlags::SIGSYS
                | SignalFlags::SIGTRAP
                | SignalFlags::SIGALRM | SignalFlags::SIGVTALRM | SignalFlags::SIGPROF
                | SignalFlags::SIGTERM | SignalFlags::SIGINT | SignalFlags::SIGQUIT
                | SignalFlags::SIGHUP | SignalFlags::SIGPIPE | SignalFlags::SIGUSR1
                | SignalFlags::SIGUSR2
                | SignalFlags::SIGXCPU | SignalFlags::SIGXFSZ
                | SignalFlags::SIGIO | SignalFlags::SIGPWR,
            );
            if fatal {
                task_ref.killed = true;
                task_ref.exit_signal = -(signum as i32);
            }
        }
        0
    } else {
        0 // kill to non-existent pid: silently succeed (POSIX)
    }
}

pub fn sys_sigprocmask(how: usize, set: *const u8, oldset: *mut u8, _sigsetsize: usize) -> isize {
    const SIG_BLOCK: usize = 0;
    const SIG_UNBLOCK: usize = 1;
    const SIG_SETMASK: usize = 2;
    if let Some(task) = current_task() {
        let token = current_user_token();
        let mut inner = task.inner_exclusive_access();
        let old_mask = inner.signal_mask;
        if !oldset.is_null() {
            *crate::mm::translated_refmut(token, oldset as *mut u64) = old_mask.bits() as u64;
        }
        if set.is_null() {
            return 0;
        }
        let new_mask = *crate::mm::translated_ref(token, set as *const u64);
        let new_flags = SignalFlags::from_bits_truncate(new_mask as u32);
        match how {
            SIG_BLOCK => inner.signal_mask |= new_flags,
            SIG_UNBLOCK => inner.signal_mask &= !new_flags,
            _ => inner.signal_mask = new_flags,
        }
        0
    } else {
        -1
    }
}

pub fn sys_sigreturn() -> isize {
    if let Some(task) = current_task() {
        let mut inner = task.inner_exclusive_access();
        inner.handling_sig = -1;
        inner.signal_mask = inner.saved_signal_mask;
        // restore the trap context
        let trap_ctx = inner.get_trap_cx();
        *trap_ctx = inner.trap_ctx_backup.clone().unwrap();
        // Here we return the value of a0 in the trap_ctx,
        // otherwise it will be overwritten after we trap
        // back to the original execution of the application.
        trap_ctx[TrapFrameArgs::RET] as isize
    } else {
        -1
    }
}

fn check_sigaction_error(signal: SignalFlags, action: usize, old_action: usize) -> bool {
    if action == 0
        || signal == SignalFlags::SIGKILL
        || signal == SignalFlags::SIGSTOP
    {
        true
    } else {
        false
    }
}

pub fn sys_sigaction(
    signum: i32,
    action: *const SignalAction,
    old_action: *mut SignalAction,
) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if signum as usize > MAX_SIG {
        return -1;
    }
    if let Some(flag) = SignalFlags::from_bits(1 << signum) {
        if check_sigaction_error(flag, action as usize, old_action as usize) {
            return -1;
        }
        let prev_action = inner.signal_actions.table[signum as usize];
        if !old_action.is_null() {
            *translated_refmut(token, old_action) = prev_action;
        }
        inner.signal_actions.table[signum as usize] = *translated_ref(token, action);
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

    if real_start.checked_add(len).is_none() {
        return -22;
    }

    if (flags & MAP_ANONYMOUS) != 0 {
        let start_addr = polyhal::VirtAddr::from(real_start);
        let end_addr = polyhal::VirtAddr::from(real_start + len);
        if (flags & MAP_FIXED) != 0 && inner.memory_set.is_overlap(start_addr, end_addr) {
            return -1;
        }
        if !inner.memory_set.insert_framed_area(start_addr, end_addr, permission) {
            return -12;
        }
        return real_start as isize;
    } else {
        let start_addr = polyhal::VirtAddr::from(real_start);
        let end_addr = polyhal::VirtAddr::from(real_start + len);

        if fd < 0 || fd as usize >= inner.fd_table.len() {
            return -1;
        }

        if let Some(file) = inner.fd_table[fd as usize].clone() {
            let ok = inner
                .memory_set
                .insert_file_area(start_addr, end_addr, permission, file, offset);
            if ok {
                if (flags & MAP_SHARED) != 0 {
                    inner.memory_set.areas_mut().last_mut().unwrap().map_type = MapType::Shared;
                }
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

pub fn sys_times(tms_ptr: usize) -> isize {
    let task = crate::task::current_task().unwrap();

    task.record_time_suspend();
    task.record_time_resume();

    if tms_ptr != 0 {
        let inner = task.inner_exclusive_access();
        let tms = Tms {
            tms_utime: inner.utime as isize,
            tms_stime: inner.stime as isize,
            tms_cutime: inner.cutime as isize,
            tms_cstime: inner.cstime as isize,
        };
        let token = inner.get_user_token();
        drop(inner);
        *crate::mm::translated_refmut(token, tms_ptr as *mut Tms) = tms;
    }

    polyhal::time::Time::now().to_msec() as isize
}

pub fn sys_uname(uts_ptr: usize) -> isize {
    if uts_ptr == 0 {
        return -14;
    }
    let task = crate::task::current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner.memory_set.valid_user_buf(uts_ptr as *const u8, core::mem::size_of::<UtsName>(), true) {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
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
    if !inner.memory_set.valid_user_buf(req_ptr as *const u8, ts_size, false) {
        return -14;
    }
    if rem_ptr != 0 && !inner.memory_set.valid_user_buf(rem_ptr as *const u8, ts_size, true) {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let req = *crate::mm::translated_refmut(token, req_ptr as *mut crate::task::TimeSpec);
    if req.tv_nsec >= 1_000_000_000 || req.tv_sec > 1_000_000_000 {
        return -22;
    }
    let duration_ms = req.tv_sec * 1000 + req.tv_nsec / 1_000_000;
    let now = polyhal::time::Time::now().to_msec() as u64;
    let wakeup_time = now.saturating_add(duration_ms as u64);
    loop {
        if task.inner_exclusive_access().killed {
            return -4;
        }
        if has_pending_signals() {
            return -4;
        }
        if polyhal::time::Time::now().to_msec() as u64 >= wakeup_time {
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
    let tid = {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        inner.tid_ptr
    };
    if tid != 0 {
        *translated_refmut(current_user_token(), tid as *mut i32) = 0;
    }

    exit_current_and_run_next(status);
    panic!("Unreachable in sys_exit_group!");
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
    if nfds > 1024 {
        return -22;
    }
    let token = current_user_token();
    // Validate fds array before reading
    if nfds > 0 {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        if !inner.memory_set.valid_user_buf(fds, nfds * core::mem::size_of::<PollFd>(), false) {
            return -14;
        }
        drop(inner);
    }
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

            // fd not valid
            pfd.revents |= POLLERR;
            ready += 1;
        }
        drop(inner);
        drop(task);

        if ready > 0 {
            return ready as isize;
        }

        // Check timeout
        if !tmo_p.is_null() {
            let tmo: &crate::task::TimeSpec =
                translated_ref(token, tmo_p as *const crate::task::TimeSpec);
            if tmo.tv_nsec >= 1_000_000_000 || tmo.tv_sec > 1_000_000_000 {
                return -22;
            }
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

pub fn sys_mprotect(_start: usize, _len: usize, _prot: usize) -> isize {
    0
}

pub fn sys_msync(start: usize, len: usize, _flags: usize) -> isize {
    if (len > 0 && start as u64 + len as u64 > usize::MAX as u64) || start % crate::config::PAGE_SIZE != 0 {
        return -22;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let start_addr = polyhal::VirtAddr::from(start);
    let end_addr = polyhal::VirtAddr::from(start + len);
    if inner.memory_set.msync(start_addr, end_addr) {
        0
    } else {
        -14
    }
}

// ---- mremap (216) ----

const MREMAP_MAYMOVE: usize = 1;

pub fn sys_mremap(old_addr: usize, old_size: usize, new_size: usize, flags: usize, new_addr: usize) -> isize {
    if old_addr == 0 || old_addr % crate::config::PAGE_SIZE != 0 || new_size == 0 {
        return -22;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    let round_old = (old_size + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE * crate::config::PAGE_SIZE;
    let round_new = (new_size + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE * crate::config::PAGE_SIZE;
    let old_end = old_addr + round_old;

    let target_addr = if (flags & MREMAP_MAYMOVE) != 0 && round_new > round_old {
        if new_addr != 0 {
            new_addr
        } else {
            let free = inner.memory_set.find_free_area(round_new);
            let max_end: usize = inner.memory_set.max_end_vaddr().into();
            (max_end + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE * crate::config::PAGE_SIZE
        }
    } else {
        old_addr
    };

    let new_end = target_addr + round_new;

    if (flags & MREMAP_MAYMOVE) != 0 && target_addr != old_addr {
        if inner.memory_set.is_overlap(polyhal::VirtAddr::from(target_addr), polyhal::VirtAddr::from(new_end)) {
            return -12;
        }
    }

    inner.memory_set.munmap(polyhal::VirtAddr::from(old_addr), polyhal::VirtAddr::from(old_end));

    let perm = crate::mm::MapPermission::R | crate::mm::MapPermission::W | crate::mm::MapPermission::U;
    inner.memory_set.insert_framed_area(polyhal::VirtAddr::from(target_addr), polyhal::VirtAddr::from(new_end), perm);
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
        if inner.memory_set.translate(polyhal::VirtAddr::from(vpn)).is_none() {
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

pub fn sys_mlock(_start: usize, _len: usize) -> isize { 0 }

pub fn sys_munlock(_start: usize, _len: usize) -> isize { 0 }

pub fn sys_mlockall(_flags: usize) -> isize { 0 }

pub fn sys_munlockall() -> isize { 0 }

pub fn sys_mlock2(_start: usize, _len: usize, _flags: usize) -> isize { 0 }

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
        rlim.rlim_cur = 256;
        rlim.rlim_max = 256;
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
    0
}

pub fn sys_prlimit64(_pid: usize, resource: usize, new_rlim: *const u8, old_rlim: *mut u8) -> isize {
    if resource >= RLIM_NLIMITS {
        return -22;
    }
    if !old_rlim.is_null() {
        let token = current_user_token();
        let rlim = crate::mm::translated_refmut(token, old_rlim as *mut RLimit);
        if resource == RLIMIT_NOFILE {
            rlim.rlim_cur = 256;
            rlim.rlim_max = 256;
        } else {
            rlim.rlim_cur = RLIM_INFINITY;
            rlim.rlim_max = RLIM_INFINITY;
        }
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
    if !inner.memory_set.valid_user_buf(info_ptr, core::mem::size_of::<SysInfo>(), true) {
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
    let pending = inner.signals.bits() & !inner.signal_mask.bits();
    let dst = crate::mm::translated_refmut(token, set as *mut u64);
    *dst = pending as u64;
    0
}

// ---- sigsuspend / rt_sigsuspend (133) ----

pub fn sys_rt_sigsuspend(mask: *const u8, _sigsize: usize) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    let saved_mask = inner.signal_mask;
    let new_mask_u64 = *crate::mm::translated_ref(token, mask as *const u64);
    inner.signal_mask = SignalFlags::from_bits_truncate(new_mask_u64 as u32);
    drop(inner);

    let mut loop_count: u32 = 0;
    loop {
        stdout_release_if_owner();
        crate::task::suspend_current_and_run_next();
        loop_count = loop_count.wrapping_add(1);

        let mut inner2 = task.inner_exclusive_access();
        if inner2.killed || !inner2.signals.is_empty() || loop_count > 500 {
            inner2.signal_mask = saved_mask;
            return -4;
        }
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
    if ruid != usize::MAX { inner.uid = ruid as u32; }
    if euid != usize::MAX { inner.euid = euid as u32; }
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
    if rgid != usize::MAX { inner.gid = rgid as u32; }
    if egid != usize::MAX { inner.egid = egid as u32; }
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
        if euid == usize::MAX { inner.euid = ruid as u32; }
    }
    if euid != usize::MAX { inner.euid = euid as u32; }
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
        if egid == usize::MAX { inner.egid = rgid as u32; }
    }
    if egid != usize::MAX { inner.egid = egid as u32; }
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
    if !timeout.is_null() && !inner.memory_set.valid_user_buf(timeout, core::mem::size_of::<crate::task::TimeSpec>(), false) {
        return -14;
    }
    drop(inner);

    let token = current_user_token();
    let mut rfds = [0u64; FD_SET_LONGS];
    let mut wfds = [0u64; FD_SET_LONGS];
    let mut efds = [0u64; FD_SET_LONGS];

    // Save original fd_sets for resetting each iteration
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
        if ts.tv_nsec >= 1_000_000_000 || ts.tv_sec > 1_000_000_000 {
            return -22;
        }
        ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
    } else {
        usize::MAX
    };

    let start_ms = polyhal::time::Time::now().to_msec() as usize;

    loop {
        // Reset fd_sets from saved originals (user-space buffer is modified by write-back)
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
                    let is_socket = file.as_any().downcast_ref::<crate::net::socket::SocketFile>().is_some();
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
            static mut N: u64 = 0;
            if !readfds.is_null() { write_fdset(readfds, &rfds); }
            if !writefds.is_null() { write_fdset(writefds, &wfds); }
            if !exceptfds.is_null() { write_fdset(exceptfds, &efds); }
            return ready as isize;
        }

        // Check timeout
        if timeout_ms == 0 {
            // Zero timeout, return immediately with empty sets
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
        if has_pending_signals() {
            return -4;
        }
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
    if !inner.memory_set.valid_user_buf(curr_value_ptr, core::mem::size_of::<ITimerVal>(), true) {
        return -14;
    }
    let token = inner.get_user_token();
    let now = polyhal::time::Time::now().to_msec() as u64;
    let remaining_ms = if inner.itimer_expire > now {
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
        let rem_sec = remaining_ms / 1000;
        let rem_usec = (remaining_ms % 1000) * 1000;
        let interval_sec = inner.itimer_interval_ms / 1000;
        let interval_usec = (inner.itimer_interval_ms % 1000) * 1000;
        ITimerVal {
            it_interval: TimeVal { sec: interval_sec, usec: interval_usec },
            it_value: TimeVal { sec: rem_sec, usec: rem_usec },
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
    let now = polyhal::time::Time::now().to_msec() as u64;

    if !old_value.is_null() {
        let ok = {
            let inner = task.inner_exclusive_access();
            inner.memory_set.valid_user_buf(old_value, core::mem::size_of::<ITimerVal>(), true)
        };
        if !ok {
            return -14;
        }
        let mut inner = task.inner_exclusive_access();
        let remaining_ms = if inner.itimer_expire > now {
            inner.itimer_expire - now
        } else {
            0
        };
        let old_interval_sec = inner.itimer_interval_ms / 1000;
        let old_interval_usec = (inner.itimer_interval_ms % 1000) * 1000;
        let old_rem_sec = remaining_ms / 1000;
        let old_rem_usec = (remaining_ms % 1000) * 1000;
        let old_itv = if inner.itimer_expire == 0 {
            ITimerVal {
                it_interval: TimeVal { sec: 0, usec: 0 },
                it_value: TimeVal { sec: 0, usec: 0 },
            }
        } else {
            ITimerVal {
                it_interval: TimeVal { sec: old_interval_sec, usec: old_interval_usec },
                it_value: TimeVal { sec: old_rem_sec, usec: old_rem_usec },
            }
        };
        drop(inner);
        *translated_refmut(token, old_value as *mut ITimerVal) = old_itv;
    }

    if new_value.is_null() {
        return 0;
    }

    let ok = {
        let inner = task.inner_exclusive_access();
        inner.memory_set.valid_user_buf(new_value, core::mem::size_of::<ITimerVal>(), false)
    };
    if !ok {
        return -14;
    }
    let new_itv: &ITimerVal = translated_ref(token, new_value as *const ITimerVal);
    let new_value_ms = new_itv.it_value.sec * 1000 + new_itv.it_value.usec / 1000;
    let new_interval_ms = new_itv.it_interval.sec * 1000 + new_itv.it_interval.usec / 1000;

    let mut inner = task.inner_exclusive_access();
    if new_value_ms == 0 {
        inner.itimer_expire = 0;
    } else {
        inner.itimer_expire = now + new_value_ms as u64;
    }
    inner.itimer_interval_ms = new_interval_ms as u64;
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
    if !inner.memory_set.valid_user_buf(ts, core::mem::size_of::<crate::task::TimeSpec>(), true) {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let now_ms = polyhal::time::Time::now().to_msec() as u64;
    let ts_out: &mut crate::task::TimeSpec = translated_refmut(token, ts as *mut crate::task::TimeSpec);
    ts_out.tv_sec = (now_ms / 1000) as usize;
    ts_out.tv_nsec = ((now_ms % 1000) * 1_000_000) as usize;
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
        total += n as isize;
        if (n as usize) < iov.iov_len {
            break;
        }
    }
    total
}

pub fn sys_writev(fd: usize, iov: *const u8, iovcnt: usize) -> isize {
    use crate::mm::translated_ref;
    const IOV_MAX: usize = 1024;
    if iovcnt == 0 {
        return 0;
    }
    if iovcnt > IOV_MAX {
        return -22;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    if !file.writable() {
        return -9;
    }
    drop(inner);
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
        if iov.iov_len > 0x7FFF_FFFF {
            return -22;
        }
        let buf = crate::mm::translated_byte_buffer(token, iov.iov_base as *mut u8, iov.iov_len);
        let n = file.write(buf);
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
        if !inner.memory_set.valid_user_buf(offset_ptr as *const u8, core::mem::size_of::<usize>(), false) {
            return -14;
        }
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

pub fn sys_getrusage(_who: usize, _usage: *mut u8) -> isize {
    0
}
