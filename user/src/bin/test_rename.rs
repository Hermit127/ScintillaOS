#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{close, open, read, write, OpenFlags};

const SYSCALL_RENAMEAT2: usize = 276;
const AT_FDCWD: isize = -100;

fn renameat2(
    olddirfd: isize,
    oldpath: *const u8,
    newdirfd: isize,
    newpath: *const u8,
    flags: usize,
) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") olddirfd as usize => ret,
            in("x11") oldpath as usize,
            in("x12") newdirfd as usize,
            in("x13") newpath as usize,
            in("x14") flags,
            in("x17") SYSCALL_RENAMEAT2
        );
    }
    ret
}

#[no_mangle]
pub fn main() -> i32 {
    let mut failed = false;

    let src = b"/rename_src\0";
    let dst = b"/rename_dst\0";
    let test_data = b"hello rename";

    // Create source file
    let fd = open(core::str::from_utf8(src).unwrap().trim_end_matches('\0'),
        OpenFlags::CREATE | OpenFlags::RDWR) as usize;
    write(fd, test_data);
    close(fd);

    // Rename
    let ret = renameat2(AT_FDCWD, src.as_ptr(), AT_FDCWD, dst.as_ptr(), 0);
    println!("rename(src -> dst) => {}", ret);
    if ret != 0 { failed = true; println!("  FAIL"); return -1; }

    // Verify old path is gone
    let fd = open(core::str::from_utf8(src).unwrap().trim_end_matches('\0'), OpenFlags::RDONLY);
    println!("open(src) after rename => {}", fd);
    if fd >= 0 { failed = true; println!("  FAIL: old path still exists"); }

    // Verify new path has content
    let fd = open(core::str::from_utf8(dst).unwrap().trim_end_matches('\0'), OpenFlags::RDONLY) as usize;
    println!("open(dst) => {}", fd);
    if fd < 3 { failed = true; println!("  FAIL"); return -1; }

    let mut buf = [0u8; 32];
    let n = read(fd, &mut buf) as usize;
    let s = core::str::from_utf8(&buf[..n]).unwrap();
    println!("read => \"{}\"", s);
    if s != "hello rename" { failed = true; println!("  FAIL"); }
    close(fd);

    if failed {
        println!("\n!!! SOME TESTS FAILED !!!");
        -1
    } else {
        println!("\n=== RENAME TESTS PASSED ===");
        0
    }
}
