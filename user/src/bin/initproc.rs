#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::{chdir, exec, exit, fork, shutdown, sys_mkdir, sys_mount, waitpid};

const NULL: *const u8 = core::ptr::null::<u8>();
const SH: *const u8 = b"sh\0".as_ptr();

const TEST_SCRIPTS: &[&[u8]] = &[
    b"./basic_testcode.sh\0",
    b"./busybox_testcode.sh\0",
    b"./iozone_testcode.sh\0",
    b"./libctest_testcode.sh\0",
    b"./libcbench_testcode.sh\0",
    b"./lmbench_testcode.sh\0",
    b"./lua_testcode.sh\0",
    b"./unixbench_testcode.sh\0",
    b"./netperf_testcode.sh\0",
    b"./iperf_testcode.sh\0",
    b"./cyclictest_testcode.sh\0",
];

fn run_exec(dir: &str, path: &str, args: &[*const u8]) {
    let pid = fork();
    if pid == 0 {
        if !dir.is_empty() {
            chdir(dir);
        }
        exec(path, args);
        exit(127);
    } else if pid > 0 {
        let mut exit_code = 0;
        waitpid(pid as usize, &mut exit_code);
    }
}

fn run_test_dir(dir: &str) -> bool {
    if chdir(dir) != 0 {
        return false;
    }
    chdir("/\0");

    for script in TEST_SCRIPTS {
        run_exec(dir, "./busybox\0", &[SH, script.as_ptr(), NULL]);
    }
    true
}

fn run_root_fallback() {
    run_exec(
        "/\0",
        "/bin/contest_runner_musl\0",
        &[b"contest_runner_musl\0".as_ptr(), NULL],
    );
    run_exec(
        "/\0",
        "/busybox\0",
        &[SH, b"/busybox_testcode.sh\0".as_ptr(), NULL],
    );
    run_exec(
        "/\0",
        "/bin/contest_runner_glibc\0",
        &[b"contest_runner_glibc\0".as_ptr(), NULL],
    );
    run_exec(
        "/\0",
        "/busybox-glibc\0",
        &[SH, b"/busybox_glibc_testcode.sh\0".as_ptr(), NULL],
    );
}

#[no_mangle]
fn main() -> i32 {
    sys_mkdir(b"/mnt\0".as_ptr(), 0o777);
    sys_mount("", "/mnt", "ext4", 0, &[]);

    let mut ran_tests = false;
    ran_tests |= run_test_dir("/mnt/musl\0");
    ran_tests |= run_test_dir("/mnt/glibc\0");

    if !ran_tests {
        run_root_fallback();
    }

    shutdown();
    0
}
