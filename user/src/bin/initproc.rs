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

fn run_script(script: &'static [u8], name: &[u8]) -> bool {
    write(1, b"[finalsubmission] starting ");
    write(1, name);
    write(1, b"\n");

    let pid = fork();
    if pid < 0 {
        fail(b"[finalsubmission] failed to fork test runner\n");
    }
    if pid == 0 {
        let argv = [
            b"bash\0".as_ptr(),
            b"-c\0".as_ptr(),
            script.as_ptr(),
            core::ptr::null::<u8>(),
        ];
        exec("/bin/bash\0", &argv);
        write(2, b"[finalsubmission] failed to execute test script\n");
        exit(127);
    }

    let mut exit_code = 0;
    if waitpid(pid as usize, &mut exit_code) != pid {
        fail(b"[finalsubmission] failed to wait for test script\n");
    }
    let output_fd = if exit_code == 0 {
        write(1, b"[finalsubmission] completed ");
        1
    } else {
        write(2, b"[finalsubmission] script returned failure: ");
        2
    };
    write(output_fd, name);
    write(output_fd, b"\n");
    exit_code == 0
}

#[no_mangle]
fn main() -> i32 {
    write(1, b"[finalsubmission] mounting final test disk\n");
    if mount("vda", "/mnt", "ext4", 0, &[]) != 0 {
        fail(b"[finalsubmission] failed to mount vda at /mnt\n");
    }

    if chroot("/mnt\0") != 0 {
        fail(b"[finalsubmission] failed to chroot into test disk\n");
    }
    if chdir("/glibc\0") != 0 {
        fail(b"[finalsubmission] failed to enter /glibc\n");
    }

    run_script(b"./cagent_testcode.sh\0", b"CAgent");
    run_script(
        b"./buildstorm_testcode.sh\0",
        b"BuildStorm 2.1 + 2.2",
    );
    shutdown_now();
}
