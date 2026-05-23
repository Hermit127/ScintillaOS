#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{close, open, OpenFlags};

const SYSCALL_FSTATAT: usize = 79;
const AT_FDCWD: isize = -100;
const AT_EMPTY_PATH: usize = 0x1000;

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

fn fstatat(dirfd: isize, path: *const u8, buf: *mut Kstat, flags: usize) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") dirfd as usize => ret,
            in("x11") path as usize,
            in("x12") buf as usize,
            in("x13") flags,
            in("x17") SYSCALL_FSTATAT
        );
    }
    ret
}

#[no_mangle]
pub fn main() -> i32 {
    let mut st = Kstat::default();
    let mut failed = false;

    // Test 1: stat an existing file by path
    let path = b"/text.txt\0";
    let ret = fstatat(AT_FDCWD, path.as_ptr(), &mut st, 0);
    println!("fstatat(/text.txt): ret={} size={} mode={:o}", ret, st.st_size, st.st_mode);
    if ret != 0 { failed = true; println!("  FAIL: expected 0"); }
    if st.st_size != 55 { failed = true; println!("  FAIL: expected size 55"); }

    // Test 2: stat shows uid/gid fields exist (host-dependent values)
    println!("  st_uid={} st_gid={}", st.st_uid, st.st_gid);

    // Test 3: AT_EMPTY_PATH stat an open fd
    let fd = open("/text.txt\0", OpenFlags::RDONLY) as usize;
    let ret = fstatat(fd as isize, core::ptr::null(), &mut st, AT_EMPTY_PATH);
    println!("fstatat(fd, \"\", AT_EMPTY_PATH): ret={} size={}", ret, st.st_size);
    if ret != 0 { failed = true; println!("  FAIL: expected 0"); }
    if st.st_size != 55 { failed = true; println!("  FAIL: expected size 55"); }
    close(fd);

    // Test 4: nonexistent path
    let path = b"/nonexistent\0";
    let ret = fstatat(AT_FDCWD, path.as_ptr(), &mut st, 0);
    println!("fstatat(/nonexistent): ret={}", ret);
    if ret != -2 { failed = true; println!("  FAIL: expected -2 ENOENT"); }

    // Test 5: null stat_buf
    let path = b"/text.txt\0";
    let ret = fstatat(AT_FDCWD, path.as_ptr(), core::ptr::null_mut(), 0);
    println!("fstatat(null buf): ret={}", ret);
    if ret != -22 { failed = true; println!("  FAIL: expected -22 EINVAL"); }

    if failed {
        println!("\n!!! SOME TESTS FAILED !!!");
        -1
    } else {
        println!("\n=== FSTATAT ALL PASSED ===");
        0
    }
}
#[cfg(target_arch = "riscv64")]
fn main() {}
