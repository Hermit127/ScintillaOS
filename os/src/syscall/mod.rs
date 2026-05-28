const SYSCALL_DUP: usize = 23;
const SYSCALL_DUP3: usize = 24;
const SYSCALL_FCNTL: usize = 25;
const SYSCALL_FACCESSAT: usize = 48;
const SYSCALL_IOCTL: usize = 29;
const SYSCALL_GETCWD: usize = 17;
const SYSCALL_MKDIRAT: usize = 34;
const SYSCALL_UNLINKAT: usize = 35;
const SYSCALL_SYMLINKAT: usize = 36;
const SYSCALL_CHDIR: usize = 49;
const SYSCALL_OPENAT: usize = 56;
const SYSCALL_CLOSE: usize = 57;
const SYSCALL_PIPE2: usize = 59;
const SYSCALL_LSEEK: usize = 62;
const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_SENDFILE: usize = 71;
const SYSCALL_WRITEV: usize = 66;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_FUTEX: usize = 98;
const SYSCALL_NANOSLEEP: usize = 101;
const SYSCALL_CLOCK_GETTIME: usize = 113;
const SYSCALL_SYSLOG: usize = 116;
const SYSCALL_YIELD: usize = 124;
const SYSCALL_KILL: usize = 129;
const SYSCALL_SIGACTION: usize = 134;
const SYSCALL_SIGPROCMASK: usize = 135;
const SYSCALL_SIGRETURN: usize = 139;
const SYSCALL_TIMES: usize = 153;
const SYSCALL_UNAME: usize = 160;
const SYSCALL_GET_TIME: usize = 169;
const SYSCALL_GETPID: usize = 172;
const SYSCALL_GETPPID: usize = 173;
const SYSCALL_GETUID: usize = 174;
const SYSCALL_GETEUID: usize = 175;
const SYSCALL_GETGID: usize = 176;
const SYSCALL_GETEGID: usize = 177;
const SYSCALL_GETTID: usize = 178;
const SYSCALL_GETRUSAGE: usize = 165;
const SYSCALL_UMASK: usize = 166;
const SYSCALL_SETUID: usize = 146;
const SYSCALL_SETGID: usize = 144;
const SYSCALL_BRK: usize = 214;
const SYSCALL_MUNMAP: usize = 215;
const SYSCALL_READV: usize = 65;
const SYSCALL_MPROTECT: usize = 226;
const SYSCALL_SETPGID: usize = 154;
const SYSCALL_GETPGID: usize = 155;
const SYSCALL_MADVISE: usize = 233;
const SYSCALL_SET_ROBUST_LIST: usize = 99;
const SYSCALL_GETRANDOM: usize = 278;
const SYSCALL_PSELECT6: usize = 72;
const SYSCALL_PPOLL: usize = 73;
const SYSCALL_STATFS: usize = 43;
const SYSCALL_FSTATFS: usize = 44;
const SYSCALL_PRCTL: usize = 167;
const SYSCALL_PRLIMIT64: usize = 261;
const SYSCALL_SYSINFO: usize = 179;
const SYSCALL_SCHED_GETAFFINITY: usize = 123;
const SYSCALL_EPOLL_CREATE1: usize = 20;
const SYSCALL_STATX: usize = 291;
const SYSCALL_EVENTFD2: usize = 290;
const SYSCALL_RSEQ: usize = 293;
const SYSCALL_RISCV_HWPROBE: usize = 258;
const SYSCALL_CLONE: usize = 220;
const SYSCALL_FORK: usize = 401;
const SYSCALL_EXEC: usize = 221;
const SYSCALL_MMAP: usize = 222;
const SYSCALL_WAITPID: usize = 260;

const SYSCALL_GETDENTS64: usize = 61;
const SYSCALL_FSTATAT: usize = 79;
const SYSCALL_READLINKAT: usize = 78;
const SYSCALL_FSTAT: usize = 80;
const SYSCALL_SLEEP: usize = 300;
const SYSCALL_DUP2: usize = 33;

const SYSCALL_MOUNT: usize = 40;
const SYSCALL_UMOUNT2: usize = 39;

const SYSCALL_UTIMENSAT: usize = 88;

const SYSCALL_RENAMEAT2: usize = 276;

const SYSCALL_SOCKET: usize = 198;
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

const SYSCALL_SET_TID_ADDRESS: usize = 96;
const SYSCALL_EXIT_GROUP: usize = 94;


mod fs;
mod process;

use fs::*;
use process::*;
use crate::net::syscall::{
    sys_socket, sys_bind, sys_listen, sys_accept, sys_connect,
    sys_getsockname, sys_getpeername, sys_sendto, sys_recvfrom,
    sys_setsockopt, sys_getsockopt, sys_sock_shutdown,
    sys_sendmsg, sys_recvmsg,
};
use log::*;
use crate::task::SignalAction;

pub fn syscall(syscall_id: usize, args: [usize; 6]) -> isize {
    trace!("syscall: id: {}, args: {:?}", syscall_id, args);
    match syscall_id {
        SYSCALL_DUP => sys_dup(args[0]),
        SYSCALL_DUP3 => sys_dup3(args[0], args[1], args[2]),
        SYSCALL_FCNTL => sys_fcntl(args[0], args[1], args[2]),
        SYSCALL_FACCESSAT => sys_faccessat(args[0] as isize, args[1] as *const u8, args[2], args[3]),
        SYSCALL_IOCTL => sys_ioctl(args[0], args[1], args[2]),
        SYSCALL_GETCWD => sys_getcwd(args[0] as *mut u8, args[1]),
        SYSCALL_MKDIRAT => sys_mkdirat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_UNLINKAT => sys_unlinkat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_SYMLINKAT => sys_symlinkat(args[0] as *const u8, args[1] as isize, args[2] as *const u8),
        SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
        SYSCALL_OPENAT => sys_openat(args[0] as isize, args[1] as *const u8, args[2] as u32, args[3] as u32),
        SYSCALL_CLOSE => sys_close(args[0]),
        SYSCALL_PIPE2 => sys_pipe2(args[0] as *mut usize, args[1]),
        SYSCALL_GETDENTS64 => sys_getdents64(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_LSEEK => sys_lseek(args[0], args[1] as i64, args[2] as i32),
        SYSCALL_READ => sys_read(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_WRITE => sys_write(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_READV => sys_readv(args[0], args[1] as *const u8, args[2]),
        SYSCALL_WRITEV => sys_writev(args[0], args[1] as *const u8, args[2]),
        SYSCALL_SENDFILE => sys_sendfile(args[0], args[1], args[2] as *mut usize, args[3]),
        SYSCALL_FUTEX => sys_futex(args[0] as *mut u32, args[1], args[2], args[3], args[4] as *mut u32, args[5]),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_EXIT_GROUP => sys_exit_group(args[0] as i32),
        SYSCALL_YIELD => sys_yield(),
        SYSCALL_SYSLOG => sys_syslog(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_KILL => sys_kill(args[0], args[1] as i32),
        SYSCALL_SIGACTION => sys_sigaction(
            args[0] as i32,
            args[1] as *const SignalAction,
            args[2] as *mut SignalAction,
        ),
        SYSCALL_SIGPROCMASK => sys_sigprocmask(args[0] as u32),
        SYSCALL_SIGRETURN => sys_sigreturn(),
        SYSCALL_GET_TIME => sys_get_time(args[0], args[1]),
        SYSCALL_GETPID => sys_getpid(),
        SYSCALL_GETPPID => sys_getppid(),
        SYSCALL_GETUID => sys_getuid(),
        SYSCALL_GETEUID => sys_geteuid(),
        SYSCALL_GETGID => sys_getgid(),
        SYSCALL_GETEGID => sys_getegid(),
        SYSCALL_GETTID => sys_gettid(),
        SYSCALL_GETRUSAGE => sys_getrusage(args[0], args[1] as *mut u8),
        SYSCALL_UMASK => sys_umask(args[0] as u32),
        SYSCALL_SETUID => sys_setuid(args[0] as u32),
        SYSCALL_SETGID => sys_setgid(args[0] as u32),
        SYSCALL_SET_TID_ADDRESS => sys_set_tid_address(args[0] as *mut i32),
        SYSCALL_CLONE => sys_clone(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_FORK => sys_fork(),
        SYSCALL_EXEC => sys_exec(args[0] as *const u8, args[1] as *const usize),
        SYSCALL_WAITPID => sys_waitpid(args[0] as isize, args[1] as *mut i32, args[2]),
        //added
        SYSCALL_FSTAT => sys_fstat(args[0], args[1] as *mut u8),
        SYSCALL_FSTATAT => sys_fstatat(args[0] as isize, args[1] as *const u8, args[2] as *mut u8, args[3]),
        SYSCALL_READLINKAT => sys_readlinkat(args[0] as isize, args[1] as *const u8, args[2] as *mut u8, args[3]),
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_SLEEP => sys_sleep(args[0] as u64),
        // keep x86_64 dup2 syscall number for compatibility
        SYSCALL_DUP2 => sys_dup2(args[0], args[1]),
        SYSCALL_MMAP => sys_mmap(args[0], args[1], args[2], args[3], args[4] as i32, args[5]),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),
        SYSCALL_SOCKET => sys_socket(args[0], args[1], args[2]),
        SYSCALL_BIND => sys_bind(args[0], args[1] as *const u8, args[2]),
        SYSCALL_LISTEN => sys_listen(args[0], args[1]),
        SYSCALL_ACCEPT => sys_accept(args[0], args[1] as *mut u8, args[2] as *mut usize),
        SYSCALL_CONNECT => sys_connect(args[0], args[1] as *const u8, args[2]),
        SYSCALL_GETSOCKNAME => sys_getsockname(args[0], args[1] as *mut u8, args[2] as *mut usize),
        SYSCALL_GETPEERNAME => sys_getpeername(args[0], args[1] as *mut u8, args[2] as *mut usize),
        SYSCALL_SENDTO => sys_sendto(args[0], args[1] as *const u8, args[2], args[3], args[4] as *const u8, args[5]),
        SYSCALL_RECVFROM => sys_recvfrom(args[0], args[1] as *mut u8, args[2], args[3], args[4] as *mut u8, args[5] as *mut usize),
        SYSCALL_SETSOCKOPT => sys_setsockopt(args[0], args[1], args[2], args[3] as *const u8, args[4]),
        SYSCALL_GETSOCKOPT => sys_getsockopt(args[0], args[1], args[2], args[3] as *mut u8, args[4] as *mut usize),
        SYSCALL_SOCK_SHUTDOWN => sys_sock_shutdown(args[0], args[1]),
        SYSCALL_SENDMSG => sys_sendmsg(args[0], args[1] as *const u8, args[2]),
        SYSCALL_RECVMSG => sys_recvmsg(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_MPROTECT => sys_mprotect(args[0], args[1], args[2]),
        // Tier 4 stubs
        SYSCALL_SET_ROBUST_LIST => 0,
        SYSCALL_GETRANDOM => sys_getrandom(args[0] as *mut u8, args[1], args[2]),
        SYSCALL_PSELECT6 => sys_pselect6(args[0], args[1] as *mut u8, args[2] as *mut u8, args[3] as *mut u8, args[4] as *const u8, args[5] as *const u8),
        SYSCALL_PPOLL => sys_ppoll(args[0] as *mut u8, args[1], args[2] as *const u8, args[3] as *const u8, args[4]),
        SYSCALL_SETPGID => 0,   // no-op: single process group
        SYSCALL_GETPGID => 0,   // no-op: always pgid=0
        SYSCALL_MADVISE => sys_madvise(args[0], args[1], args[2]),
        SYSCALL_STATFS => sys_statfs(args[0] as *const u8, args[1] as *mut u8),
        SYSCALL_FSTATFS => sys_fstatfs(args[0], args[1] as *mut u8),
        SYSCALL_PRCTL => 0,
        SYSCALL_PRLIMIT64 => 0,
        SYSCALL_SYSINFO => -38,
        SYSCALL_SCHED_GETAFFINITY => -38,
        SYSCALL_EPOLL_CREATE1 => -38,
        SYSCALL_STATX => sys_statx(args[0] as isize, args[1] as *const u8, args[2] as u32, args[3] as u32, args[4] as *mut u8),
        SYSCALL_EVENTFD2 => -38,
        SYSCALL_RSEQ => -38,
        SYSCALL_RISCV_HWPROBE => -38,
        SYSCALL_TIMES => sys_times(args[0]),
        SYSCALL_UNAME => sys_uname(args[0]),
        SYSCALL_NANOSLEEP => sys_nanosleep(args[0], args[1]),
        SYSCALL_CLOCK_GETTIME => sys_clock_gettime(args[0], args[1] as *mut u8),
        SYSCALL_MOUNT => sys_mount(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4] as *const u8,
        ),
        SYSCALL_UMOUNT2 => sys_umount2(args[0] as *const u8, args[1]),
        SYSCALL_UTIMENSAT => sys_utimensat(
            args[0] as isize, args[1] as *const u8,
            args[2] as *const u8, args[3],
        ),
        SYSCALL_SHUTDOWN => sys_shutdown(),
        SYSCALL_RENAMEAT2 => sys_renameat2(
            args[0] as isize, args[1] as *const u8,
            args[2] as isize, args[3] as *const u8,
            args[4],
        ),
        _ => -38,
    }
}
