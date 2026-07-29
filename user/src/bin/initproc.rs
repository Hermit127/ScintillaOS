#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::{chdir, chroot, exec, exit, fork, mount, shutdown, waitpid, write, yield_};

fn shutdown_now() -> ! {
    shutdown();
    loop {
        yield_();
    }
}

fn fail(message: &[u8]) -> ! {
    write(2, message);
    shutdown_now();
}

#[no_mangle]
fn main() -> i32 {
    write(1, b"[finalsubmission] mounting CAgent test disk\n");
    if mount("vda", "/mnt", "ext4", 0, &[]) != 0 {
        fail(b"[finalsubmission] failed to mount vda at /mnt\n");
    }

    if chroot("/mnt\0") != 0 {
        fail(b"[finalsubmission] failed to chroot into test disk\n");
    }
    if chdir("/glibc\0") != 0 {
        fail(b"[finalsubmission] failed to enter /glibc\n");
    }

    let pid = fork();
    if pid < 0 {
        fail(b"[finalsubmission] failed to fork CAgent runner\n");
    }
    if pid == 0 {
        let argv = [
            b"bash\0".as_ptr(),
            b"-c\0".as_ptr(),
            b"./cagent_testcode.sh\0".as_ptr(),
            core::ptr::null::<u8>(),
        ];
        exec("/bin/bash\0", &argv);
        write(2, b"[finalsubmission] failed to execute CAgent script\n");
        exit(127);
    }

    let mut exit_code = 0;
    if waitpid(pid as usize, &mut exit_code) != pid {
        fail(b"[finalsubmission] failed to wait for CAgent script\n");
    }
    if exit_code == 0 {
        write(1, b"[finalsubmission] CAgent script completed\n");
    } else {
        write(2, b"[finalsubmission] CAgent script returned failure\n");
    }
    shutdown_now();
}
