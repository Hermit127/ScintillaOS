#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{close, open, read, write, OpenFlags};

const SYSCALL_LSEEK: usize = 62;

fn lseek(fd: usize, offset: i64, whence: i32) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") fd => ret,
            in("x11") offset as usize,
            in("x12") whence as usize,
            in("x17") SYSCALL_LSEEK
        );
    }
    ret
}

#[no_mangle]
pub fn main() -> i32 {
    let mut failed = false;

    // Test write + lseek
    let fd = open("lseek_file\0", OpenFlags::CREATE | OpenFlags::RDWR) as usize;
    println!("open/create => {}", fd);
    if fd < 3 { println!("FAIL open"); return -1; }

    let n = write(fd, b"ABCDEFGHIJKLMNOPQRSTUVWXYZ");
    println!("write 26 bytes => {}", n);

    // SEEK_SET
    let pos = lseek(fd, 5, 0);
    println!("lseek(SET,5) => {}", pos);
    let mut buf = [0u8; 5];
    read(fd, &mut buf);
    let s = core::str::from_utf8(&buf).unwrap();
    println!("  read: \"{}\"", s);
    if s != "FGHIJ" { failed = true; println!("  FAIL"); }

    // SEEK_CUR
    let pos = lseek(fd, 3, 1);
    println!("lseek(CUR,3) => {}", pos);
    read(fd, &mut buf);
    let s = core::str::from_utf8(&buf).unwrap();
    println!("  read: \"{}\"", s);
    if s != "NOPQR" { failed = true; println!("  FAIL"); }

    // SEEK_END
    let pos = lseek(fd, -5, 2);
    println!("lseek(END,-5) => {}", pos);
    read(fd, &mut buf);
    let s = core::str::from_utf8(&buf).unwrap();
    println!("  read: \"{}\"", s);
    if s != "VWXYZ" { failed = true; println!("  FAIL"); }

    // Bad whence
    let pos = lseek(fd, 0, 99);
    println!("lseek(bad,0) => {}", pos);
    if pos != -22 { failed = true; println!("  FAIL: expected -22"); }

    close(fd);

    if failed {
        println!("\n!!! SOME TESTS FAILED !!!");
        -1
    } else {
        println!("\n=== LSEEK ALL PASSED ===");
        0
    }
}
#[cfg(target_arch = "riscv64")]
fn main() {}
