#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::shutdown;

#[no_mangle]
fn main() -> i32 {
    println!("[initproc] hello world");
    shutdown();
    0
}
