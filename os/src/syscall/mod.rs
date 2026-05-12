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
const SYSCALL_EXIT: usize = 93;
const SYSCALL_NANOSLEEP: usize = 101;
const SYSCALL_CLOCK_GETTIME: usize = 113;
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
const SYSCALL_SETUID: usize = 146;
const SYSCALL_SETGID: usize = 144;
const SYSCALL_BRK: usize = 214;
const SYSCALL_MUNMAP: usize = 215;
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

const SYSCALL_SHUTDOWN: usize = 403;

const SYSCALL_RENAMEAT2: usize = 276;

const SYSCALL_SET_TID_ADDRESS: usize = 96;
const SYSCALL_EXIT_GROUP: usize = 94;


mod fs;
mod process;

use fs::*;
use process::*;
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
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_EXIT_GROUP => sys_exit_group(args[0] as i32),
        SYSCALL_YIELD => sys_yield(),
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
        SYSCALL_SETUID => sys_setuid(args[0] as u32),
        SYSCALL_SETGID => sys_setgid(args[0] as u32),
        SYSCALL_SET_TID_ADDRESS => sys_set_tid_address(args[0] as *mut i32),
        SYSCALL_CLONE => sys_clone(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_FORK => sys_fork(),
        SYSCALL_EXEC => sys_exec(args[0] as *const u8, args[1] as *const usize),
        SYSCALL_WAITPID => sys_waitpid(args[0] as isize, args[1] as *mut i32),
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
        SYSCALL_SHUTDOWN => sys_shutdown(),
        SYSCALL_RENAMEAT2 => sys_renameat2(
            args[0] as isize, args[1] as *const u8,
            args[2] as isize, args[3] as *const u8,
            args[4],
        ),
        _ => -1,
    }
}
