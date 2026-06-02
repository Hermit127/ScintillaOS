#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{exec, exit, fork, sleep, waitpid};

const IPERF3: &str = "/bin/iperf3\0";
const NULL: *const u8 = core::ptr::null::<u8>();

fn run_client(name: &str, args: &[*const u8]) {
    println!("====== iperf {} begin ======", name);
    let pid = fork();
    if pid == 0 {
        exec(IPERF3, args);
        exit(127);
    }
    let mut exit_code: i32 = 0;
    waitpid(pid as usize, &mut exit_code);
    if exit_code == 0 {
        println!("====== iperf {} end success ======", name);
    } else {
        println!("====== iperf {} end fail-{} ======", name, exit_code);
    }
}

#[no_mangle]
pub fn main() -> i32 {
    println!("#### OS COMP TEST GROUP START iperf-musl ####");

    let server_args: [*const u8; 5] = [
        b"iperf3\0".as_ptr(),
        b"-s\0".as_ptr(),
        b"-p\0".as_ptr(),
        b"5001\0".as_ptr(),
        NULL,
    ];
    let pid = fork();
    if pid == 0 {
        exec(IPERF3, &server_args);
        exit(127);
    }
    println!("iperf server started (pid={})", pid);
    sleep(500);

    // TCP only: -n 1K
    let args: [*const u8; 10] = [
        b"iperf3\0".as_ptr(),
        b"-c\0".as_ptr(),
        b"127.0.0.1\0".as_ptr(),
        b"-p\0".as_ptr(),
        b"5001\0".as_ptr(),
        b"-n\0".as_ptr(),
        b"100\0".as_ptr(),
        b"-i\0".as_ptr(),
        b"0\0".as_ptr(),
        NULL,
    ];
    run_client("BASIC_TCP", &args);

    let args: [*const u8; 13] = [
        b"iperf3\0".as_ptr(),
        b"-c\0".as_ptr(),
        b"127.0.0.1\0".as_ptr(),
        b"-p\0".as_ptr(),
        b"5001\0".as_ptr(),
        b"-t\0".as_ptr(),
        b"1\0".as_ptr(),
        b"-l\0".as_ptr(),
        b"1K\0".as_ptr(),
        b"-u\0".as_ptr(),
        b"-b\0".as_ptr(),
        b"1000G\0".as_ptr(),
        NULL,
    ];
    run_client("BASIC_UDP", &args);

    println!("#### OS COMP TEST GROUP END iperf-musl ####");
    0
}
