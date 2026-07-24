use core::arch::asm;

use crate::SignalAction;

const SYSCALL_DUP: usize = 23;
const SYSCALL_DUP3: usize = 24;
const SYSCALL_GETCWD: usize = 17;
const SYSCALL_CHDIR: usize = 49;
const SYSCALL_CHROOT: usize = 51;
const SYSCALL_OPENAT: usize = 56;
const SYSCALL_CLOSE: usize = 57;
const SYSCALL_PIPE2: usize = 59;
const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_MKDIRAT: usize = 34;
const SYSCALL_UNLINKAT: usize = 35;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_FUTEX: usize = 98;
const SYSCALL_SCHED_SETAFFINITY: usize = 122;
const SYSCALL_SCHED_GETAFFINITY: usize = 123;
const SYSCALL_YIELD: usize = 124;
const SYSCALL_KILL: usize = 129;
const SYSCALL_SIGACTION: usize = 134;
const SYSCALL_SIGPROCMASK: usize = 135;
const SYSCALL_SIGRETURN: usize = 139;
const SYSCALL_TIMES: usize = 153;
const SYSCALL_SETPGID: usize = 154;
const SYSCALL_GETPGID: usize = 155;
const SYSCALL_SETSID: usize = 157;
const SYSCALL_GET_TIME: usize = 169;
const SYSCALL_GETCPU: usize = 168;
const SYSCALL_SHMGET: usize = 194;
const SYSCALL_SHMAT: usize = 196;
const SYSCALL_SHMDT: usize = 197;
const SYSCALL_GETPID: usize = 172;
const SYSCALL_GETUID: usize = 174;
const SYSCALL_GETEUID: usize = 175;
const SYSCALL_GETGID: usize = 176;
const SYSCALL_GETEGID: usize = 177;
const SYSCALL_GETTID: usize = 178;
const SYSCALL_SETUID: usize = 146;
const SYSCALL_SETGID: usize = 144;
const SYSCALL_BRK: usize = 214;
const SYSCALL_MUNMAP: usize = 215;
const SYSCALL_FORK: usize = 401;
const SYSCALL_CLONE: usize = 220;
const SYSCALL_EXEC: usize = 221;
const SYSCALL_MMAP: usize = 222;
const SYSCALL_MPROTECT: usize = 226;
const SYSCALL_WAITPID: usize = 260;
const SYSCALL_NANOSLEEP: usize = 101;

const SYSCALL_MOUNT: usize = 40;
const SYSCALL_UMOUNT2: usize = 39;
const SYSCALL_SYMLINKAT: usize = 36;
const SYSCALL_READLINKAT: usize = 78;

const SYSCALL_GETDENTS64: usize = 61;

const SYSCALL_SHUTDOWN: usize = 403;

const AT_FDCWD: isize = -100;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Tms {
    pub tms_utime: isize,
    pub tms_stime: isize,
    pub tms_cutime: isize,
    pub tms_cstime: isize,
}

#[cfg(target_arch = "riscv64")]
fn syscall(id: usize, args: [usize; 6]) -> isize {
    let mut ret: isize;
    unsafe {
        asm!(
            "ecall",
            inlateout("x10") args[0] => ret,
            in("x11") args[1],
            in("x12") args[2],
            in("x13") args[3],
            in("x14") args[4],
            in("x15") args[5],
            in("x17") id
        );
    }
    ret
}

#[cfg(target_arch = "aarch64")]
fn syscall(id: usize, args: [usize; 6]) -> isize {
    let mut ret: isize;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") args[0] => ret,
            in("x1") args[1],
            in("x2") args[2],
            in("x3") args[3],
            in("x4") args[4],
            in("x5") args[5],
            in("x8") id
        );
    }
    ret
}

#[cfg(target_arch = "x86_64")]
fn syscall(id: usize, args: [usize; 6]) -> isize {
    let mut ret: isize;
    unsafe {
        asm!(
            "
                push r11
                push rcx
                syscall
                pop  rcx
                pop  r11
            ",
            in("rdi") args[0],
            in("rsi") args[1],
            in("rdx") args[2],
            in("r10") args[3],
            in("r8") args[4],
            in("r9") args[5],
            inlateout("rax") id => ret
        );
    }
    ret
}

#[cfg(target_arch = "loongarch64")]
fn syscall(id: usize, args: [usize; 6]) -> isize {
    let mut ret: isize;
    unsafe {
        asm!(
            "syscall 0",
            inlateout("$r4") args[0] => ret,
            in("$r5") args[1],
            in("$r6") args[2],
            in("$r7") args[3],
            in("$r8") args[4],
            in("$r9") args[5],
            in("$r11") id
        );
    }
    ret
}

pub fn sys_dup(fd: usize) -> isize {
    syscall(SYSCALL_DUP, [fd, 0, 0, 0, 0, 0])
}

pub fn sys_dup2(old_fd: usize, new_fd: usize) -> isize {
    syscall(SYSCALL_DUP3, [old_fd, new_fd, 0, 0, 0, 0])
}

pub fn sys_open(path: &str, flags: u32) -> isize {
    sys_openat(AT_FDCWD, path, flags, 0)
}

pub fn sys_openat(dirfd: isize, path: &str, flags: u32, mode: u32) -> isize {
    syscall(
        SYSCALL_OPENAT,
        [
            dirfd as usize,
            path.as_ptr() as usize,
            flags as usize,
            mode as usize,
            0,
            0,
        ],
    )
}

pub fn sys_close(fd: usize) -> isize {
    syscall(SYSCALL_CLOSE, [fd, 0, 0, 0, 0, 0])
}

pub fn sys_pipe(pipe: &mut [u32]) -> isize {
    syscall(SYSCALL_PIPE2, [pipe.as_mut_ptr() as usize, 0, 0, 0, 0, 0])
}

pub fn sys_read(fd: usize, buffer: &mut [u8]) -> isize {
    syscall(
        SYSCALL_READ,
        [fd, buffer.as_mut_ptr() as usize, buffer.len(), 0, 0, 0],
    )
}

pub fn sys_write(fd: usize, buffer: &[u8]) -> isize {
    syscall(
        SYSCALL_WRITE,
        [fd, buffer.as_ptr() as usize, buffer.len(), 0, 0, 0],
    )
}

pub fn sys_exit(exit_code: i32) -> ! {
    syscall(SYSCALL_EXIT, [exit_code as usize, 0, 0, 0, 0, 0]);
    panic!("sys_exit never returns!");
}

pub fn sys_yield() -> isize {
    syscall(SYSCALL_YIELD, [0, 0, 0, 0, 0, 0])
}

pub fn sys_sched_setaffinity(pid: usize, mask: &[u8]) -> isize {
    syscall(
        SYSCALL_SCHED_SETAFFINITY,
        [pid, mask.len(), mask.as_ptr() as usize, 0, 0, 0],
    )
}

pub fn sys_sched_getaffinity(pid: usize, mask: &mut [u8]) -> isize {
    syscall(
        SYSCALL_SCHED_GETAFFINITY,
        [pid, mask.len(), mask.as_mut_ptr() as usize, 0, 0, 0],
    )
}

pub fn sys_getcpu(cpu: &mut u32, node: Option<&mut u32>) -> isize {
    let node_ptr = node.map_or(core::ptr::null_mut(), |node| node as *mut u32);
    syscall(
        SYSCALL_GETCPU,
        [cpu as *mut u32 as usize, node_ptr as usize, 0, 0, 0, 0],
    )
}

pub fn sys_kill(pid: isize, signal: i32) -> isize {
    syscall(SYSCALL_KILL, [pid as usize, signal as usize, 0, 0, 0, 0])
}

pub fn sys_setpgid(pid: usize, pgid: usize) -> isize {
    syscall(SYSCALL_SETPGID, [pid, pgid, 0, 0, 0, 0])
}

pub fn sys_getpgid(pid: usize) -> isize {
    syscall(SYSCALL_GETPGID, [pid, 0, 0, 0, 0, 0])
}

pub fn sys_setsid() -> isize {
    syscall(SYSCALL_SETSID, [0, 0, 0, 0, 0, 0])
}

pub fn sys_get_time() -> isize {
    syscall(SYSCALL_GET_TIME, [0, 0, 0, 0, 0, 0])
}

pub fn sys_getpid() -> isize {
    syscall(SYSCALL_GETPID, [0, 0, 0, 0, 0, 0])
}

pub fn sys_fork() -> isize {
    syscall(SYSCALL_FORK, [0, 0, 0, 0, 0, 0])
}

/// Raw RISC-V clone ABI: flags, child_stack, parent_tid, tls, child_tid.
/// Tests use this only with an explicitly allocated child stack.
pub fn sys_clone(
    flags: usize,
    child_stack: usize,
    parent_tid: usize,
    tls: usize,
    child_tid: usize,
) -> isize {
    syscall(
        SYSCALL_CLONE,
        [flags, child_stack, parent_tid, tls, child_tid, 0],
    )
}

pub fn sys_exec(path: &str, args: &[*const u8], envs: &[*const u8]) -> isize {
    syscall(
        SYSCALL_EXEC,
        [
            path.as_ptr() as usize,
            args.as_ptr() as usize,
            envs.as_ptr() as usize,
            0,
            0,
            0,
        ],
    )
}

pub fn sys_chroot(path: &str) -> isize {
    syscall(SYSCALL_CHROOT, [path.as_ptr() as usize, 0, 0, 0, 0, 0])
}

pub fn sys_readlinkat(dirfd: isize, path: *const u8, buf: *mut u8, bufsiz: usize) -> isize {
    syscall(
        SYSCALL_READLINKAT,
        [dirfd as usize, path as usize, buf as usize, bufsiz, 0, 0],
    )
}

pub fn sys_waitpid(pid: isize, exit_code: *mut i32) -> isize {
    syscall(
        SYSCALL_WAITPID,
        [pid as usize, exit_code as usize, 0, 0, 0, 0],
    )
}

pub fn sys_waitpid_options(pid: isize, exit_code: *mut i32, options: usize) -> isize {
    syscall(
        SYSCALL_WAITPID,
        [pid as usize, exit_code as usize, options, 0, 0, 0],
    )
}

pub fn sys_sigaction(
    signum: i32,
    action: *const SignalAction,
    old_action: *mut SignalAction,
) -> isize {
    syscall(
        SYSCALL_SIGACTION,
        [
            signum as usize,
            action as usize,
            old_action as usize,
            0,
            0,
            0,
        ],
    )
}

pub fn sys_sigprocmask(how: usize, set: usize, oldset: usize, sigsetsize: usize) -> isize {
    syscall(SYSCALL_SIGPROCMASK, [how, set, oldset, sigsetsize, 0, 0])
}

pub fn sys_sigreturn() -> isize {
    syscall(SYSCALL_SIGRETURN, [0, 0, 0, 0, 0, 0])
}

pub fn sys_brk(brk_addr: usize) -> isize {
    syscall(SYSCALL_BRK, [brk_addr, 0, 0, 0, 0, 0])
}

/// Raw Linux-compatible virtual-memory syscalls used by the SMP regression
/// binaries.  The user library intentionally leaves policy (flags and page
/// alignment) to the caller, as the kernel ABI does.
pub fn sys_mmap(
    addr: usize,
    len: usize,
    prot: usize,
    flags: usize,
    fd: isize,
    offset: usize,
) -> isize {
    syscall(
        SYSCALL_MMAP,
        [addr, len, prot, flags, fd as usize, offset],
    )
}

pub fn sys_mprotect(addr: usize, len: usize, prot: usize) -> isize {
    syscall(SYSCALL_MPROTECT, [addr, len, prot, 0, 0, 0])
}

pub fn sys_munmap(addr: usize, len: usize) -> isize {
    syscall(SYSCALL_MUNMAP, [addr, len, 0, 0, 0, 0])
}

pub fn sys_times(tms: *mut Tms) -> isize {
    syscall(SYSCALL_TIMES, [tms as usize, 0, 0, 0, 0, 0])
}

pub fn sys_chdir(path: &str) -> isize {
    syscall(SYSCALL_CHDIR, [path.as_ptr() as usize, 0, 0, 0, 0, 0])
}

pub fn sys_getcwd(buf: &mut [u8]) -> isize {
    syscall(
        SYSCALL_GETCWD,
        [buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0],
    )
}
pub fn sys_getdents64(fd: usize, buf: &mut [u8]) -> isize {
    syscall(
        SYSCALL_GETDENTS64,
        [fd, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0],
    )
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TimeSpec {
    pub tv_sec: usize,
    pub tv_nsec: usize,
}

pub fn sys_nanosleep(req: *const TimeSpec, rem: *mut TimeSpec) -> isize {
    syscall(SYSCALL_NANOSLEEP, [req as usize, rem as usize, 0, 0, 0, 0])
}

/// Invoke the legacy futex syscall used by the RISC-V kernel ABI.
///
/// The raw interface is intentionally exposed for kernel regression tests;
/// higher-level user synchronization primitives should wrap it instead of
/// open-coding operation numbers.
pub fn sys_futex(
    uaddr: *mut u32,
    op: usize,
    val: usize,
    timeout_or_val2: usize,
    uaddr2: *mut u32,
    val3: usize,
) -> isize {
    syscall(
        SYSCALL_FUTEX,
        [
            uaddr as usize,
            op,
            val,
            timeout_or_val2,
            uaddr2 as usize,
            val3,
        ],
    )
}

pub fn sys_shmget(key: usize, size: usize, flags: usize) -> isize {
    syscall(SYSCALL_SHMGET, [key, size, flags, 0, 0, 0])
}

pub fn sys_shmat(shmid: usize, addr: usize, flags: usize) -> isize {
    syscall(SYSCALL_SHMAT, [shmid, addr, flags, 0, 0, 0])
}

pub fn sys_shmdt(addr: usize) -> isize {
    syscall(SYSCALL_SHMDT, [addr, 0, 0, 0, 0, 0])
}

pub fn sys_mount(source: &str, target: &str, fstype: &str, flags: usize, data: &[u8]) -> isize {
    let src = alloc::format!("{}\0", source);
    let tgt = alloc::format!("{}\0", target);
    let fst = alloc::format!("{}\0", fstype);
    let data_ptr = if data.is_empty() {
        0
    } else {
        data.as_ptr() as usize
    };
    syscall(
        SYSCALL_MOUNT,
        [
            src.as_ptr() as usize,
            tgt.as_ptr() as usize,
            fst.as_ptr() as usize,
            flags,
            data_ptr,
            0,
        ],
    )
}

pub fn sys_umount2(target: &str, flags: usize) -> isize {
    let tgt = alloc::format!("{}\0", target);
    syscall(SYSCALL_UMOUNT2, [tgt.as_ptr() as usize, flags, 0, 0, 0, 0])
}

pub fn sys_shutdown() -> isize {
    syscall(SYSCALL_SHUTDOWN, [0, 0, 0, 0, 0, 0])
}

pub fn sys_mkdir(path: *const u8, mode: u32) -> isize {
    syscall(
        SYSCALL_MKDIRAT,
        [AT_FDCWD as usize, path as usize, mode as usize, 0, 0, 0],
    )
}

pub fn sys_symlinkat(target: *const u8, linkpath: *const u8) -> isize {
    syscall(
        SYSCALL_SYMLINKAT,
        [
            target as usize,
            AT_FDCWD as usize,
            linkpath as usize,
            0,
            0,
            0,
        ],
    )
}

pub fn sys_unlinkat_path(path: *const u8, flags: u32) -> isize {
    syscall(
        SYSCALL_UNLINKAT,
        [AT_FDCWD as usize, path as usize, flags as usize, 0, 0, 0],
    )
}

pub fn sys_getuid() -> isize {
    syscall(SYSCALL_GETUID, [0, 0, 0, 0, 0, 0])
}

pub fn sys_geteuid() -> isize {
    syscall(SYSCALL_GETEUID, [0, 0, 0, 0, 0, 0])
}

pub fn sys_getgid() -> isize {
    syscall(SYSCALL_GETGID, [0, 0, 0, 0, 0, 0])
}

pub fn sys_getegid() -> isize {
    syscall(SYSCALL_GETEGID, [0, 0, 0, 0, 0, 0])
}

pub fn sys_gettid() -> isize {
    syscall(SYSCALL_GETTID, [0, 0, 0, 0, 0, 0])
}

pub fn sys_setuid(uid: u32) -> isize {
    syscall(SYSCALL_SETUID, [uid as usize, 0, 0, 0, 0, 0])
}

pub fn sys_setgid(gid: u32) -> isize {
    syscall(SYSCALL_SETGID, [gid as usize, 0, 0, 0, 0, 0])
}

const SYS_SOCKET: usize = 198;
const SYS_BIND: usize = 200;
const SYS_LISTEN: usize = 201;

pub fn sys_socket(domain: usize, sock_type: usize, protocol: usize) -> isize {
    syscall(SYS_SOCKET, [domain, sock_type, protocol, 0, 0, 0])
}

pub fn sys_bind(fd: usize, addr: usize, addrlen: usize) -> isize {
    syscall(SYS_BIND, [fd, addr, addrlen, 0, 0, 0])
}

pub fn sys_listen(fd: usize, backlog: usize) -> isize {
    syscall(SYS_LISTEN, [fd, backlog, 0, 0, 0, 0])
}
