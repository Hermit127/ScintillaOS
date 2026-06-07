#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{exec, fork, waitpid, shutdown, mount, exit};

const TEST_LIST: &[&str] = &[
    "brk", "chdir", "close", "dup", "dup2", "execve", "exit",
    "fstat", "getcwd", "getdents", "getpid", "getppid", "gettimeofday",
    "mkdir_", "mmap", "mount", "munmap", "openat", "open", "pipe", "read",
    "sleep", "times", "umount", "uname", "unlink", "wait", "waitpid",
    "write", "yield",
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

fn run_iozone() {
    println!("#### OS COMP TEST GROUP START iozone ####");
    let bin = "/bin/iozone\0";

    println!("iozone throughput write/read measurements");
    run_cmd(bin, &["-i\0", "0\0", "-i\0", "1\0", "-r\0", "1k\0", "-s\0", "1m\0"]);

    println!("iozone throughput random-read measurements");
    run_cmd(bin, &["-i\0", "0\0", "-i\0", "2\0", "-r\0", "1k\0", "-s\0", "1m\0"]);

    println!("iozone throughput read-backwards measurements");
    run_cmd(bin, &["-i\0", "0\0", "-i\0", "3\0", "-r\0", "1k\0", "-s\0", "1m\0"]);

    println!("iozone throughput stride-read measurements");
    run_cmd(bin, &["-i\0", "0\0", "-i\0", "5\0", "-r\0", "1k\0", "-s\0", "1m\0"]);

    println!("iozone throughput fwrite/fread measurements");
    run_cmd(bin, &["-i\0", "6\0", "-i\0", "7\0", "-r\0", "1k\0", "-s\0", "1m\0"]);

    println!("iozone throughput pwrite/pread measurements");
    run_cmd(bin, &["-i\0", "9\0", "-i\0", "10\0", "-r\0", "1k\0", "-s\0", "1m\0"]);

    println!("iozone throughtput pwritev/preadv measurements");
    run_cmd(bin, &["-i\0", "11\0", "-i\0", "12\0", "-r\0", "1k\0", "-s\0", "1m\0"]);

    println!("#### OS COMP TEST GROUP END iozone ####");
}

#[no_mangle]
pub fn main() -> i32 {
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

    println!("===== ALL TESTS DONE =====");
    shutdown();
    0
}
