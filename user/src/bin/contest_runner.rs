#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::format;
use user_lib::{exec, fork, waitpid};

const TEST_LIST: &[&str] = &[
    "brk",
    "chdir",
    "clone",
    "close",
    "dup",
    "dup2",
    "execve",
    "exit",
    "fork",
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

fn run_test(name: &str) {
    let path = format!("/bin/{}\0", name);
    let pid = fork();
    if pid == 0 {
        exec(path.as_str(), &[core::ptr::null::<u8>()]);
    }
    let mut exit_code: i32 = 0;
    waitpid(pid as usize, &mut exit_code);
}

#[no_mangle]
pub fn main() -> i32 {
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

    0
}
