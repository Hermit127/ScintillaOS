#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{chdir, exec, exit, fork, mount, shutdown, waitpid};

const TEST_LIST: &[&str] = &[
    "brk",
    "chdir",
    "close",
    "dup",
    "dup2",
    "execve",
    "exit",
    "fstat",
    "getcwd",
    "getdents",
    "getpid",
    "getppid",
    "gettimeofday",
    "mkdir_",
    "mmap",
    "mount",
    "munmap",
    "openat",
    "open",
    "pipe",
    "read",
    "sleep",
    "times",
    "umount",
    "uname",
    "unlink",
    "wait",
    "waitpid",
    "write",
    "yield",
];

fn run_test(name: &str) -> i32 {
    let path = format!("/bin/{}\0", name);
    println!("Testing {} :", name);
    let pid = fork();
    if pid == 0 {
        exec(path.as_str(), &[core::ptr::null::<u8>()]);
    }
    let mut exit_code: i32 = -1;
    waitpid(pid as usize, &mut exit_code);
    exit_code
}

fn run_cmd(path: &str, args: &[&str]) -> i32 {
    let pid = fork();
    if pid == 0 {
        let mut c_args: Vec<String> = Vec::new();
        c_args.push(format!("{}\0", path));
        for arg in args {
            c_args.push(format!("{}\0", arg));
        }
        let mut ptrs: Vec<*const u8> = c_args.iter().map(|s| s.as_ptr() as *const u8).collect();
        ptrs.push(core::ptr::null::<u8>());
        exec(path, &ptrs);
        exit(-1);
    }
    let mut exit_code: i32 = -1;
    waitpid(pid as usize, &mut exit_code);
    exit_code
}

fn run_script(dir: &str, script: &str) {
    let pid = fork();
    if pid == 0 {
        if !dir.is_empty() {
            chdir(dir);
        }
        let ptrs: [*const u8; 4] = [
            b"./busybox\0".as_ptr(),
            b"sh\0".as_ptr(),
            script.as_ptr(),
            core::ptr::null::<u8>(),
        ];
        exec("./busybox\0", &ptrs[..3]);
        exit(127);
    }
    let mut exit_code: i32 = 0;
    waitpid(pid as usize, &mut exit_code);
}

fn run_libctest(dir: &str) -> bool {
    if chdir(dir) != 0 {
        return false;
    }
    println!("#### OS COMP TEST GROUP START libctest ####");
    let static_tests = &[
        "argv",
        "basename",
        "clocale_mbfuncs",
        "clock_gettime",
        "dirname",
        "env",
        "fdopen",
        "fnmatch",
        "fscanf",
        "fwscanf",
        "iconv_open",
        "inet_pton",
        "mbc",
        "memstream",
        "pthread_cancel_points",
        "pthread_cancel",
        "pthread_cond",
        "pthread_tsd",
        "qsort",
        "random",
        "search_hsearch",
        "search_insque",
        "search_lsearch",
        "search_tsearch",
        "setjmp",
        "snprintf",
        "socket",
        "sscanf",
        "sscanf_long",
        "stat",
        "strftime",
        "string",
        "string_memcpy",
        "string_memmem",
        "string_memset",
        "string_strchr",
        "string_strcspn",
        "string_strstr",
        "strptime",
        "strtod",
        "strtod_simple",
        "strtof",
        "strtol",
        "strtold",
        "swprintf",
        "tgmath",
        "time",
        "tls_align",
        "udiv",
        "ungetc",
        "utime",
        "wcsstr",
        "wcstol",
    ];
    for name in static_tests {
        let pid = fork();
        if pid == 0 {
            chdir(dir);
            let name_z = format!("{}\0", name);
            let ptrs: [*const u8; 5] = [
                b"./runtest.exe\0".as_ptr(),
                b"-w\0".as_ptr(),
                b"entry-static.exe\0".as_ptr(),
                name_z.as_ptr(),
                core::ptr::null::<u8>(),
            ];
            exec("./runtest.exe\0", &ptrs[..4]);
            exit(127);
        }
        let mut exit_code: i32 = 0;
        waitpid(pid as usize, &mut exit_code);
    }
    chdir("/\0");
    println!("#### OS COMP TEST GROUP END libctest ####");
    true
}

fn run_iozone() {
    println!("#### OS COMP TEST GROUP START iozone ####");
    let bin = "/bin/iozone\0";

    println!("iozone throughput write/read measurements");
    run_cmd(
        bin,
        &["-i\0", "0\0", "-i\0", "1\0", "-r\0", "1k\0", "-s\0", "1m\0"],
    );

    println!("iozone throughput random-read measurements");
    run_cmd(
        bin,
        &["-i\0", "0\0", "-i\0", "2\0", "-r\0", "1k\0", "-s\0", "1m\0"],
    );

    println!("iozone throughput read-backwards measurements");
    run_cmd(
        bin,
        &["-i\0", "0\0", "-i\0", "3\0", "-r\0", "1k\0", "-s\0", "1m\0"],
    );

    println!("iozone throughput stride-read measurements");
    run_cmd(
        bin,
        &["-i\0", "0\0", "-i\0", "5\0", "-r\0", "1k\0", "-s\0", "1m\0"],
    );

    println!("iozone throughput fwrite/fread measurements");
    run_cmd(
        bin,
        &["-i\0", "6\0", "-i\0", "7\0", "-r\0", "1k\0", "-s\0", "1m\0"],
    );

    println!("iozone throughput pwrite/pread measurements");
    run_cmd(
        bin,
        &[
            "-i\0", "9\0", "-i\0", "10\0", "-r\0", "1k\0", "-s\0", "1m\0",
        ],
    );

    println!("iozone throughtput pwritev/preadv measurements");
    run_cmd(
        bin,
        &[
            "-i\0", "11\0", "-i\0", "12\0", "-r\0", "1k\0", "-s\0", "1m\0",
        ],
    );

    println!("#### OS COMP TEST GROUP END iozone ####");
}

#[no_mangle]
pub fn main() -> i32 {
    println!("[contest_runner] started!");
    mount("", "/mnt\0", "ext4\0", 0, &[]);

    println!("#### OS COMP TEST GROUP START basic-musl ####");
    for test in TEST_LIST {
        run_test(test);
    }
    println!("#### OS COMP TEST GROUP END basic-musl ####");

    println!("#### OS COMP TEST GROUP START basic-glibc ####");
    for test in TEST_LIST {
        run_test(test);
    }
    println!("#### OS COMP TEST GROUP END basic-glibc ####");

    run_iozone();

    run_libctest("/libctest/glibc\0");

    println!("===== ALL TESTS DONE =====");
    shutdown();
    0
}
