#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{close, open, OpenFlags};

const SYSCALL_FCNTL: usize = 25;

const F_DUPFD: usize = 0;
const F_GETFD: usize = 1;
const F_SETFD: usize = 2;
const F_GETFL: usize = 3;
const F_DUPFD_CLOEXEC: usize = 1030;
const FD_CLOEXEC: usize = 1;

fn fcntl(fd: usize, cmd: usize, arg: usize) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") fd => ret,
            in("x11") cmd,
            in("x12") arg,
            in("x17") SYSCALL_FCNTL
        );
    }
    ret
}

#[no_mangle]
pub fn main() -> i32 {
    let mut failed = false;

    let fd = open("/text.txt\0", OpenFlags::RDONLY) as usize;
    println!("open => fd={}", fd);

    // Test F_GETFD: initially should be 0
    let ret = fcntl(fd, F_GETFD, 0);
    println!("F_GETFD => {}", ret);
    if ret != 0 { failed = true; println!("  FAIL: expected 0"); }

    // Test F_SETFD: set FD_CLOEXEC
    let ret = fcntl(fd, F_SETFD, FD_CLOEXEC);
    println!("F_SETFD(FD_CLOEXEC) => {}", ret);
    if ret != 0 { failed = true; println!("  FAIL: expected 0"); }

    // Test F_GETFD: should be 1 now
    let ret = fcntl(fd, F_GETFD, 0);
    println!("F_GETFD after set => {}", ret);
    if ret != FD_CLOEXEC as isize { failed = true; println!("  FAIL: expected {}", FD_CLOEXEC); }

    // Test F_SETFD: clear FD_CLOEXEC
    let ret = fcntl(fd, F_SETFD, 0);
    println!("F_SETFD(0) => {}", ret);

    // Test F_DUPFD: duplicate to a specific fd
    let new_fd = fcntl(fd, F_DUPFD, 10);
    println!("F_DUPFD(10) => {}", new_fd);
    if new_fd != 10 { failed = true; println!("  FAIL: expected 10"); }

    // Test F_DUPFD_CLOEXEC
    let new_fd = fcntl(fd, F_DUPFD_CLOEXEC, 20);
    println!("F_DUPFD_CLOEXEC(20) => {}", new_fd);
    if new_fd != 20 { failed = true; println!("  FAIL: expected 20"); }

    let ret = fcntl(new_fd as usize, F_GETFD, 0);
    println!("F_GETFD on DUPFD_CLOEXEC fd => {}", ret);
    if ret != FD_CLOEXEC as isize { failed = true; println!("  FAIL: expected {}", FD_CLOEXEC); }

    // Test F_GETFL
    let ret = fcntl(fd, F_GETFL, 0);
    println!("F_GETFL => {:#x}", ret);
    if ret & 0x8000 == 0 { failed = true; println!("  FAIL: O_LARGEFILE missing"); }

    // Test bad fd
    let ret = fcntl(999, F_GETFD, 0);
    println!("fcntl(bad fd) => {}", ret);
    if ret != -9 { failed = true; println!("  FAIL: expected -9 EBADF"); }

    // Test bad cmd
    let ret = fcntl(fd, 999, 0);
    println!("fcntl(bad cmd) => {}", ret);
    if ret != -22 { failed = true; println!("  FAIL: expected -22 EINVAL"); }

    close(fd);
    close(new_fd as usize);

    if failed {
        println!("\n!!! SOME TESTS FAILED !!!");
        -1
    } else {
        println!("\n=== FCNTL ALL PASSED ===");
        0
    }
}
