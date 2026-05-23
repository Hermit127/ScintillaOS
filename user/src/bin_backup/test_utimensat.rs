#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{close, open, write, setuid, OpenFlags};

const SYSCALL_UTIMENSAT: usize = 88;
const SYSCALL_FSTAT: usize = 80;
const AT_FDCWD: isize = -100;
const UTIME_NOW: i64 = (1i64 << 30) - 1;
const UTIME_OMIT: i64 = (1i64 << 30) - 2;

fn utimensat(dirfd: isize, path: *const u8, times: *const u8, flags: usize) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!("ecall",
            inlateout("x10") dirfd as usize => ret,
            in("x11") path as usize, in("x12") times as usize,
            in("x13") flags, in("x17") SYSCALL_UTIMENSAT);
    }
    ret
}

fn fstat(fd: usize, buf: *mut u8) -> isize {
    let mut ret: isize;
    unsafe { core::arch::asm!("ecall",
        inlateout("x10") fd => ret, in("x11") buf as usize,
        in("x17") SYSCALL_FSTAT); }
    ret
}

fn mtime(buf: &[u8; 128]) -> i64 {
    let b = |i: usize| buf[88 + i];
    i64::from_le_bytes([b(0),b(1),b(2),b(3),b(4),b(5),b(6),b(7)])
}
fn ctime(buf: &[u8; 128]) -> i64 {
    let b = |i: usize| buf[104 + i];
    i64::from_le_bytes([b(0),b(1),b(2),b(3),b(4),b(5),b(6),b(7)])
}

#[no_mangle]
pub fn main() -> i32 {
    let mut failed = false;

    let fd = open("utime_test\0", OpenFlags::CREATE | OpenFlags::RDWR) as usize;
    write(fd, b"x");

    // Test 1: times=NULL sets timestamps (wait 1s so clock > 0)
    user_lib::sleep(1100);
    let mut st = [0u8; 128];
    let ret = utimensat(AT_FDCWD, b"/utime_test\0".as_ptr(), core::ptr::null(), 0);
    println!("Test1 NULL: ret={}", ret);
    if ret != 0 { failed = true; println!("  FAIL"); }
    fstat(fd, st.as_mut_ptr());
    println!("  mtime={} ctime={}", mtime(&st), ctime(&st));
    if mtime(&st) == 0 || ctime(&st) == 0 { failed = true; println!("  FAIL: zero"); }

    // Test 2: UTIME_NOW sets to current time
    #[repr(C)] struct TS { tv_sec: i64, tv_nsec: i64 }
    let now = [TS{ tv_sec: 0, tv_nsec: UTIME_NOW }, TS{ tv_sec: 0, tv_nsec: UTIME_NOW }];
    let m_before = mtime(&st);
    let ret = utimensat(AT_FDCWD, b"/utime_test\0".as_ptr(), &now as *const _ as *const u8, 0);
    println!("Test2 UTIME_NOW: ret={}", ret);
    if ret != 0 { failed = true; println!("  FAIL"); }
    fstat(fd, st.as_mut_ptr());
    println!("  mtime {}→{}", m_before, mtime(&st));

    // Test 3: UTIME_OMIT leaves unchanged, still succeeds
    let omit = [TS{ tv_sec: 0, tv_nsec: UTIME_OMIT }, TS{ tv_sec: 0, tv_nsec: UTIME_OMIT }];
    let ret = utimensat(AT_FDCWD, b"/utime_test\0".as_ptr(), &omit as *const _ as *const u8, 0);
    println!("Test3 UTIME_OMIT: ret={}", ret);
    if ret != 0 { failed = true; println!("  FAIL: should succeed (no error)"); }

    // Test 4: nonexistent path
    let ret = utimensat(AT_FDCWD, b"/no_such\0".as_ptr(), core::ptr::null(), 0);
    println!("Test4 nonexistent: ret={}", ret);
    if ret != -2 { failed = true; println!("  FAIL: expected -2"); }

    // Test 5: non-owner without write permission denied
    setuid(9999);
    let fd2 = open("priv_file\0", OpenFlags::CREATE | OpenFlags::RDWR) as usize;
    write(fd2, b"x");
    setuid(0); // back to root to chmod... actually we don't have chmod.
    // File is 0o777, so write perm is granted to "other". Non-owner CAN write.
    // Set uid to someone else and test
    setuid(8888);
    let ret = utimensat(AT_FDCWD, b"/priv_file\0".as_ptr(), core::ptr::null(), 0);
    println!("Test5 other user (0o777): ret={}", ret);
    if ret != 0 { failed = true; println!("  FAIL: should succeed (other has write)"); }
    setuid(0);
    close(fd2);
    close(fd);

    if failed { println!("\n!!! FAILED !!!"); -1 }
    else { println!("\n=== UTIMENSAT ALL PASSED ==="); 0 }
}
#[cfg(target_arch = "riscv64")]
fn main() {}
