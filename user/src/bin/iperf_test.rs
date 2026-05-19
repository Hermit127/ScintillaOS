#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{exec, exit, fork, waitpid, yield_};

fn nullterm(s: &str) -> String {
    let mut out = String::from(s);
    out.push('\0');
    out
}

fn run_client(name: &str, extra: &[&str]) -> bool {
    let pid = fork();
    if pid == 0 {
        let mut strs: Vec<String> = Vec::new();
        strs.push(nullterm("/bin/iperf3"));
        strs.push(nullterm("-c"));
        strs.push(nullterm("10.0.2.15"));
        strs.push(nullterm("-p"));
        strs.push(nullterm("5001"));
        strs.push(nullterm("-t"));
        strs.push(nullterm("2"));
        strs.push(nullterm("-i"));
        strs.push(nullterm("0"));
        for a in extra {
            strs.push(nullterm(a));
        }
        let ptrs: Vec<*const u8> = strs.iter().map(|s| s.as_ptr())
            .chain(core::iter::once(core::ptr::null()))
            .collect();
        exec("/bin/iperf3\0", &ptrs);
        exit(1);
    }
    let mut code: i32 = 0;
    waitpid(pid as usize, &mut code);
    code == 0
}

#[no_mangle]
pub fn main() -> i32 {
    println!("#### OS COMP TEST GROUP START iperf ####");

    // Start server in background
    let server_pid = fork();
    if server_pid == 0 {
        let args: [*const u8; 5] = [
            "/bin/iperf3\0".as_ptr(),
            "-s\0".as_ptr(),
            "-p\0".as_ptr(),
            "5001\0".as_ptr(),
            core::ptr::null(),
        ];
        exec("/bin/iperf3\0", &args);
        exit(1);
    }

    // Give server time to start
    for _ in 0..500 { yield_(); }
    println!("Server running (pid={})", server_pid);

    let tests: &[(&str, &[&str])] = &[
        ("BASIC_TCP",     &[]),
        ("BASIC_UDP",     &["-u", "-b", "100G"]),
        ("PARALLEL_TCP",  &["-P", "5"]),
        ("PARALLEL_UDP",  &["-u", "-P", "5", "-b", "100G"]),
        ("REVERSE_TCP",   &["-R"]),
        ("REVERSE_UDP",   &["-u", "-R", "-b", "100G"]),
    ];

    let mut passed: usize = 0;
    for (i, (name, extra)) in tests.iter().enumerate() {
        print!("[{}] {} ... ", i + 1, name);
        if run_client(name, extra) {
            println!("PASS");
            passed += 1;
        } else {
            println!("FAIL");
        }
        for _ in 0..100 { yield_(); }
    }

    println!("#### RESULT: {}/{} passed ####", passed, tests.len());
    println!("#### OS COMP TEST GROUP END iperf ####");
    0
}
