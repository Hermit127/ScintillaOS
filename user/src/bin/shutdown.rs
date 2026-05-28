#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::shutdown;

#[no_mangle]
fn main() -> i32 {
    shutdown();
    0
}
