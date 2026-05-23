#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

const SYSCALL_FACCESSAT: usize = 48;
const AT_FDCWD: isize = -100;

const F_OK: usize = 0;
const R_OK: usize = 4;
const W_OK: usize = 2;
const X_OK: usize = 1;

fn faccessat(dirfd: isize, path: *const u8, mode: usize, flags: usize) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") dirfd as usize => ret,
            in("x11") path as usize,
            in("x12") mode,
            in("x13") flags,
            in("x17") SYSCALL_FACCESSAT
        );
    }
    ret
}

#[no_mangle]
pub fn main() -> i32 {
    let mut failed = false;
    let path = b"/text.txt\0";

    // Test F_OK: file exists
    let ret = faccessat(AT_FDCWD, path.as_ptr(), F_OK, 0);
    println!("faccessat(F_OK) => {}", ret);
    if ret != 0 { failed = true; println!("  FAIL"); }

    // Test R_OK: root can always read
    let ret = faccessat(AT_FDCWD, path.as_ptr(), R_OK, 0);
    println!("faccessat(R_OK) => {}", ret);
    if ret != 0 { failed = true; println!("  FAIL"); }

    // Test R_OK | W_OK
    let ret = faccessat(AT_FDCWD, path.as_ptr(), R_OK | W_OK, 0);
    println!("faccessat(R_OK|W_OK) => {}", ret);
    if ret != 0 { failed = true; println!("  FAIL"); }

    // Test X_OK
    let ret = faccessat(AT_FDCWD, path.as_ptr(), X_OK, 0);
    println!("faccessat(X_OK) => {}", ret);
    if ret != 0 { failed = true; println!("  FAIL"); }

    // Test nonexistent path
    let bad = b"/nonexistent\0";
    let ret = faccessat(AT_FDCWD, bad.as_ptr(), F_OK, 0);
    println!("faccessat(nonexistent) => {}", ret);
    if ret != -2 { failed = true; println!("  FAIL: expected -2"); }

    if failed {
        println!("\n!!! SOME TESTS FAILED !!!");
        -1
    } else {
        println!("\n=== FACCESSAT ALL PASSED ===");
        0
    }
}
#[cfg(target_arch = "riscv64")]
fn main() {}
