#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

const SYSCALL_UMASK: usize = 166;

fn umask(mask: u32) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") mask as usize => ret,
            in("x17") SYSCALL_UMASK
        );
    }
    ret
}

#[no_mangle]
pub fn main() -> i32 {
    let mut failed = false;

    // Read initial umask (should be 0o022)
    let old = umask(0o777); // set to 777, get back old
    println!("umask(0o777) => old={:#o}", old);
    if old != 0o022 { failed = true; println!("  FAIL: expected 0o22"); }

    // Set and verify
    let old = umask(0o000);
    println!("umask(0o000) => old={:#o}", old);
    if old != 0o777 { failed = true; println!("  FAIL: expected 0o777"); }

    let old = umask(old as u32); // restore
    println!("umask(restore) => old={:#o}", old);

    if failed { println!("\n!!! FAILED !!!"); -1 }
    else { println!("\n=== UMASK PASSED ==="); 0 }
}
