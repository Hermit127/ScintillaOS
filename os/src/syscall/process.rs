use crate::fs::{open_file, OpenFlags};
use crate::mm::{translated_byte_buffer, translated_ref, translated_refmut, translated_str, MapPermission};
use crate::task::{
    add_task, current_task, current_user_token, exit_current_and_run_next, pid2task,
    sleep_current_and_run_next, suspend_current_and_run_next, SignalAction, SignalFlags, MAX_SIG,
};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use log::info;
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
        let token = current_user_token();
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
        Some(parent_weak) => parent_weak.upgrade().map(|p| p.getpid() as isize).unwrap_or(1),
        None => 1,
    }
}

pub fn sys_clone(_flags: usize, stack: usize, _ptid: usize, _tls: usize, _ctid: usize) -> isize {
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    trap_cx[TrapFrameArgs::RET] = 0;
    if stack != 0 {
        trap_cx[TrapFrameArgs::SP] = stack;
    }
    add_task(new_task);
    new_pid as isize
}

pub fn sys_fork() -> isize {
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;
    // modify trap context of new_task, because it returns immediately after switching
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx[TrapFrameArgs::RET] = 0;
    // add new task to scheduler
    add_task(new_task);
    new_pid as isize
}

pub fn sys_exec(path: *const u8, mut args: *const usize) -> isize {
    let token = current_user_token();
    info!("sys_exec: {:p}  args: {:p}", path, args);
    // log::info!("path: {:p} token: {:#x} kernel token: {:#x}", path, current_user_token(), kernel_page_table_token());
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
        let argc = args_vec.len();
        task.exec(all_data.as_slice(), args_vec);
        // return argc because cx.x[10] will be covered with it later
        argc as isize
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32, _options: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        p.inner_exclusive_access().is_zombie() && (pid == -1 || pid as usize == p.getpid())
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        let exit_code = child.inner_exclusive_access().exit_code;
        if !exit_code_ptr.is_null() {
            *translated_refmut(inner.memory_set.token(), exit_code_ptr) = (exit_code << 8) as i32;
        }
        return found_pid as isize;
    }
    drop(inner);
    crate::task::WAITPID_QUEUE.exclusive_access().push(task.clone());
    drop(task);
    crate::task::sleep_current_and_run_next();
    sys_waitpid(pid, exit_code_ptr, _options)
}

pub fn sys_kill(pid: usize, signum: i32) -> isize {
    if let Some(task) = pid2task(pid) {
        if let Some(flag) = SignalFlags::from_bits(1 << signum) {
            let mut task_ref = task.inner_exclusive_access();
            if task_ref.signals.contains(flag) {
                return 0;
            }
            task_ref.signals.insert(flag);
            0
        } else {
            -22 // EINVAL
        }
    } else {
        0 // kill to non-existent pid: silently succeed (POSIX)
    }
}

pub fn sys_sigprocmask(mask: u32) -> isize {
    if let Some(task) = current_task() {
        let mut inner = task.inner_exclusive_access();
        let old_mask = inner.signal_mask;
        if let Some(flag) = SignalFlags::from_bits(mask) {
            inner.signal_mask = flag;
            old_mask.bits() as isize
        } else {
            -1
        }
    } else {
        -1
    }
}

pub fn sys_sigreturn() -> isize {
    if let Some(task) = current_task() {
        let mut inner = task.inner_exclusive_access();
        inner.handling_sig = -1;
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
        || old_action == 0
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
        *translated_refmut(token, old_action) = prev_action;
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

    let old_brk_page = (old_brk + crate::config::PAGE_SIZE - 1) /  crate::config::PAGE_SIZE;
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
        inner.memory_set.push(map_area, None, 0);
    }else if new_brk_page < old_brk_page {
        let limit_va = polyhal::VirtAddr::new(new_brk_page * crate::config::PAGE_SIZE);
        inner.memory_set.shrink_to(limit_va);
    }

    inner.program_brk = brk_addr;
    brk_addr as isize
}

pub fn sys_sleep(duration: u64) -> isize {
    let wakeup_time = sys_get_time(0, 0) + duration as isize;
    loop {
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
    if prot & PROT_READ != 0 { perm |= crate::mm::MapPermission::R; }
    if prot & PROT_WRITE != 0 { perm |= crate::mm::MapPermission::W; }
    if prot & PROT_EXEC != 0 { perm |= crate::mm::MapPermission::X; }
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

    if (flags & MAP_ANONYMOUS) != 0 {
        let actual_start = if start == 0 {
            let free_va = inner.memory_set.find_free_area(len);
            if free_va.raw() == 0 {
                return -1;
            }
            free_va.raw()
        } else {
            start
        };
        let start_addr = polyhal::VirtAddr::from(actual_start);
        let end_addr = polyhal::VirtAddr::from(actual_start + len);
        if (flags & MAP_FIXED) != 0 && inner.memory_set.is_overlap(start_addr, end_addr) {
            return -1;
        }
        inner.memory_set.insert_framed_area(start_addr, end_addr, permission);
        return actual_start as isize;
    } else {
        let actual_start = if start == 0 {
            let free_va = inner.memory_set.find_free_area(len);
            if free_va.raw() == 0 {
                return -1;
            }
            free_va.raw()
        } else {
            start
        };
        let start_addr = polyhal::VirtAddr::from(actual_start);
        let end_addr = polyhal::VirtAddr::from(actual_start + len);

        if fd < 0 || fd as usize >= inner.fd_table.len() {
            crate::println!("[mmap] bad fd={} table_len={}", fd, inner.fd_table.len());
            return -1;
        }
        
        if let Some(file) = inner.fd_table[fd as usize].clone() {
            let ok = inner.memory_set.insert_file_area(
                start_addr, 
                end_addr, 
                permission, 
                file, 
                offset
            );
            if ok {
                return actual_start as isize;
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
        5 => { // clear
            crate::logging::klog_clear();
            0
        }
        2 | 3 | 4 => { // read / read all / read+clear
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
        9 => { // number of unread chars
            let (part1, part2) = crate::logging::klog_bytes();
            (part1.len() + part2.len()) as isize
        }
        _ => -22, // EINVAL
    }
}

pub fn sys_nanosleep(req_ptr: usize, rem_ptr: usize) -> isize {
    let task = crate::task::current_task().unwrap();
    let token = task.inner_exclusive_access().get_user_token();
    let req = *crate::mm::translated_refmut(token, req_ptr as *mut crate::task::TimeSpec);
    let duration_ms = req.tv_sec * 1000 + req.tv_nsec / 1_000_000;
    let wakeup_time = polyhal::time::Time::now().to_msec() as u64 + duration_ms as u64;
    loop {
        if polyhal::time::Time::now().to_msec() as u64 >= wakeup_time {
            break;
        }
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
    let tid_ptr = {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        inner.tid_ptr
    };
    if tid_ptr != 0 {
        *translated_refmut(current_user_token(), tid_ptr as *mut i32) = 0;
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
    current_task()
        .map(|t| t.getpid() as isize)
        .unwrap_or(-1)
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

pub fn sys_ppoll(fds: *mut u8, nfds: usize, tmo_p: *const u8, _sigmask: *const u8, _sigsetsize: usize) -> isize {
    let token = current_user_token();
    let buf = translated_byte_buffer(token, fds, nfds * core::mem::size_of::<PollFd>());
    let pollfds: &mut [PollFd] = unsafe {
        core::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut PollFd, nfds)
    };

    let start_ms = polyhal::time::Time::now().to_msec() as usize;

    loop {
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
                    if (pfd.events & POLLIN) != 0 && readable { pfd.revents |= POLLIN; }
                    if (pfd.events & POLLOUT) != 0 && writable { pfd.revents |= POLLOUT; }
                    if error { pfd.revents |= POLLHUP; }
                    if pfd.revents != 0 { ready += 1; }
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
            let tmo: &crate::task::TimeSpec = translated_ref(token, tmo_p as *const crate::task::TimeSpec);
            let timeout_ms = tmo.tv_sec * 1000 + tmo.tv_nsec / 1_000_000;
            if timeout_ms == 0 {
                return 0;
            }
            let now_ms = polyhal::time::Time::now().to_msec() as usize;
            if now_ms - start_ms >= timeout_ms {
                return 0;
            }
        }

        suspend_current_and_run_next();
    }
}

pub fn sys_mprotect(_start: usize, _len: usize, _prot: usize) -> isize {
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

    let timeout_ms = if !timeout.is_null() {
        let ts: &crate::task::TimeSpec = translated_ref(token, timeout as *const crate::task::TimeSpec);
        ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
    } else {
        usize::MAX
    };

    let start_ms = polyhal::time::Time::now().to_msec() as usize;

    loop {
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
                }
            }
            // Invalid fd: error only if requested in exceptfds
            // Valid fd but file is None: should not happen with fd_table holes

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
            // Write back modified fd_sets
            if !readfds.is_null() {
                write_fdset(readfds, &rfds);
            }
            if !writefds.is_null() {
                write_fdset(writefds, &wfds);
            }
            if !exceptfds.is_null() {
                write_fdset(exceptfds, &efds);
            }
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

        suspend_current_and_run_next();
    }
}

pub fn sys_futex(_uaddr: *mut u32, futex_op: usize, _val: usize, _val2: usize, _uaddr2: *mut u32, _val3: usize) -> isize {
    match futex_op & 0x0F {
        0 | 9 => -38, // FUTEX_WAIT / FUTEX_WAIT_BITSET → ENOSYS (musl fallback to busy-wait)
        1 => 0,       // FUTEX_WAKE → nobody waiting
        _ => -38,
    }
}

pub fn sys_madvise(_start: usize, _len: usize, _advice: usize) -> isize {
    0
}

pub fn sys_clock_gettime(_clk_id: usize, _ts: *mut u8) -> isize {
    -38
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
        if n <= 0 { break; }
        let written = out_file.write(&mut buf[..n as usize]);
        if written <= 0 { break; }
        total += written;
        if written < n { break; }
    }

    if let Some(off) = saved_in_off {
        in_file.seek(off);
    }

    total
}

pub fn sys_getrusage(_who: usize, _usage: *mut u8) -> isize { 0 }
