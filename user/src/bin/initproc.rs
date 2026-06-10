#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{close, exec, exit, fork, getdents64, open, shutdown, sys_mkdir, sys_mount, waitpid, OpenFlags};

fn list_dir(path: &str) -> Vec<String> {
    println!("[initproc] list_dir({})", path);
    let fd = open(path, OpenFlags::RDONLY);
    if fd < 0 { println!("[initproc] open failed: {}", fd); return Vec::new(); }
    let mut buf = [0u8; 4096];
    let mut names = Vec::new();
    loop {
        let n = getdents64(fd as usize, &mut buf);
        if n <= 0 { break; }
        let mut pos = 0;
        while pos < n as usize {
            let d_reclen = u16::from_le_bytes([buf[pos+16], buf[pos+17]]) as usize;
            let name_start = pos + 19;
            let mut name_len = 0;
            while name_start + name_len < pos + d_reclen && buf[name_start + name_len] != 0 { name_len += 1; }
            let name = core::str::from_utf8(&buf[name_start..name_start+name_len]).unwrap_or("");
            if name != "." && name != ".." { names.push(String::from(name)); }
            if d_reclen == 0 { break; }
            pos += d_reclen;
        }
    }
    close(fd as usize);
    println!("[initproc] list_dir({}) -> {} entries", path, names.len());
    names
}

fn run_script(dir: &str, script: &str, bb: &str) {
    println!("[initproc] run {} in {} with {}", script, dir, bb);
    let script_arg = String::from("./") + script + "\0";
    let args: [*const u8; 3] = [b"sh\0".as_ptr(), script_arg.as_ptr(), core::ptr::null()];
    let pid = fork();
    if pid == 0 {
        user_lib::chdir(dir);
        exec(bb, &args);
        exit(127);
    } else if pid > 0 { let mut ec = 0; waitpid(pid as usize, &mut ec); }
}

#[no_mangle]
fn main() -> i32 {
    println!("[initproc] === START ===");
    
    sys_mkdir(b"/mnt\0".as_ptr(), 0o777);
    let r = sys_mount("vda\0", "/mnt\0", "ext4\0", 0, &[]);
    println!("[initproc] mount vda -> /mnt: {}", r);

    for v in &["musl", "glibc"] {
        let path = String::from("/mnt/") + v + "/\0";
        let scripts = list_dir(&path);
        for s in &scripts {
            if !s.ends_with("_testcode.sh") { continue; }
            let dir = String::from("/mnt/") + v + "\0";
            let bb = if *v == "glibc" { "/busybox-glibc\0" } else { "/busybox\0" };
            println!("[initproc] [RUN] {}/{}", v, s);
            run_script(&dir, s, bb);
        }
    }

    println!("[initproc] === DONE ===");
    shutdown();
    0
}
