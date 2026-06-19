#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::{exec, fork, wait, write, yield_};

#[no_mangle]
fn main() -> i32 {
    if fork() == 0 {
        let argv = [b"user_shell\0".as_ptr(), core::ptr::null::<u8>()];
        let ret = exec("user_shell\0", &argv);
        write(2, b"initproc: exec user_shell failed\n");
        ret as i32
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
}
