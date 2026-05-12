#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{getegid, geteuid, getgid, getpid, gettid, getuid, setgid, setuid};

#[no_mangle]
pub fn main() -> i32 {
    println!("--- initial ---");
    println!("getuid: {}", getuid());
    println!("geteuid: {}", geteuid());
    println!("getgid: {}", getgid());
    println!("getegid: {}", getegid());
    println!("gettid: {}", gettid());
    println!("getpid: {}", getpid());

    println!("\n--- setuid(1000) ---");
    setuid(1000);
    println!("getuid: {}", getuid());
    println!("geteuid: {}", geteuid());

    println!("\n--- setgid(500) ---");
    setgid(500);
    println!("getgid: {}", getgid());
    println!("getegid: {}", getegid());

    0
}
