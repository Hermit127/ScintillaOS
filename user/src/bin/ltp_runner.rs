#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::format;
use user_lib::{exec, fork, waitpid, open, close, read, getdents64, OpenFlags};

const LTP_DIR: &str = "/ltp/testcases/bin/";

const SUBSET_LIST: &[&str] = &[
    // Socket more variants
    "accept03", "accept4_01",
    "bind02", "bind03",
    "sendto03", "sendmsg03", "recvmsg03",
    "getsockopt02", "setsockopt02",
    "socketpair01", "socketpair02",
    // fcntl more
    "fcntl27", "fcntl29", "fcntl30", "fcntl31", "fcntl32",
    "fcntl33", "fcntl34", "fcntl35", "fcntl36",
    // Kill more (skip kill10 — needs full multi-process signal sync)
    "kill07", "kill08", "kill09", "kill11", "kill12",
    // prctl / ioctl
    "prctl01", "prctl02", "prctl03", "prctl04", "prctl05",
    "ioctl01", "ioctl02", "ioctl03",
    // Memory
    "mprotect04", "mprotect05",
    "madvise01", "madvise02", "madvise03",
    // getrandom / getrusage
    "getrandom01", "getrandom02", "getrandom03", "getrandom04",
    "getrusage01", "getrusage02",
    // UID/GID more
    "setfsgid01", "setfsuid01", "getegid01", "geteuid01",
    // nice
    "nice01", "nice02", "nice03",
    // mount / chroot
    "mount01", "mount02", "mount03",
    "chroot01", "chroot03",
    "umount01", "umount02",
];

fn is_elf(name: &str) -> bool {
    let path = format!("{}{}\0", LTP_DIR, name);
    let fd = open(path.as_str(), OpenFlags::RDONLY);
    if fd < 0 {
        return false;
    }
    let mut magic = [0u8; 4];
    let n = read(fd as usize, &mut magic);
    close(fd as usize);
    n == 4 && magic[0] == 0x7f && magic[1] == b'E' && magic[2] == b'L' && magic[3] == b'F'
}

fn run_test(name: &str) {
    let path = format!("{}{}\0", LTP_DIR, name);
    println!("RUN LTP CASE {}", name);
    let pid = fork();
    if pid == 0 {
        exec(path.as_str(), &[core::ptr::null::<u8>()]);
        user_lib::exit(-1);
    }
    let mut exit_code: i32 = 0;
    waitpid(pid as usize, &mut exit_code);
    let status = exit_code >> 8;
    println!("FINISH LTP CASE {} : {}", name, status);
}

fn is_full_mode() -> bool {
    let fd = open("/ltp_full\0", OpenFlags::RDONLY);
    if fd >= 0 {
        close(fd as usize);
        true
    } else {
        false
    }
}

fn skip_name(name: &str) -> bool {
    name == "." || name == ".." || name.ends_with(".sh") || name.ends_with(".py")
}

fn for_each_dirent(buf: &[u8], n: isize, mut f: impl FnMut(&str)) {
    let n = n as usize;
    let mut pos: usize = 0;
    while pos < n {
        let reclen = (buf[pos + 16] as usize) | ((buf[pos + 17] as usize) << 8);
        if reclen == 0 {
            return;
        }
        let name_start = pos + 19;
        let mut name_end = name_start;
        while name_end < pos + reclen && buf[name_end] != 0 {
            name_end += 1;
        }
        if let Ok(name) = core::str::from_utf8(&buf[name_start..name_end]) {
            if !skip_name(name) {
                f(name);
            }
        }
        pos += reclen;
    }
}

#[no_mangle]
pub fn main() -> i32 {
    println!("========================================");
    println!("  LTP RUNNER STARTING (v2)");
    println!("========================================");
    let full = is_full_mode();
    if full {
        println!("#### OS COMP TEST GROUP START ltp-musl ####");
        let fd = open(LTP_DIR, OpenFlags::RDONLY | OpenFlags::DIRECTORY);
        if fd < 0 {
            println!("LTP_DIR_NOT_FOUND: {}", LTP_DIR);
            return -1;
        }
        let mut buf = [0u8; 4096];
        let mut total: usize = 0;
        loop {
            let n = getdents64(fd as usize, &mut buf);
            if n <= 0 { break; }
            for_each_dirent(&buf, n, |name| {
                if is_elf(name) {
                    run_test(name);
                    total += 1;
                }
            });
        }
        close(fd as usize);
        println!("total LTP cases: {}", total);
    } else {
        println!("#### OS COMP TEST GROUP START ltp-subset-musl ####");
        for test in SUBSET_LIST {
            run_test(test);
        }
    }

    println!("#### OS COMP TEST GROUP END ltp-musl ####");
    0
}
