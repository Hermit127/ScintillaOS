#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::format;
use user_lib::{exec, fork, waitpid, open, close, read, getdents64, OpenFlags};

const LTP_DIR: &str = "/ltp/testcases/bin/";

const SUBSET_LIST: &[&str] = &[
    "abort01", "abs01", "accept01", "accept02",
    "accept03", "accept4_01", "access01", "access02",
    "access03", "access04", "acct01", "acct02",
    "acl1", "add_key01", "add_key02", "add_key03",
    "add_key04", "add_key05", "adjtimex01", "adjtimex02",
    "adjtimex03", "af_alg01", "af_alg02", "af_alg03",
    "af_alg04", "af_alg05", "af_alg06", "af_alg07",
    "aio-stress", "aio01", "aio02", "aiocp",
    "aiodio_append", "aiodio_sparse", "alarm02", "alarm03",
    "alarm05", "alarm06", "alarm07", "arch_prctl01",
    "asapi_01", "asapi_02", "asapi_03", "aslr01",
    "atof01", "autogroup01", "bind01", "bind02",
    "bind03", "bind04", "bind05", "bind06",
    "block_dev", "bpf_map01", "bpf_prog01", "bpf_prog02",
    "bpf_prog03", "bpf_prog04", "bpf_prog05", "bpf_prog06",
    "bpf_prog07", "cacheflush01", "can_bcm01", "can_filter",
    "can_rcv_own_msgs", "cap_bounds_r", "cap_bounds_rw", "cap_bset_inh_bounds",
    "capget01", "capget02", "capset01", "capset02",
    "capset03", "capset04", "cfs_bandwidth01", "chdir01",
    "chdir04",
    "check_envval", "check_keepcaps", "check_netem", "check_pe",
    "check_setkey", "check_simple_capset", "chmod01", "chmod03",
    "chmod05", "chmod06", "chmod07", "chown01",
    "chown02", "chown03", "chown04", "chown05",
    "chroot01", "chroot02", "chroot03", "chroot04",
    "clock_adjtime01", "clock_adjtime02", "clock_getres01", "clock_gettime01",
    "clock_gettime02", "clock_gettime03", "clock_gettime04", "clock_nanosleep01",
    "clock_nanosleep02", "clock_nanosleep03", "clock_settime01",
    "clock_settime02", "clock_settime03", "clone01", "clone02",
    "clone03", "clone04", "clone05", "clone06",
    "clone07", "clone08", "clone09", "clone301",
    "clone302", "clone303", "close01", "close02",
    "close_range01", "close_range02", "confstr01", "connect01",
    "connect02", "copy_file_range01", "copy_file_range02", "copy_file_range03",
    "cpuacct_task", "cpuctl_def_task01", "cpuctl_def_task02", "cpuctl_def_task03",
    "cpuctl_def_task04", "cpuctl_fj_cpu-hog", "cpuctl_fj_simple_echo", "cpuctl_latency_check_task",
    "cpuctl_latency_test", "cpuctl_test01", "cpuctl_test02", "cpuctl_test03",
    "cpuctl_test04", "cpufreq_boost", "cpuhotplug_do_disk_write_loop", "cpuhotplug_do_kcompile_loop",
    "cpuhotplug_do_spin_loop", "cpuhotplug_report_proc_interrupts", "cpuset01", "crash01",
    "creat01", "creat03", "creat04", "creat05",
    "creat06", "creat07", "creat08", "creat09",
    "create_datafile", "create_file", "crypto_user01", "crypto_user02",
    "cve-2014-0196", "cve-2015-3290", "cve-2016-10044", "cve-2016-7042",
    "cve-2016-7117", "cve-2017-16939", "delete_module03", "dup01",
    "dup02", "dup03", "dup04", "dup05",
    "dup06", "dup07", "epoll_create1_01", "epoll_create1_02",
    "epoll_ctl01", "epoll_ctl02", "epoll_ctl03", "epoll_pwait01",
    "epoll_pwait03", "epoll_wait01", "epoll_wait03", "eventfd01",
    "eventfd02", "eventfd03", "eventfd04", "eventfd05",
    "eventfd2_01", "eventfd2_02", "execve01", "execve02",
    "execve03", "execve04", "execve05", "exit01",
    "exit02", "faccessat01", "faccessat02", "fallocate01",
    "fallocate02", "fallocate03", "fallocate04", "fallocate05",
    "fallocate06", "fchdir01", "fchdir02", "fchdir03",
    "fchmod01", "fchmod02", "fchmod03", "fchmod04",
    "fchmod05", "fchmod06", "fchown01", "fchown02",
    "fchown03", "fchown04", "fchown05", "fcntl01",
    "fcntl02", "fcntl03", "fcntl04", "fcntl05",
    "fcntl07", "fcntl08", "fcntl09", "fcntl10",
    "fcntl11", "fcntl12", "fcntl13", "fcntl15",
    "fcntl16", "fcntl17", "fcntl18", "fcntl19",
    "fcntl20", "fcntl21", "fcntl22", "fcntl23",
    "fcntl24", "fcntl25", "fcntl26", "fcntl27",
    "fcntl29", "fcntl30", "fcntl31", "fcntl32",
    "fcntl33", "fcntl34", "fcntl35", "fcntl36",
    "fcntl37", "fcntl38", "fcntl39", "fdatasync01",
    "fdatasync02", "fdatasync03", "fgetxattr01", "fgetxattr02",
    "fgetxattr03", "flistxattr01", "flistxattr02", "flistxattr03",
    "flock01", "flock02", "flock03", "flock04",
    "flock06", "fork01", "fork03", "fork04",
    "fork05", "fork07", "fork08", "fork09",
    "fork10", "fork13", "fork14", "fpathconf01",
    "fremovexattr01", "fstat03", "fstatfs01", "fstatfs02",
    "fsync01", "fsync02", "fsync03", "fsync04",
    "ftruncate01", "ftruncate03", "ftruncate04", "get_mempolicy01",
    "get_robust_list01", "getcpu01", "getcwd01", "getcwd02",
    "getcwd03", "getcwd04", "getdents01", "getdents02",
    "getdomainname01", "getegid01", "getegid02", "geteuid01",
    "geteuid02", "getgid01", "gethostname01", "gethostname02",
    "getitimer01", "getitimer02", "getpagesize01", "getpeername01",
    "getpgid01", "getpgid02", "getpid01", "getpid02",
    "getppid01", "getppid02", "getpriority01", "getpriority02",
    "getrandom01", "getrandom02", "getrandom03", "getrandom04",
    "getresgid01", "getresuid01", "getrlimit01", "getrlimit02",
    "getrusage01", "getrusage02", "getrusage03", "getrusage04",
    "getsid01", "getsid02", "getsockname01", "getsockopt01",
    "getsockopt02", "gettid01", "gettid02", "gettimeofday01",
    "gettimeofday02", "getuid01", "getxattr01", "getxattr02",
    "getxattr03", "ioctl01", "ioctl02", "ioctl03",
    "ioctl04", "ioctl05", "ioctl06", "ioctl07",
    "keyctl01", "keyctl02", "keyctl03", "keyctl04",
    "kill02", "kill03", "kill05", "kill06",
    "kill07", "kill08", "kill09",
     "kill12", "kill13", "lgetxattr01",
    "link02", "link04", "link05", "listen01",
    "llistxattr01", "lremovexattr01", "lseek01", "lseek02",
    "lseek07", "lseek11", "madvise01", "madvise02",
    "madvise03", "madvise05", "membarrier01", "memfd_create01",
    "memfd_create02", "memfd_create03", "memfd_create04", "migrate_pages01",
    "mincore01", "mincore02", "mincore03", "mincore04",
    "mkdir03", "mkdir04", "mkdir05", "mkdirat01",
    "mkdirat02", "mknod01", "mknod02", "mknod03",
    "mknod04", "mknod05", "mknod06", "mknod07",
    "mknod08", "mknod09", "mknodat01", "mknodat02",
    "mlock01", "mlock02", "mlock03", "mlock04",
    "mlock05", "mlockall01", "mmap01", "mmap02",
    "mmap03", "mmap04", "mmap05", "mmap06",
    "mmap08", "mmap09", "mmap10", "mmap11",
    "mmap12", "mmap13", "mmap14", "mmap15",
    "mmap16", "mmap17", "mmap19", "mmap20",
    "modify_ldt01", "modify_ldt02", "mount01", "mount02",
    "mount03", "mount04", "mount05", "mount06",
    "mount07", "move_pages02", "mprotect01", "mprotect02",
    "mprotect03", "mprotect04", "mprotect05", "mq_notify01",
    "mq_open01", "mq_timedreceive01", "mq_timedsend01", "mq_unlink01",
    "mremap01", "mremap02", "mremap03", "mremap04",
    "mremap05", "mremap06", "msync01", "msync02",
    "msync03", "msync04", "munlock01", "munlockall01",
    "munmap01", "munmap02", "munmap03", "nanosleep01",
    "nanosleep02", "nanosleep04", "newuname01", "nice01",
    "nice02", "nice03", "nice04", "nice05",
    "open01", "open02", "open03", "open04",
    "open06", "open07", "open08", "open09",
    "open10", "open11", "open12", "open13",
    "open14", "pathconf01", "pause01", "pause02",
    "pause03", "personality01", "personality02", "pipe01",
    "pipe02", "pipe03", "pipe04", "pipe05",
    "pipe06", "pipe07", "pipe08", "pipe09",
    "pipe10", "pipe11", "pipe12", "pipe13",
    "pipe14", "pipe15", "poll01", "poll02",
    "ppoll01", "prctl01", "prctl02", "prctl03",
    "prctl04", "prctl05", "prctl06", "prctl07",
    "prctl08", "pread01", "pread02", "preadv01",
    "preadv02", "preadv03", "profil01", "pselect01",
    "pselect02", "pselect03", "pwrite01", "pwrite02",
    "pwrite03", "pwrite04", "pwritev01", "pwritev02",
    "pwritev03", "quotactl01", "quotactl02", "quotactl03",
    "quotactl04", "read01", "read02", "read03",
    "read04", "readahead01", "readahead02", "readdir01",
    "readlink01", "readlink03", "readv01", "readv02",
    "realpath01", "reboot01", "reboot02", "recv01",
    "recvfrom01", "recvmmsg01", "recvmsg01", "recvmsg02",
    "recvmsg03", "removexattr01", "rename01", "rename03",
    "rename04", "rename05", "rename06", "rename07",
    "rename08", "rename09", "rename10", "rename11",
    "rename12", "rename13", "rename14", "request_key01",
    "request_key02", "request_key03", "request_key04", "rmdir01",
    "rmdir02", "rmdir03", "rt_sigqueueinfo01", "sched_get_priority_max01",
    "sched_get_priority_min01", "sched_getattr01", "sched_getattr02", "sched_getparam01",
    "sched_getscheduler01", "sched_rr_get_interval01", "sched_setattr01", "sched_setparam01",
    "sched_setscheduler01", "select01", "select02", "select03",
    "select04", "send01", "send02", "sendfile02",
    "sendfile03", "sendfile04_64", "sendfile05", "sendfile06",
    "sendfile07", "sendfile08", "sendfile09", "sendmmsg01",
    "sendmmsg02", "sendmsg01", "sendmsg02", "sendmsg03",
    "sendto01", "sendto02", "sendto03", "setdomainname01",
    "setdomainname02", "setdomainname03", "setfsgid01", "setfsgid02",
    "setfsuid01", "setfsuid02", "sethostname01", "sethostname02",
    "setitimer01", "setitimer02", "setpgid01", "setpgid02",
    "setpgid03", "setpriority01", "setpriority02", "setregid01",
    "setresgid01", "setresuid01", "setreuid01", "setrlimit01",
    "setrlimit02", "setsid01", "setsockopt01", "setsockopt02",
    "settimeofday01", "settimeofday02", "setxattr01", "setxattr02",
    "setxattr03", "sigaction01", "sigaction02", "sigaltstack01",
    "sigaltstack02", "signalfd01", "signalfd4_01", "sigpending02",
    "sigsuspend01", "socket01",
    "socket02", "socketpair01", "socketpair02", "splice01",
    "splice02", "splice03", "splice04", "stat01",
    "stat02", "stat03", "statfs01", "statfs02",
    "statfs03", "statvfs01", "statvfs02", "statx01",
    "statx02", "statx03", "statx04", "statx05",
    "statx06", "statx07", "swapoff01", "swapon01",
    "symlink01", "symlink02", "symlink03", "symlink04",
    "sync01", "sync_file_range01", "sync_file_range02", "syncfs01",
    "sysctl01", "sysinfo01", "sysinfo02", "sysinfo03",
    "tee01", "tee02", "tgkill01", "timer_delete01",
    "timer_delete02", "timer_getoverrun01", "timer_gettime01", "timer_settime01",
    "timerfd_create01", "timerfd_gettime01", "timerfd_settime01", "timerfd_settime02",
    "times01", "times03", "truncate02", "truncate03",
    "umask01", "umount01", "umount02", "umount03",
    "uname01", "uname02", "uname04", "unlink05",
    "unlink07", "unlink08", "unlink09", "ustat01",
    "ustat02", "utime01", "utime02", "utime03",
    "utime04", "utime05", "utime06", "utimensat01",
    "vfork01", "vfork02", "vhangup01", "vmsplice01",
    "vmsplice02", "vmsplice03", "vmsplice04", "wait01",
    "wait02", "waitpid01", "waitpid03", "waitpid04",
    "waitpid06", "waitpid07", "waitpid08", "waitpid09",
    "waitpid10", "write01", "write02", "write03",
    "write04", "write05", "write06", "writev01",
    "writev02", "writev03", "writev05", "writev06",
];

fn is_elf(name: &str) -> bool {
    let path = format!("{}{}\0", LTP_DIR, name);
    let fd = open(path.as_str(), OpenFlags::RDONLY);
    if fd < 0 {
        return false;
    }
    let mut magic = [0u8; 4];
    let n = read(fd as usize, &mut magic);
    close(fd as usize);
    n == 4 && magic[0] == 0x7f && magic[1] == b'E' && magic[2] == b'L' && magic[3] == b'F'
}

fn run_test(name: &str) {
    let path = format!("{}{}\0", LTP_DIR, name);
    println!("RUN LTP CASE {}", name);
    let pid = fork();
    if pid == 0 {
        let args: &[*const u8] = &[path.as_ptr() as *const u8, core::ptr::null::<u8>()];
        exec(path.as_str(), args);
        user_lib::exit(-1);
    }
    let mut exit_code: i32 = 0;
    waitpid(pid as usize, &mut exit_code);
    let status = exit_code >> 8;
    println!("FINISH LTP CASE {} : {}", name, status);
}

fn is_full_mode() -> bool {
    let fd = open("/ltp_full\0", OpenFlags::RDONLY);
    if fd >= 0 {
        close(fd as usize);
        true
    } else {
        false
    }
}

fn skip_name(name: &str) -> bool {
    name == "." || name == ".." || name.ends_with(".sh") || name.ends_with(".py")
}

fn for_each_dirent(buf: &[u8], n: isize, mut f: impl FnMut(&str)) {
    let n = n as usize;
    let mut pos: usize = 0;
    while pos < n {
        let reclen = (buf[pos + 16] as usize) | ((buf[pos + 17] as usize) << 8);
        if reclen == 0 {
            return;
        }
        let name_start = pos + 19;
        let mut name_end = name_start;
        while name_end < pos + reclen && buf[name_end] != 0 {
            name_end += 1;
        }
        if let Ok(name) = core::str::from_utf8(&buf[name_start..name_end]) {
            if !skip_name(name) {
                f(name);
            }
        }
        pos += reclen;
    }
}

#[no_mangle]
pub fn main() -> i32 {
    println!("========================================");
    println!("  LTP RUNNER STARTING (v2)");
    println!("========================================");
    let full = is_full_mode();
    if full {
        println!("#### OS COMP TEST GROUP START ltp-musl ####");
        let fd = open(LTP_DIR, OpenFlags::RDONLY | OpenFlags::DIRECTORY);
        if fd < 0 {
            println!("LTP_DIR_NOT_FOUND: {}", LTP_DIR);
            return -1;
        }
        let mut buf = [0u8; 4096];
        let mut total: usize = 0;
        loop {
            let n = getdents64(fd as usize, &mut buf);
            if n <= 0 { break; }
            for_each_dirent(&buf, n, |name| {
                if is_elf(name) {
                    run_test(name);
                    total += 1;
                }
            });
        }
        close(fd as usize);
        println!("total LTP cases: {}", total);
    } else {
        println!("#### OS COMP TEST GROUP START ltp-subset-musl ####");
        for test in SUBSET_LIST {
            run_test(test);
        }
    }

    println!("#### OS COMP TEST GROUP END ltp-musl ####");
    0
}
