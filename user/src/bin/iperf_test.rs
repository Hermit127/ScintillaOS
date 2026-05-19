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

#[no_mangle]
pub fn main() -> i32 {
    println!("#### OS COMP TEST GROUP START iperf ####");

    // Start server in background
    let server_pid = fork();
    if server_pid == 0 {
        let args: [*const u8; 7] = [
            "/bin/iperf3\0".as_ptr(),
            "-s\0".as_ptr(),
            "-B\0".as_ptr(),
            "127.0.0.1\0".as_ptr(),
            "-p\0".as_ptr(),
            "5001\0".as_ptr(),
            core::ptr::null(),
        ];
        exec("/bin/iperf3\0", &args);
        exit(1);
    }
    if server_pid < 0 {
        println!("FAIL: fork server failed");
        return -1;
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
        println!("[{}] {}", i + 1, name);
        let pid = fork();
        if pid < 0 {
            println!("  FAIL: fork failed");
            continue;
        }
        if pid == 0 {
            // Child: run iperf3 client
            let mut strs: Vec<alloc::string::String> = Vec::new();
            strs.push(nullterm("/bin/iperf3"));
            strs.push(nullterm("-c"));
            strs.push(nullterm("127.0.0.1"));
            strs.push(nullterm("-p"));
            strs.push(nullterm("5001"));
            strs.push(nullterm("-t"));
            strs.push(nullterm("2"));
            strs.push(nullterm("-i"));
            strs.push(nullterm("0"));
            for a in *extra {
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
        if code == 0 {
            println!("  PASS");
            passed += 1;
        } else {
            println!("  FAIL (exit={})", code);
        }
        for _ in 0..50 { yield_(); }
    }

    println!("#### RESULT: {}/{} passed ####", passed, tests.len());
    println!("#### OS COMP TEST GROUP END iperf ####");
    0
}
