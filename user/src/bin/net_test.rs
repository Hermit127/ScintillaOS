#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
use user_lib::*;

// Linux RISC-V syscall numbers
const SYS_SOCKET: usize = 198;
const SYS_BIND: usize = 200;
const SYS_LISTEN: usize = 201;
const SYS_CLOSE: usize = 57;
const SYS_WRITE: usize = 64;
const SYS_EXIT: usize = 93;

const AF_INET: usize = 2;
const SOCK_STREAM: usize = 1;

#[repr(C)]
struct SockAddrIn {
    sin_family: u16,
    sin_port: u16,
    sin_addr: u32,
    sin_zero: [u8; 8],
}

fn sockaddr(port: u16) -> SockAddrIn {
    SockAddrIn {
        sin_family: AF_INET as u16,
        sin_port: port.to_be(),
        sin_addr: 0, // INADDR_ANY
        sin_zero: [0; 8],
    }
}

fn sys_socket(domain: usize, socket_type: usize, protocol: usize) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") domain => ret,
            in("x11") socket_type,
            in("x12") protocol,
            in("x17") SYS_SOCKET
        );
    }
    ret
}

fn sys_bind(fd: usize, addr: usize, addrlen: usize) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") fd => ret,
            in("x11") addr,
            in("x12") addrlen,
            in("x17") SYS_BIND
        );
    }
    ret
}

fn sys_listen(fd: usize, backlog: usize) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") fd => ret,
            in("x11") backlog,
            in("x17") SYS_LISTEN
        );
    }
    ret
}

#[no_mangle]
pub fn main() -> i32 {
    println!("net_test: creating socket...");
    let fd = sys_socket(AF_INET, SOCK_STREAM, 0);
    if fd < 0 {
        println!("net_test: socket() failed: {}", fd);
        return -1;
    }
    println!("net_test: socket() -> fd={}", fd);

    let addr = sockaddr(5001);
    println!("net_test: binding to 0.0.0.0:5001...");
    let ret = sys_bind(fd as usize, &addr as *const SockAddrIn as usize, core::mem::size_of::<SockAddrIn>());
    if ret < 0 {
        println!("net_test: bind() failed: {}", ret);
        return -1;
    }
    println!("net_test: bind() ok");

    println!("net_test: listening...");
    let ret = sys_listen(fd as usize, 5);
    if ret < 0 {
        println!("net_test: listen() failed: {}", ret);
        return -1;
    }
    println!("net_test: listen() ok, server ready on port 5001");
    println!("net_test: PASSED");
    0
}
