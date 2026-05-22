#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::{exec, fork, wait, yield_};

#[no_mangle]
fn main() -> i32 {
    if fork() == 0 {
        #[cfg(target_arch = "riscv64")]
        exec("busybox\0", &[
            "sh\0".as_ptr(),
            "/etc/init.sh\0".as_ptr(),
            core::ptr::null::<u8>(),
        ]);
        exec("contest_runner\0", &[core::ptr::null::<u8>()]);
        exec("user_shell\0", &[core::ptr::null::<u8>()]);
    } else {
        loop {
            let mut exit_code: i32 = 0;
            let pid = wait(&mut exit_code);
            if pid == -1 {
                yield_();
                continue;
            }
            user_lib::shutdown();
        }
    }
    0
}
