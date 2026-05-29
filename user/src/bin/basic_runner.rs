#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use alloc::format;
use alloc::string::String;
use user_lib::{close, dup, exec, exit, fork, pipe, read, waitpid};

const TESTS: &[&str] = &[
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

#[no_mangle]
pub fn main() -> i32 {
    println!("===== BASIC TEST RUNNER =====");
    println!("Running {} tests...\n", TESTS.len());

    let mut passed: usize = 0;
    let mut failed: usize = 0;

    for test_name in TESTS {
        let path = format!("/bin/{}\0", test_name);

        let mut pipe_fd = [0u32; 2];
        pipe(&mut pipe_fd);

        let pid = fork();
        if pid == 0 {
            close(pipe_fd[0] as usize);
            close(1);
            let _ = dup(pipe_fd[1] as usize);
            close(pipe_fd[1] as usize);

            let args: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
            let _ = exec(path.as_str(), &args);
            exit(1);
        }

        close(pipe_fd[1] as usize);

        let mut output = String::new();
        let mut buf = [0u8; 256];
        loop {
            let n = read(pipe_fd[0] as usize, &mut buf);
            if n <= 0 {
                break;
            }
            let n = n as usize;
            for &b in &buf[..n] {
                output.push(b as char);
            }
        }
        close(pipe_fd[0] as usize);

        let mut exit_code: i32 = 0;
        waitpid(pid as usize, &mut exit_code);

        let has_start = output.contains("========== START ");
        let has_end = output.contains("========== END ");

        if has_start && has_end {
            passed += 1;
            println!("[PASS] {:>12} (exit: {})", test_name, exit_code);
        } else {
            failed += 1;
            println!("[FAIL] {:>12} (exit: {})", test_name, exit_code);
        }
    }

    println!("\n===== RESULT =====");
    println!("Passed: {}/{}", passed, TESTS.len());
    println!("Failed: {}/{}", failed, TESTS.len());
    if failed == 0 {
        println!("ALL TESTS PASSED!");
    }

    0
}
