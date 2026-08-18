const SYSCALL_SETXATTR: usize = 5;
const SYSCALL_LSETXATTR: usize = 6;
const SYSCALL_FSETXATTR: usize = 7;
const SYSCALL_GETXATTR: usize = 8;
const SYSCALL_LGETXATTR: usize = 9;
const SYSCALL_FGETXATTR: usize = 10;
const SYSCALL_LISTXATTR: usize = 11;
const SYSCALL_LLISTXATTR: usize = 12;
const SYSCALL_FLISTXATTR: usize = 13;
const SYSCALL_REMOVEXATTR: usize = 14;
const SYSCALL_LREMOVEXATTR: usize = 15;
const SYSCALL_FREMOVEXATTR: usize = 16;
const SYSCALL_DUP: usize = 23;
const SYSCALL_DUP3: usize = 24;
const SYSCALL_FCNTL: usize = 25;
const SYSCALL_FACCESSAT: usize = 48;
const SYSCALL_FACCESSAT2: usize = 439;
const SYSCALL_FCHMOD: usize = 52;
const SYSCALL_FCHMODAT: usize = 53;
const SYSCALL_FCHMODAT2: usize = 452;
const SYSCALL_FCHOWNAT: usize = 54;
const SYSCALL_FCHOWN: usize = 55;
const SYSCALL_IOCTL: usize = 29;
const SYSCALL_MKNODAT: usize = 33;
const SYSCALL_GETCWD: usize = 17;
const SYSCALL_MKDIRAT: usize = 34;
const SYSCALL_UNLINKAT: usize = 35;
const SYSCALL_SYMLINKAT: usize = 36;
const SYSCALL_LINKAT: usize = 37;
const SYSCALL_TRUNCATE: usize = 45;
const SYSCALL_FTRUNCATE: usize = 46;
const SYSCALL_FALLOCATE: usize = 47;
const SYSCALL_CHDIR: usize = 49;
const SYSCALL_FCHDIR: usize = 50;
const SYSCALL_CHROOT: usize = 51;
const SYSCALL_OPENAT: usize = 56;
const SYSCALL_CLOSE: usize = 57;
const SYSCALL_PIPE2: usize = 59;
const SYSCALL_LSEEK: usize = 62;
const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_SENDFILE: usize = 71;
const SYSCALL_WRITEV: usize = 66;
const SYSCALL_PREAD64: usize = 67;
const SYSCALL_PWRITE64: usize = 68;
const SYSCALL_PREADV: usize = 69;
const SYSCALL_PWRITEV: usize = 70;
const SYSCALL_PREADV2: usize = 286;
const SYSCALL_PWRITEV2: usize = 287;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_FUTEX: usize = 98;
const SYSCALL_NANOSLEEP: usize = 101;
const SYSCALL_CLOCK_GETTIME: usize = 113;
const SYSCALL_CLOCK_GETRES: usize = 114;
const SYSCALL_CLOCK_NANOSLEEP: usize = 115;
const SYSCALL_SYSLOG: usize = 116;
const SYSCALL_YIELD: usize = 124;
const SYSCALL_KILL: usize = 129;
const SYSCALL_TKILL: usize = 130;
const SYSCALL_TGKILL: usize = 131;
const SYSCALL_SIGALTSTACK: usize = 132;
const SYSCALL_SIGSUSPEND: usize = 133;
const SYSCALL_SIGACTION: usize = 134;
const SYSCALL_SIGPROCMASK: usize = 135;
const SYSCALL_RT_SIGPENDING: usize = 136;
const SYSCALL_RT_SIGTIMEDWAIT: usize = 137;
const SYSCALL_SIGRETURN: usize = 139;
const SYSCALL_GETITIMER: usize = 102;
const SYSCALL_SETITIMER: usize = 103;
const SYSCALL_TIMER_CREATE: usize = 107;
const SYSCALL_TIMER_GETTIME: usize = 108;
const SYSCALL_TIMER_GETOVERRUN: usize = 109;
const SYSCALL_TIMER_SETTIME: usize = 110;
const SYSCALL_TIMER_DELETE: usize = 111;
const SYSCALL_TIMES: usize = 153;
const SYSCALL_UNAME: usize = 160;
const SYSCALL_GETCPU: usize = 168;
const SYSCALL_GET_TIME: usize = 169;
const SYSCALL_GETPID: usize = 172;
const SYSCALL_GETPPID: usize = 173;
const SYSCALL_GETUID: usize = 174;
const SYSCALL_GETEUID: usize = 175;
const SYSCALL_GETGID: usize = 176;
const SYSCALL_GETEGID: usize = 177;
const SYSCALL_GETTID: usize = 178;
const SYSCALL_GETRESUID: usize = 148;
const SYSCALL_GETRESGID: usize = 150;
const SYSCALL_GETRLIMIT: usize = 163;
const SYSCALL_SETRLIMIT: usize = 164;
const SYSCALL_GETRUSAGE: usize = 165;
const SYSCALL_UMASK: usize = 166;
const SYSCALL_SETREGID: usize = 143;
const SYSCALL_SETGID: usize = 144;
const SYSCALL_SETREUID: usize = 145;
const SYSCALL_SETUID: usize = 146;
const SYSCALL_SETRESUID: usize = 147;
const SYSCALL_SETRESGID: usize = 149;
const SYSCALL_GETGROUPS: usize = 158;
const SYSCALL_SETGROUPS: usize = 159;
const SYSCALL_BRK: usize = 214;
const SYSCALL_MUNMAP: usize = 215;
const SYSCALL_MREMAP: usize = 216;
const SYSCALL_READV: usize = 65;
const SYSCALL_MPROTECT: usize = 226;
const SYSCALL_SETPGID: usize = 154;
const SYSCALL_GETPGID: usize = 155;
const SYSCALL_GETSID: usize = 156;
const SYSCALL_SETSID: usize = 157;
const SYSCALL_MADVISE: usize = 233;
const SYSCALL_SET_ROBUST_LIST: usize = 99;
const SYSCALL_GET_ROBUST_LIST: usize = 100;
const SYSCALL_GETRANDOM: usize = 278;
const SYSCALL_COPY_FILE_RANGE: usize = 285;
const SYSCALL_PSELECT6: usize = 72;
const SYSCALL_PPOLL: usize = 73;
const SYSCALL_STATFS: usize = 43;
const SYSCALL_FSTATFS: usize = 44;
const SYSCALL_PRCTL: usize = 167;
const SYSCALL_PRLIMIT64: usize = 261;
const SYSCALL_SYSINFO: usize = 179;
const SYSCALL_SETPRIORITY: usize = 140;
const SYSCALL_GETPRIORITY: usize = 141;
const SYSCALL_SCHED_SETAFFINITY: usize = 122;
const SYSCALL_SCHED_GETAFFINITY: usize = 123;
const SYSCALL_EPOLL_CREATE1: usize = 20;
const SYSCALL_EPOLL_CTL: usize = 21;
const SYSCALL_EPOLL_PWAIT: usize = 22;
const SYSCALL_STATX: usize = 291;
const SYSCALL_EVENTFD2: usize = 19;
const SYSCALL_TIMERFD_CREATE: usize = 85;
const SYSCALL_TIMERFD_SETTIME: usize = 86;
const SYSCALL_TIMERFD_GETTIME: usize = 87;
const SYSCALL_RSEQ: usize = 293;
const SYSCALL_RISCV_HWPROBE: usize = 258;
#[cfg(any(
    feature = "smp-diagnostics",
    feature = "buildstorm-diagnostics",
    feature = "unknown-syscall-diagnostics"
))]
const SYSCALL_FSOPEN: usize = 430;
const SYSCALL_CLONE3: usize = 435;
const SYSCALL_CLONE: usize = 220;
const SYSCALL_FORK: usize = 401;
pub(crate) const SYSCALL_EXEC: usize = 221;
const SYSCALL_MMAP: usize = 222;
const SYSCALL_FADVISE64: usize = 223;
const SYSCALL_MSYNC: usize = 227;
const SYSCALL_MLOCK: usize = 228;
const SYSCALL_MUNLOCK: usize = 229;
const SYSCALL_MLOCKALL: usize = 230;
const SYSCALL_MUNLOCKALL: usize = 231;
const SYSCALL_MINCORE: usize = 232;
const SYSCALL_MLOCK2: usize = 284;
const SYSCALL_WAITPID: usize = 260;

const SYSCALL_GETDENTS64: usize = 61;
const SYSCALL_FSTATAT: usize = 79;
const SYSCALL_READLINKAT: usize = 78;
const SYSCALL_FSTAT: usize = 80;
const SYSCALL_SYNC: usize = 81;
const SYSCALL_FSYNC: usize = 82;
const SYSCALL_FDATASYNC: usize = 83;
const SYSCALL_SLEEP: usize = 300;
const SYSCALL_SYNCFS: usize = 267;

const SYSCALL_MOUNT: usize = 40;
const SYSCALL_UMOUNT2: usize = 39;

const SYSCALL_UTIMENSAT: usize = 88;

const SYSCALL_RENAMEAT2: usize = 276;

const SYSCALL_FLOCK: usize = 32;
const SYSCALL_READAHEAD: usize = 213;
const SYSCALL_SOCKET: usize = 198;
const SYSCALL_SOCKETPAIR: usize = 199;
const SYSCALL_SHMGET: usize = 194;
const SYSCALL_SHMCTL: usize = 195;
const SYSCALL_SHMAT: usize = 196;
const SYSCALL_SHMDT: usize = 197;
const SYSCALL_BIND: usize = 200;
const SYSCALL_LISTEN: usize = 201;
const SYSCALL_ACCEPT: usize = 202;
const SYSCALL_CONNECT: usize = 203;
const SYSCALL_GETSOCKNAME: usize = 204;
const SYSCALL_GETPEERNAME: usize = 205;
const SYSCALL_SENDTO: usize = 206;
const SYSCALL_RECVFROM: usize = 207;
const SYSCALL_SETSOCKOPT: usize = 208;
const SYSCALL_GETSOCKOPT: usize = 209;
const SYSCALL_SOCK_SHUTDOWN: usize = 210;
const SYSCALL_SHUTDOWN: usize = 403;
const SYSCALL_SENDMSG: usize = 211;
const SYSCALL_RECVMSG: usize = 212;
const SYSCALL_CLOSE_RANGE: usize = 436;

const SYSCALL_SET_TID_ADDRESS: usize = 96;
const SYSCALL_EXIT_GROUP: usize = 94;
const SYSCALL_WAITID: usize = 95;

mod fs;
pub(crate) mod process;

pub(crate) use fs::install_fd_shared;

use crate::net::syscall::{
    sys_accept, sys_bind, sys_connect, sys_getpeername, sys_getsockname, sys_getsockopt,
    sys_listen, sys_recvfrom, sys_recvmsg, sys_sendmsg, sys_sendto, sys_setsockopt,
    sys_sock_shutdown, sys_socket,
};
use core::sync::atomic::{AtomicUsize, Ordering};
use fs::*;
use process::*;

#[cfg(feature = "buildstorm-diagnostics")]
struct ActiveSyscall {
    id: AtomicUsize,
    pid: AtomicUsize,
    args: [AtomicUsize; 6],
}

#[cfg(feature = "buildstorm-diagnostics")]
impl ActiveSyscall {
    const fn new() -> Self {
        Self {
            id: AtomicUsize::new(usize::MAX),
            pid: AtomicUsize::new(0),
            args: [const { AtomicUsize::new(0) }; 6],
        }
    }
}

#[cfg(feature = "buildstorm-diagnostics")]
static ACTIVE_SYSCALLS: [ActiveSyscall; crate::config::MAX_CPUS] =
    [const { ActiveSyscall::new() }; crate::config::MAX_CPUS];

#[cfg(feature = "buildstorm-diagnostics")]
fn record_active_syscall(pid: usize, id: usize, args: &[usize; 6]) {
    let Some(cpu) = crate::task::try_current_hart_id() else {
        return;
    };
    let Some(active) = ACTIVE_SYSCALLS.get(cpu) else {
        return;
    };
    active.pid.store(pid, Ordering::Relaxed);
    for (slot, value) in active.args.iter().zip(args.iter()) {
        slot.store(*value, Ordering::Relaxed);
    }
    active.id.store(id, Ordering::Release);
}

#[cfg(feature = "buildstorm-diagnostics")]
fn clear_active_syscall() {
    let Some(cpu) = crate::task::try_current_hart_id() else {
        return;
    };
    if let Some(active) = ACTIVE_SYSCALLS.get(cpu) {
        active.id.store(usize::MAX, Ordering::Release);
    }
}

#[cfg(feature = "buildstorm-diagnostics")]
pub(crate) fn dump_active_syscall_for_oom() {
    let cpu = crate::task::try_current_hart_id().unwrap_or(usize::MAX);
    let Some(active) = ACTIVE_SYSCALLS.get(cpu) else {
        crate::println!("[heap-debug] active-syscall cpu={} unavailable", cpu);
        return;
    };
    let id = active.id.load(Ordering::Acquire);
    crate::println!(
        "[heap-debug] active-syscall cpu={} pid={} nr={} a0={:#x} a1={:#x} a2={:#x} a3={:#x} a4={:#x} a5={:#x}",
        cpu,
        active.pid.load(Ordering::Relaxed),
        id,
        active.args[0].load(Ordering::Relaxed),
        active.args[1].load(Ordering::Relaxed),
        active.args[2].load(Ordering::Relaxed),
        active.args[3].load(Ordering::Relaxed),
        active.args[4].load(Ordering::Relaxed),
        active.args[5].load(Ordering::Relaxed),
    );
}

#[cfg(feature = "buildstorm-diagnostics")]
pub(crate) fn dump_all_active_syscalls(reason: &str) {
    let current_cpu = crate::task::try_current_hart_id().unwrap_or(usize::MAX);
    for (cpu, active) in ACTIVE_SYSCALLS.iter().enumerate() {
        let id = active.id.load(Ordering::Acquire);
        if id == usize::MAX {
            continue;
        }
        crate::println!(
            "[buildstorm-diag] active-syscall reason={} cpu={} current={} pid={} nr={} a0={:#x} a1={:#x} a2={:#x} a3={:#x} a4={:#x} a5={:#x}",
            reason,
            cpu,
            cpu == current_cpu,
            active.pid.load(Ordering::Relaxed),
            id,
            active.args[0].load(Ordering::Relaxed),
            active.args[1].load(Ordering::Relaxed),
            active.args[2].load(Ordering::Relaxed),
            active.args[3].load(Ordering::Relaxed),
            active.args[4].load(Ordering::Relaxed),
            active.args[5].load(Ordering::Relaxed),
        );
    }
}

#[no_mangle]
pub static LAST_SYSCALL_ID: AtomicUsize = AtomicUsize::new(0);
#[no_mangle]
pub static LAST_SYSCALL_ARG0: AtomicUsize = AtomicUsize::new(0);
#[no_mangle]
pub static LAST_SYSCALL_ARG1: AtomicUsize = AtomicUsize::new(0);
#[no_mangle]
pub static LAST_SYSCALL_ARG2: AtomicUsize = AtomicUsize::new(0);
#[no_mangle]
pub static LAST_SYSCALL_RET: AtomicUsize = AtomicUsize::new(0);
#[no_mangle]
pub static LAST_SYSCALL_PID: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp-diagnostics")]
// This is enabled only in the local diagnostic build. Start at cargo new so
// the dynamic loaders for the shell, rustc and cargo --version do not consume
// the trace budget before the filesystem operation under investigation.
static SYSCALL_DIAG_BUDGET: AtomicUsize = AtomicUsize::new(0);

pub fn syscall(syscall_id: usize, args: [usize; 6]) -> isize {
    if syscall_id == SYSCALL_SHUTDOWN {
        #[cfg(feature = "smp-diagnostics")]
        crate::println!(
            "[shutdown-diag] tid={} a0={:#x} a1={:#x}",
            crate::task::current_task()
                .map(|task| task.getpid())
                .unwrap_or(0),
            args[0],
            args[1],
        );
        return sys_shutdown();
    }
    let current_pid = crate::task::current_task()
        .map(|task| task.getpid())
        .unwrap_or(0);
    #[cfg(feature = "buildstorm-diagnostics")]
    record_active_syscall(current_pid, syscall_id, &args);
    #[cfg(any(debug_assertions, feature = "smp-diagnostics", feature = "timeout-trace"))]
    {
        LAST_SYSCALL_ID.store(syscall_id, Ordering::Relaxed);
        LAST_SYSCALL_ARG0.store(args[0], Ordering::Relaxed);
        LAST_SYSCALL_ARG1.store(args[1], Ordering::Relaxed);
        LAST_SYSCALL_ARG2.store(args[2], Ordering::Relaxed);
        LAST_SYSCALL_PID.store(current_pid, Ordering::Relaxed);
    }
    #[cfg(feature = "smp-diagnostics")]
    let trace_this = current_pid >= 3
        && matches!(
            syscall_id,
            SYSCALL_SETITIMER
                | 107..=111
                | SYSCALL_FUTEX
                | SYSCALL_WAITPID
                | SYSCALL_SIGPROCMASK
                | SYSCALL_SIGRETURN
                | SYSCALL_KILL
                | SYSCALL_TKILL
                | SYSCALL_TGKILL
                | SYSCALL_EXIT
                | SYSCALL_EXIT_GROUP
        )
        && SYSCALL_DIAG_BUDGET
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .is_ok();
    #[cfg(feature = "smp-diagnostics")]
    if trace_this {
        crate::println!(
            "[syscall-diag] enter tid={} nr={} a0={:#x} a1={:#x} a2={:#x}",
            current_pid,
            syscall_id,
            args[0],
            args[1],
            args[2],
        );
    }
    // Avoid formatting on the syscall hot path; lmbench is very sensitive to this.
    let ret = match syscall_id {
        SYSCALL_DUP => sys_dup(args[0]),
        SYSCALL_DUP3 => sys_dup3(args[0], args[1], args[2]),
        SYSCALL_FCNTL => sys_fcntl(args[0], args[1], args[2]),
        SYSCALL_FLOCK => sys_flock(args[0], args[1]),
        SYSCALL_FACCESSAT => {
            sys_faccessat(args[0] as isize, args[1] as *const u8, args[2], args[3])
        }
        SYSCALL_FACCESSAT2 => {
            sys_faccessat2(args[0] as isize, args[1] as *const u8, args[2], args[3])
        }
        SYSCALL_FCHMOD => sys_fchmod(args[0], args[1] as u32),
        SYSCALL_FCHMODAT => sys_fchmodat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_FCHMODAT2 => sys_fchmodat2(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3],
        ),
        SYSCALL_FCHOWNAT => sys_fchownat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u32,
            args[4] as u32,
        ),
        SYSCALL_FCHOWN => sys_fchown(args[0], args[1] as u32, args[2] as u32),
        SYSCALL_IOCTL => sys_ioctl(args[0], args[1], args[2]),
        SYSCALL_GETCWD => sys_getcwd(args[0] as *mut u8, args[1]),
        SYSCALL_MKDIRAT => sys_mkdirat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_MKNODAT => sys_mknodat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3],
        ),
        SYSCALL_UNLINKAT => sys_unlinkat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_SYMLINKAT => {
            sys_symlinkat(args[0] as *const u8, args[1] as isize, args[2] as *const u8)
        }
        SYSCALL_LINKAT => sys_linkat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4] as u32,
        ),
        SYSCALL_FALLOCATE => sys_fallocate(args[0], args[1], args[2], args[3]),
        SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
        SYSCALL_FCHDIR => sys_fchdir(args[0]),
        SYSCALL_CHROOT => sys_chroot(args[0] as *const u8),
        SYSCALL_OPENAT => sys_openat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u32,
        ),
        SYSCALL_CLOSE => sys_close(args[0]),
        SYSCALL_PIPE2 => sys_pipe2(args[0] as *mut usize, args[1]),
        SYSCALL_GETDENTS64 => sys_getdents64(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_LSEEK => sys_lseek(args[0], args[1] as i64, args[2] as i32),
        SYSCALL_READ => sys_read(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_WRITE => sys_write(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_READV => sys_readv(args[0], args[1] as *const u8, args[2]),
        SYSCALL_WRITEV => sys_writev(args[0], args[1] as *const u8, args[2]),
        SYSCALL_PREAD64 => sys_pread64(args[0], args[1] as *mut u8, args[2], args[3]),
        SYSCALL_PWRITE64 => sys_pwrite64(args[0], args[1] as *mut u8, args[2], args[3]),
        SYSCALL_PREADV => sys_preadv(args[0], args[1] as *const u8, args[2], args[3]),
        SYSCALL_PWRITEV => sys_pwritev(args[0], args[1] as *const u8, args[2], args[3]),
        SYSCALL_PREADV2 => sys_preadv2(
            args[0],
            args[1] as *const u8,
            args[2],
            split_offset_arg(args[3], args[4]),
            args[5],
        ),
        SYSCALL_PWRITEV2 => sys_pwritev2(
            args[0],
            args[1] as *const u8,
            args[2],
            split_offset_arg(args[3], args[4]),
            args[5],
        ),
        SYSCALL_SENDFILE => sys_sendfile(args[0], args[1], args[2] as *mut usize, args[3]),
        SYSCALL_READAHEAD => sys_readahead(args[0], args[1], args[2]),
        SYSCALL_COPY_FILE_RANGE => sys_copy_file_range(
            args[0],
            args[1] as *mut usize,
            args[2],
            args[3] as *mut usize,
            args[4],
            args[5],
        ),
        SYSCALL_FUTEX => sys_futex(
            args[0] as *mut u32,
            args[1],
            args[2],
            args[3],
            args[4] as *mut u32,
            args[5],
        ),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_EXIT_GROUP => sys_exit_group(args[0] as i32),
        SYSCALL_YIELD => sys_yield(),
        SYSCALL_SYSLOG => sys_syslog(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_KILL => sys_kill(args[0] as isize, args[1] as i32),
        SYSCALL_TKILL => sys_tkill(args[0], args[1] as i32),
        SYSCALL_TGKILL => sys_tgkill(args[0], args[1], args[2] as i32),
        SYSCALL_SIGALTSTACK => sys_sigaltstack(args[0] as *const u8, args[1] as *mut u8),
        SYSCALL_SIGSUSPEND => sys_rt_sigsuspend(args[0] as *const u8, args[1]),
        SYSCALL_SIGACTION => sys_sigaction(
            args[0] as i32,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_SIGPROCMASK => sys_sigprocmask(
            args[0] as i32,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_RT_SIGPENDING => sys_rt_sigpending(args[0] as *mut u8, args[1]),
        SYSCALL_RT_SIGTIMEDWAIT => sys_rt_sigtimedwait(
            args[0] as *const u8,
            args[1] as *mut u8,
            args[2] as *const u8,
            args[3],
        ),
        SYSCALL_SIGRETURN => sys_sigreturn(),
        SYSCALL_GETITIMER => sys_getitimer(args[0], args[1] as *mut u8),
        SYSCALL_SETITIMER => sys_setitimer(args[0], args[1] as *const u8, args[2] as *mut u8),
        SYSCALL_GET_TIME => sys_get_time(args[0], args[1]),
        SYSCALL_GETPID => sys_getpid(),
        SYSCALL_GETPPID => sys_getppid(),
        SYSCALL_GETUID => sys_getuid(),
        SYSCALL_GETEUID => sys_geteuid(),
        SYSCALL_GETGID => sys_getgid(),
        SYSCALL_GETEGID => sys_getegid(),
        SYSCALL_GETTID => sys_gettid(),
        SYSCALL_GETRESUID => sys_getresuid(
            args[0] as *mut u32,
            args[1] as *mut u32,
            args[2] as *mut u32,
        ),
        SYSCALL_GETRESGID => sys_getresgid(
            args[0] as *mut u32,
            args[1] as *mut u32,
            args[2] as *mut u32,
        ),
        SYSCALL_GETRUSAGE => sys_getrusage(args[0], args[1] as *mut u8),
        SYSCALL_UMASK => sys_umask(args[0] as u32),
        SYSCALL_SETUID => sys_setuid(args[0] as u32),
        SYSCALL_SETGID => sys_setgid(args[0] as u32),
        SYSCALL_SETREUID => sys_setreuid(args[0], args[1]),
        SYSCALL_SETREGID => sys_setregid(args[0], args[1]),
        SYSCALL_SETRESUID => sys_setresuid(args[0], args[1], args[2]),
        SYSCALL_SETRESGID => sys_setresgid(args[0], args[1], args[2]),
        SYSCALL_GETGROUPS => sys_getgroups(args[0], args[1] as *mut u32),
        SYSCALL_SETGROUPS => sys_setgroups(args[0], args[1] as *const u32),
        SYSCALL_SET_TID_ADDRESS => sys_set_tid_address(args[0] as *mut i32),
        SYSCALL_CLONE => sys_clone(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_CLONE3 => sys_clone3(args[0] as *const u8, args[1]),
        SYSCALL_FORK => sys_fork(),
        SYSCALL_EXEC => sys_exec(
            args[0] as *const u8,
            args[1] as *const usize,
            args[2] as *const usize,
        ),
        SYSCALL_WAITPID => sys_waitpid(args[0] as isize, args[1] as *mut i32, args[2]),
        SYSCALL_WAITID => sys_waitid(
            args[0],
            args[1],
            args[2] as *mut u8,
            args[3],
            args[4] as *mut u8,
        ),
        //added
        SYSCALL_FSTAT => sys_fstat(args[0], args[1] as *mut u8),
        SYSCALL_SYNC => sys_sync(),
        SYSCALL_SYNCFS => sys_syncfs(args[0]),
        SYSCALL_FSYNC => sys_fsync(args[0]),
        SYSCALL_FDATASYNC => sys_fdatasync(args[0]),
        SYSCALL_FSTATAT => sys_fstatat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_READLINKAT => sys_readlinkat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_SETXATTR => sys_setxattr(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4],
            true,
        ),
        SYSCALL_LSETXATTR => sys_setxattr(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4],
            false,
        ),
        SYSCALL_FSETXATTR => sys_fsetxattr(
            args[0],
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4],
        ),
        SYSCALL_GETXATTR => sys_getxattr(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
            true,
        ),
        SYSCALL_LGETXATTR => sys_getxattr(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
            false,
        ),
        SYSCALL_FGETXATTR => sys_fgetxattr(
            args[0],
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_LISTXATTR => {
            sys_listxattr(args[0] as *const u8, args[1] as *mut u8, args[2], true)
        }
        SYSCALL_LLISTXATTR => {
            sys_listxattr(args[0] as *const u8, args[1] as *mut u8, args[2], false)
        }
        SYSCALL_FLISTXATTR => sys_flistxattr(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_REMOVEXATTR => {
            sys_removexattr(args[0] as *const u8, args[1] as *const u8, true)
        }
        SYSCALL_LREMOVEXATTR => {
            sys_removexattr(args[0] as *const u8, args[1] as *const u8, false)
        }
        SYSCALL_FREMOVEXATTR => sys_fremovexattr(args[0], args[1] as *const u8),
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_SLEEP => sys_sleep(args[0] as u64),
        SYSCALL_MMAP => sys_mmap(args[0], args[1], args[2], args[3], args[4] as i32, args[5]),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),
        SYSCALL_MREMAP => sys_mremap(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_MSYNC => sys_msync(args[0], args[1], args[2]),
        SYSCALL_MLOCK => sys_mlock(args[0], args[1]),
        SYSCALL_MUNLOCK => sys_munlock(args[0], args[1]),
        SYSCALL_MLOCKALL => sys_mlockall(args[0]),
        SYSCALL_MUNLOCKALL => sys_munlockall(),
        SYSCALL_MINCORE => sys_mincore(args[0], args[1], args[2] as *mut u8),
        SYSCALL_MLOCK2 => sys_mlock2(args[0], args[1], args[2]),
        SYSCALL_FADVISE64 => sys_fadvise64(args[0], args[1], args[2], args[3]),
        SYSCALL_SHMGET => sys_shmget(args[0], args[1], args[2]),
        SYSCALL_SHMCTL => sys_shmctl(args[0], args[1], args[2] as *mut u8),
        SYSCALL_SHMAT => sys_shmat(args[0], args[1], args[2]),
        SYSCALL_SHMDT => sys_shmdt(args[0]),
        SYSCALL_SOCKET => sys_socket(args[0], args[1], args[2]),
        SYSCALL_SOCKETPAIR => sys_socketpair(args[0], args[1], args[2], args[3] as *mut usize),
        SYSCALL_BIND => sys_bind(args[0], args[1] as *const u8, args[2]),
        SYSCALL_LISTEN => sys_listen(args[0], args[1]),
        SYSCALL_ACCEPT => sys_accept(args[0], args[1] as *mut u8, args[2] as *mut usize),
        SYSCALL_CONNECT => sys_connect(args[0], args[1] as *const u8, args[2]),
        SYSCALL_GETSOCKNAME => sys_getsockname(args[0], args[1] as *mut u8, args[2] as *mut usize),
        SYSCALL_GETPEERNAME => sys_getpeername(args[0], args[1] as *mut u8, args[2] as *mut usize),
        SYSCALL_SENDTO => sys_sendto(
            args[0],
            args[1] as *const u8,
            args[2],
            args[3],
            args[4] as *const u8,
            args[5],
        ),
        SYSCALL_RECVFROM => sys_recvfrom(
            args[0],
            args[1] as *mut u8,
            args[2],
            args[3],
            args[4] as *mut u8,
            args[5] as *mut usize,
        ),
        SYSCALL_SETSOCKOPT => {
            sys_setsockopt(args[0], args[1], args[2], args[3] as *const u8, args[4])
        }
        SYSCALL_GETSOCKOPT => sys_getsockopt(
            args[0],
            args[1],
            args[2],
            args[3] as *mut u8,
            args[4] as *mut usize,
        ),
        SYSCALL_SOCK_SHUTDOWN => sys_sock_shutdown(args[0], args[1]),
        SYSCALL_SENDMSG => sys_sendmsg(args[0], args[1] as *const u8, args[2]),
        SYSCALL_RECVMSG => sys_recvmsg(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_MPROTECT => sys_mprotect(args[0], args[1], args[2]),
        // Tier 4 stubs
        SYSCALL_SET_ROBUST_LIST => sys_set_robust_list(args[0], args[1]),
        SYSCALL_GET_ROBUST_LIST => {
            sys_get_robust_list(args[0], args[1] as *mut usize, args[2] as *mut usize)
        }
        SYSCALL_GETRANDOM => sys_getrandom(args[0] as *mut u8, args[1], args[2]),
        SYSCALL_PSELECT6 => sys_pselect6(
            args[0],
            args[1] as *mut u8,
            args[2] as *mut u8,
            args[3] as *mut u8,
            args[4] as *const u8,
            args[5] as *const u8,
        ),
        SYSCALL_PPOLL => sys_ppoll(
            args[0] as *mut u8,
            args[1],
            args[2] as *const u8,
            args[3] as *const u8,
            args[4],
        ),
        SYSCALL_SETPGID => sys_setpgid(args[0], args[1]),
        SYSCALL_GETPGID => sys_getpgid(args[0]),
        SYSCALL_GETSID => sys_getsid(args[0]),
        SYSCALL_SETSID => sys_setsid(),
        SYSCALL_MADVISE => sys_madvise(args[0], args[1], args[2]),
        SYSCALL_STATFS => sys_statfs(args[0] as *const u8, args[1] as *mut u8),
        SYSCALL_FSTATFS => sys_fstatfs(args[0], args[1] as *mut u8),
        SYSCALL_PRCTL => 0,
        SYSCALL_GETRLIMIT => sys_getrlimit(args[0], args[1] as *mut u8),
        SYSCALL_SETRLIMIT => sys_setrlimit(args[0], args[1] as *const u8),
        SYSCALL_PRLIMIT64 => {
            sys_prlimit64(args[0], args[1], args[2] as *const u8, args[3] as *mut u8)
        }
        SYSCALL_SYSINFO => sys_sysinfo(args[0] as *mut u8),
        SYSCALL_SETPRIORITY => sys_setpriority(args[0], args[1], args[2] as isize),
        SYSCALL_GETPRIORITY => sys_getpriority(args[0], args[1]),
        SYSCALL_SCHED_SETAFFINITY => sys_sched_setaffinity(args[0], args[1], args[2] as *const u8),
        SYSCALL_SCHED_GETAFFINITY => sys_sched_getaffinity(args[0], args[1], args[2] as *mut u8),
        SYSCALL_EPOLL_CREATE1 => sys_epoll_create1(args[0]),
        SYSCALL_EPOLL_CTL => sys_epoll_ctl(args[0], args[1], args[2], args[3] as *const u8),
        SYSCALL_EPOLL_PWAIT => sys_epoll_pwait(
            args[0],
            args[1] as *mut u8,
            args[2],
            args[3] as isize,
            args[4] as *const u8,
            args[5],
        ),
        SYSCALL_STATX => sys_statx(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u32,
            args[4] as *mut u8,
        ),
        SYSCALL_EVENTFD2 => sys_eventfd2(args[0] as u32, args[1]),
        SYSCALL_TIMERFD_CREATE => sys_timerfd_create(args[0] as i32, args[1]),
        SYSCALL_TIMERFD_SETTIME => sys_timerfd_settime(
            args[0],
            args[1],
            args[2] as *const u8,
            args[3] as *mut u8,
        ),
        SYSCALL_TIMERFD_GETTIME => sys_timerfd_gettime(args[0], args[1] as *mut u8),
        SYSCALL_RSEQ => -38,
        SYSCALL_RISCV_HWPROBE => -38,
        SYSCALL_TIMES => sys_times(args[0]),
        SYSCALL_UNAME => sys_uname(args[0]),
        SYSCALL_GETCPU => sys_getcpu(args[0] as *mut u32, args[1] as *mut u32, args[2]),
        SYSCALL_NANOSLEEP => sys_nanosleep(args[0], args[1]),
        SYSCALL_CLOCK_GETTIME => sys_clock_gettime(args[0], args[1] as *mut u8),
        SYSCALL_CLOCK_GETRES => sys_clock_getres(args[0], args[1] as *mut u8),
        SYSCALL_CLOCK_NANOSLEEP => sys_clock_nanosleep(args[0], args[1], args[2], args[3]),
        SYSCALL_TIMER_CREATE => {
            sys_timer_create(args[0] as i32, args[1] as *const u8, args[2] as *mut i32)
        }
        SYSCALL_TIMER_GETTIME => sys_timer_gettime(args[0] as i32, args[1] as *mut u8),
        SYSCALL_TIMER_GETOVERRUN => sys_timer_getoverrun(args[0] as i32),
        SYSCALL_TIMER_SETTIME => sys_timer_settime(
            args[0] as i32,
            args[1],
            args[2] as *const u8,
            args[3] as *mut u8,
        ),
        SYSCALL_TIMER_DELETE => sys_timer_delete(args[0] as i32),
        SYSCALL_MOUNT => sys_mount(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4] as *const u8,
        ),
        SYSCALL_UMOUNT2 => sys_umount2(args[0] as *const u8, args[1]),
        SYSCALL_UTIMENSAT => sys_utimensat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
        ),
        SYSCALL_RENAMEAT2 => sys_renameat2(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4],
        ),
        SYSCALL_TRUNCATE => sys_truncate(args[0] as *const u8, args[1] as u64),
        SYSCALL_FTRUNCATE => sys_ftruncate(args[0], args[1] as u64),
        SYSCALL_CLOSE_RANGE => sys_close_range(args[0], args[1], args[2]),
        _ => -38,
    };
    #[cfg(any(debug_assertions, feature = "smp-diagnostics", feature = "timeout-trace"))]
    LAST_SYSCALL_RET.store(ret as usize, Ordering::Relaxed);
    #[cfg(any(
        feature = "smp-diagnostics",
        feature = "buildstorm-diagnostics",
        feature = "unknown-syscall-diagnostics"
    ))]
    if ret == -38
        && !matches!(
            syscall_id,
            SYSCALL_RSEQ | SYSCALL_RISCV_HWPROBE | SYSCALL_FSOPEN
        )
    {
        crate::println!(
            "[syscall-diag] ENOSYS tid={} nr={} a0={:#x} a1={:#x} a2={:#x}",
            current_pid,
            syscall_id,
            args[0],
            args[1],
            args[2],
        );
    }
    #[cfg(feature = "buildstorm-diagnostics")]
    if ret == -16 {
        crate::println!(
            "[buildstorm-diag] EBUSY tid={} nr={} a0={:#x} a1={:#x} a2={:#x} a3={:#x} a4={:#x} a5={:#x}",
            current_pid,
            syscall_id,
            args[0],
            args[1],
            args[2],
            args[3],
            args[4],
            args[5],
        );
    }
    #[cfg(feature = "buildstorm-diagnostics")]
    if ret == -14 {
        crate::println!(
            "[buildstorm-diag] EFAULT tid={} nr={} a0={:#x} a1={:#x} a2={:#x} a3={:#x} a4={:#x} a5={:#x}",
            current_pid,
            syscall_id,
            args[0],
            args[1],
            args[2],
            args[3],
            args[4],
            args[5],
        );
    }
    #[cfg(feature = "smp-diagnostics")]
    if trace_this {
        crate::println!(
            "[syscall-diag] leave tid={} nr={} ret={}",
            current_pid,
            syscall_id,
            ret,
        );
    }
    #[cfg(feature = "smp-diagnostics")]
    if ret == -9 {
        crate::println!(
            "[syscall-diag] EBADF tid={} nr={} a0={:#x} a1={:#x} a2={:#x}",
            current_pid,
            syscall_id,
            args[0],
            args[1],
            args[2],
        );
    }
    #[cfg(feature = "buildstorm-diagnostics")]
    clear_active_syscall();
    ret
}

fn split_offset_arg(lo: usize, hi: usize) -> usize {
    if hi == 0 && lo > u32::MAX as usize {
        lo
    } else {
        (((hi as u64 & 0xffff_ffff) << 32) | (lo as u64 & 0xffff_ffff)) as usize
    }
}
