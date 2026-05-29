#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::syscall::sys_umount2;

#[no_mangle]
pub fn main() -> i32 {
    println!("========== START umount ==========");
    let ret = sys_umount2("/", 0);
    println!("umount returned: {}", ret);
    println!("========== END umount ==========");
    if ret >= 0 {
        0
    } else {
        1
    }
}
