#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::syscall::{sys_mkdir, sys_mount};

#[no_mangle]
pub fn main() -> i32 {
    println!("========== START mount ==========");
    sys_mkdir("/mnt\0".as_ptr(), 0);
    let ret = sys_mount("", "/mnt", "ext4", 0, &[]);
    println!("mount returned: {}", ret);
    println!("========== END mount ==========");
    if ret >= 0 {
        0
    } else {
        1
    }
}
