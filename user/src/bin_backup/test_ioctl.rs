#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::open;
use user_lib::OpenFlags;

const SYSCALL_IOCTL: usize = 29;
const TCGETS: usize = 0x5401;
const TIOCGWINSZ: usize = 0x5413;

#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 19],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

fn ioctl(fd: usize, request: usize, arg: *mut u8) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") fd => ret,
            in("x11") request,
            in("x12") arg as usize,
            in("x17") SYSCALL_IOCTL
        );
    }
    ret
}

#[no_mangle]
pub fn main() -> i32 {
    let mut failed = false;

    // Test TCGETS on stdin (fd=0)
    let mut term = Termios { c_iflag: 0, c_oflag: 0, c_cflag: 0, c_lflag: 0, c_line: 0, c_cc: [0; 19] };
    let ret = ioctl(0, TCGETS, &mut term as *mut Termios as *mut u8);
    println!("ioctl(stdin, TCGETS) => {}", ret);
    if ret != 0 { failed = true; println!("  FAIL: expected 0"); }
    println!("  iflag={:#x} oflag={:#x} cflag={:#x} lflag={:#x}",
        term.c_iflag, term.c_oflag, term.c_cflag, term.c_lflag);
    if term.c_lflag == 0 { failed = true; println!("  FAIL: lflag should be non-zero"); }

    // Test TIOCGWINSZ on stdout (fd=1)
    let mut ws = Winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    let ret = ioctl(1, TIOCGWINSZ, &mut ws as *mut Winsize as *mut u8);
    println!("ioctl(stdout, TIOCGWINSZ) => {}", ret);
    if ret != 0 { failed = true; println!("  FAIL: expected 0"); }
    println!("  rows={} cols={}", ws.ws_row, ws.ws_col);
    if ws.ws_row != 24 || ws.ws_col != 80 { failed = true; println!("  FAIL: expected 24x80"); }

    // Test non-terminal fd returns ENOTTY
    let fd = open("/text.txt\0", OpenFlags::RDONLY) as usize;
    let ret = ioctl(fd, TCGETS, &mut term as *mut Termios as *mut u8);
    println!("ioctl(file, TCGETS) => {}", ret);
    if ret != -25 { failed = true; println!("  FAIL: expected -25 ENOTTY"); }

    // Test null pointer
    let ret = ioctl(0, TCGETS, core::ptr::null_mut());
    println!("ioctl(stdin, TCGETS, NULL) => {}", ret);
    if ret != -14 { failed = true; println!("  FAIL: expected -14 EFAULT"); }

    if failed {
        println!("\n!!! SOME TESTS FAILED !!!");
        -1
    } else {
        println!("\n=== IOCTL ALL PASSED ===");
        0
    }
}
