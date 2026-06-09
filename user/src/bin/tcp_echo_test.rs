#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{exit, fork, sleep, waitpid};

const SYS_SOCKET: usize = 198;
const SYS_BIND: usize = 200;
const SYS_LISTEN: usize = 201;
const SYS_ACCEPT: usize = 202;
const SYS_CONNECT: usize = 203;
const SYS_CLOSE: usize = 57;
const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_SETSOCKOPT: usize = 208;

const AF_INET: usize = 2;
const SOCK_STREAM: usize = 1;
const SOL_SOCKET: usize = 1;
const SO_REUSEADDR: usize = 2;

#[cfg(target_arch = "riscv64")]
fn sys_call(id: usize, args: [usize; 6]) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") args[0] => ret,
            in("x11") args[1],
            in("x12") args[2],
            in("x13") args[3],
            in("x14") args[4],
            in("x15") args[5],
            in("x17") id
        );
    }
    ret
}

#[cfg(target_arch = "loongarch64")]
fn sys_call(id: usize, args: [usize; 6]) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "syscall 0",
            inlateout("$r4") args[0] => ret,
            in("$r5") args[1],
            in("$r6") args[2],
            in("$r7") args[3],
            in("$r8") args[4],
            in("$r9") args[5],
            in("$r11") id
        );
    }
    ret
}

fn sockaddr(addr: u32, port: u16) -> [u8; 16] {
    let mut sa = [0u8; 16];
    sa[0] = AF_INET as u8;
    let port_be = port.to_be();
    sa[2] = port_be as u8;
    sa[3] = (port_be >> 8) as u8;
    let addr_be = addr.to_be();
    sa[4] = addr_be as u8;
    sa[5] = (addr_be >> 8) as u8;
    sa[6] = (addr_be >> 16) as u8;
    sa[7] = (addr_be >> 24) as u8;
    sa
}

fn run_server() {
    let fd = sys_call(SYS_SOCKET, [AF_INET, SOCK_STREAM, 0, 0, 0, 0]);
    println!("server socket={}", fd);
    if fd < 0 { exit(1); }
    let fd = fd as usize;

    let opt: i32 = 1;
    let opt_ptr = &opt as *const i32 as usize;
    sys_call(SYS_SETSOCKOPT, [fd, SOL_SOCKET, SO_REUSEADDR, opt_ptr, 4, 0]);

    let sa = sockaddr(0, 9999);
    let sa_ptr = sa.as_ptr() as usize;
    let ret = sys_call(SYS_BIND, [fd, sa_ptr, 16, 0, 0, 0]);
    println!("server bind={}", ret);
    if ret < 0 { exit(1); }

    let ret = sys_call(SYS_LISTEN, [fd, 5, 0, 0, 0, 0]);
    println!("server listen={}", ret);
    if ret < 0 { exit(1); }

    println!("server listening on port 9999");

    let cfd = sys_call(SYS_ACCEPT, [fd, 0, 0, 0, 0, 0]);
    println!("server accepted fd={}", cfd);
    if cfd < 0 { exit(1); }
    let cfd = cfd as usize;

    let mut buf = [0u8; 64];
    let n = sys_call(SYS_READ, [cfd, buf.as_mut_ptr() as usize, 64, 0, 0, 0]);
    println!("server read n={} data={:?}", n, &buf[..n as usize]);

    let n2 = sys_call(SYS_WRITE, [cfd, buf.as_mut_ptr() as usize, n as usize, 0, 0, 0]);
    println!("server wrote n2={}", n2);

    sys_call(SYS_CLOSE, [cfd, 0, 0, 0, 0, 0]);
    sys_call(SYS_CLOSE, [fd, 0, 0, 0, 0, 0]);
    println!("server done");
    exit(0);
}

fn run_client() {
    let fd = sys_call(SYS_SOCKET, [AF_INET, SOCK_STREAM, 0, 0, 0, 0]);
    println!("client socket={}", fd);
    if fd < 0 { exit(1); }
    let fd = fd as usize;

    let sa = sockaddr(0x7f000001u32, 9999); // 127.0.0.1
    let sa_ptr = sa.as_ptr() as usize;
    let ret = sys_call(SYS_CONNECT, [fd, sa_ptr, 16, 0, 0, 0]);
    println!("client connect={}", ret);
    if ret < 0 { exit(1); }

    let msg = b"HELLO_TCP\0";
    let n = sys_call(SYS_WRITE, [fd, msg.as_ptr() as usize, 10, 0, 0, 0]);
    println!("client wrote n={}", n);

    let mut buf = [0u8; 64];
    let n = sys_call(SYS_READ, [fd, buf.as_mut_ptr() as usize, 64, 0, 0, 0]);
    println!("client read n={} data={:?}", n, &buf[..n as usize]);

    let success = n == 10 && &buf[0..10] == b"HELLO_TCP\0";
    if success {
        println!("CLIENT VERIFY: DATA MATCH!");
    } else {
        println!("CLIENT VERIFY: MISMATCH!");
    }

    sys_call(SYS_CLOSE, [fd, 0, 0, 0, 0, 0]);
    println!("client done");
    exit(if success { 0 } else { 1 });
}

#[no_mangle]
pub fn main() -> i32 {
    println!("===== PURE TCP ECHO TEST =====");

    let server_pid = fork();
    if server_pid == 0 {
        run_server();
    }
    println!("server pid={}", server_pid);
    sleep(500);

    let client_pid = fork();
    if client_pid == 0 {
        run_client();
    }
    println!("client pid={}", client_pid);

    let mut exit_code: i32 = 0;
    waitpid(client_pid as usize, &mut exit_code);
    println!("client exited with {}", exit_code);

    if exit_code == 0 {
        println!("===== TCP ECHO TEST PASSED =====");
    } else {
        println!("===== TCP ECHO TEST FAILED =====");
    }
    0
}
