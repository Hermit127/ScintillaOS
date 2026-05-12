#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

const SYSCALL_SYMLINKAT: usize = 36;
const SYSCALL_READLINKAT: usize = 78;
const AT_FDCWD: isize = -100;

fn symlinkat(target: *const u8, newdirfd: isize, linkpath: *const u8) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") target as usize => ret,
            in("x11") newdirfd as usize,
            in("x12") linkpath as usize,
            in("x17") SYSCALL_SYMLINKAT
        );
    }
    ret
}

fn readlinkat(dirfd: isize, path: *const u8, buf: *mut u8, bufsiz: usize) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") dirfd as usize => ret,
            in("x11") path as usize,
            in("x12") buf as usize,
            in("x13") bufsiz,
            in("x17") SYSCALL_READLINKAT
        );
    }
    ret
}

#[no_mangle]
pub fn main() -> i32 {
    let mut failed = false;

    let target = b"/text.txt\0";
    let link = b"/test_link\0";

    let ret = symlinkat(target.as_ptr(), AT_FDCWD, link.as_ptr());
    println!("symlinkat(/text.txt -> /test_link) => {}", ret);
    if ret != 0 { failed = true; println!("  FAIL: expected 0"); }

    let mut buf = [0u8; 64];
    let ret = readlinkat(AT_FDCWD, link.as_ptr(), buf.as_mut_ptr(), buf.len());
    println!("readlinkat(/test_link) => {}", ret);
    if ret < 0 { failed = true; println!("  FAIL: expected >= 0, got {}", ret); }
    if ret > 0 {
        let s = core::str::from_utf8(&buf[..ret as usize]).unwrap();
        println!("  target = \"{}\"", s);
        if s != "/text.txt" { failed = true; println!("  FAIL: expected /text.txt"); }
    }

    if failed {
        println!("\n!!! SOME TESTS FAILED !!!");
        -1
    } else {
        println!("\n=== SYMLINK TESTS PASSED ===");
        0
    }
}
