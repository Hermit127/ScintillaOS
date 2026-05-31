#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::{exec, fork, wait, yield_};

#[no_mangle]
fn main() -> i32 {
    if fork() == 0 {
        exec("busybox\0", &[
            b"busybox\0".as_ptr(),
            b"sh\0".as_ptr(),
            core::ptr::null::<u8>(),
        ]);
    } else {
        loop {
            let mut exit_code: i32 = 0;
            let pid = wait(&mut exit_code);
            if pid == -1 {
                yield_();
                continue;
            }
        }
    }
    0
}
