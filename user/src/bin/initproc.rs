#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::{exec, exit, fork, shutdown, waitpid};

const NULL: *const u8 = core::ptr::null::<u8>();

fn run_cmd(cmd: &str) {
    let pid = fork();
    if pid == 0 {
        exec(cmd, &[NULL]);
        exit(127);
    } else if pid > 0 {
        let mut exit_code = 0;
        waitpid(pid as usize, &mut exit_code);
    }
}

fn run_busybox(busybox: &str, script: &str) {
    let pid = fork();
    if pid == 0 {
        exec(busybox, &[b"sh\0".as_ptr(), script.as_ptr(), NULL]);
        exit(127);
    } else if pid > 0 {
        let mut exit_code = 0;
        waitpid(pid as usize, &mut exit_code);
    }
}

#[no_mangle]
fn main() -> i32 {
    // 1. basic musl
    run_cmd("/basic_runner\0");
    // 2. musl busybox
    run_busybox("/busybox\0", "/busybox_testcode.sh\0");
    // 3. glibc busybox
    run_busybox("/busybox-glibc\0", "/busybox_glibc_testcode.sh\0");
    // 4. shutdown
    shutdown();
    0
}
