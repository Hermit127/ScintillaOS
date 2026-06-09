#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::{exec, exit, fork, open, shutdown, sys_mkdir, sys_mount, waitpid, OpenFlags};

const NULL: *const u8 = core::ptr::null::<u8>();
const SH: *const u8 = b"sh\0".as_ptr();

fn run_exec(dir: &str, path: &str, args: &[*const u8]) {
    let pid = fork();
    if pid == 0 {
        if !dir.is_empty() {
            user_lib::chdir(dir);
        }
        exec(path, args);
        exit(127);
    } else if pid > 0 {
        let mut exit_code = 0;
        waitpid(pid as usize, &mut exit_code);
    }
}

#[no_mangle]
fn main() -> i32 {
    // IOZONE only mode
    if open("/iozone_only\0", OpenFlags::RDONLY) >= 0 {
        run_exec("/\0", "/busybox\0", &[SH, b"/iozone_testcode.sh\0".as_ptr(), NULL]);
        run_exec("/\0", "/busybox-glibc\0", &[SH, b"/iozone_glibc_testcode.sh\0".as_ptr(), NULL]);
        shutdown();
    }

    sys_mkdir(b"/mnt\0".as_ptr(), 0o777);
    sys_mount("", "/mnt", "ext4", 0, &[]);

    // Run the dynamic contest runner
    run_exec("/\0", "/busybox\0", &[SH, b"/contest.sh\0".as_ptr(), NULL]);

    shutdown();
    0
}
