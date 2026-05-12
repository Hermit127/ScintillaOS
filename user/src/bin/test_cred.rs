#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{close, open, OpenFlags, geteuid, getegid, setuid, setgid};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Kstat {
    st_dev: u64,
    st_ino: u64,
    st_mode: u32,
    st_nlink: u32,
    st_uid: u32,
    st_gid: u32,
    st_rdev: u64,
    __pad: u64,
    st_size: i64,
    st_blksize: u32,
    __pad2: i32,
    st_blocks: u64,
    st_atime_sec: i64,
    st_atime_nsec: i64,
    st_mtime_sec: i64,
    st_mtime_nsec: i64,
    st_ctime_sec: i64,
    st_ctime_nsec: i64,
    __unused: [u32; 2],
}

const SYSCALL_FSTAT: usize = 80;

fn fstat(fd: usize, st: &mut Kstat) -> isize {
    syscall6(SYSCALL_FSTAT, fd, st as *mut Kstat as usize, 0, 0, 0, 0)
}

fn syscall6(id: usize, a0: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") a0 => ret,
            in("x11") a1,
            in("x12") a2,
            in("x13") a3,
            in("x14") a4,
            in("x15") a5,
            in("x17") id
        );
    }
    ret
}

const FILE1: &str = "cred_test_root\0";
const FILE2: &str = "cred_test_u1000\0";

#[no_mangle]
pub fn main() -> i32 {
    let mut st = Kstat::default();

    // Test 1: root creates a file, fstat should show uid=0 gid=0
    println!("=== Test 1: root creates file ===");
    println!("euid: {}  egid: {}", geteuid(), getegid());

    let fd = open(FILE1, OpenFlags::CREATE | OpenFlags::RDWR);
    println!("open/create '{}' -> fd={}", FILE1, fd);
    if fd < 0 {
        println!("FAIL: could not create file");
        return -1;
    }

    let ret = fstat(fd as usize, &mut st);
    println!("fstat ret: {}", ret);
    println!("  st_uid: {}  st_gid: {}", st.st_uid, st.st_gid);
    if st.st_uid != 0 || st.st_gid != 0 {
        println!("FAIL: expected uid=0 gid=0, got uid={} gid={}", st.st_uid, st.st_gid);
        return -1;
    }
    println!("PASS: root file has uid=0 gid=0");
    close(fd as usize);

    // Test 2: setuid(1000) then create a file, fstat should show uid=1000
    println!("\n=== Test 2: user 1000 creates file ===");
    setuid(1000);
    setgid(500);
    println!("euid: {}  egid: {}", geteuid(), getegid());

    let fd = open(FILE2, OpenFlags::CREATE | OpenFlags::RDWR);
    println!("open/create '{}' -> fd={}", FILE2, fd);
    if fd < 0 {
        println!("FAIL: could not create file");
        return -1;
    }

    let ret = fstat(fd as usize, &mut st);
    println!("fstat ret: {}", ret);
    println!("  st_uid: {}  st_gid: {}", st.st_uid, st.st_gid);
    if st.st_uid != 1000 || st.st_gid != 500 {
        println!("FAIL: expected uid=1000 gid=500, got uid={} gid={}", st.st_uid, st.st_gid);
        return -1;
    }
    println!("PASS: user file has uid=1000 gid=500");
    close(fd as usize);

    // Test 3: reopen the root-created file as user 1000, fstat should still show uid=0
    println!("\n=== Test 3: read back root's file as user 1000 ===");
    let fd = open(FILE1, OpenFlags::RDONLY);
    println!("open '{}' -> fd={}", FILE1, fd);
    if fd < 0 {
        println!("FAIL: could not open root's file");
        return -1;
    }
    let ret = fstat(fd as usize, &mut st);
    println!("fstat ret: {}", ret);
    println!("  st_uid: {}  st_gid: {}", st.st_uid, st.st_gid);
    if st.st_uid != 0 || st.st_gid != 0 {
        println!("FAIL: root's file uid changed! got uid={} gid={}", st.st_uid, st.st_gid);
        return -1;
    }
    println!("PASS: root's file still uid=0 gid=0");
    close(fd as usize);

    println!("\n=== ALL TESTS PASSED ===");
    0
}
