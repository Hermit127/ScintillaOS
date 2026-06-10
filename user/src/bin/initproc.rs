#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{close, exec, exit, fork, getdents64, open, shutdown, sys_mkdir, sys_mount, waitpid, OpenFlags};

fn install_busybox() {
    sys_mkdir(b"/bin\0".as_ptr(), 0o777);
    let args: [*const u8; 5] = [
        b"/busybox\0".as_ptr(),
        b"--install\0".as_ptr(),
        b"-s\0".as_ptr(),
        b"/bin\0".as_ptr(),
        core::ptr::null(),
    ];
    let pid = fork();
    if pid == 0 {
        exec("/busybox\0", &args);
        exit(0);
    } else if pid > 0 {
        let mut ec = 0;
        waitpid(pid as usize, &mut ec);
    }
}

fn list_dir(path: &str) -> Vec<String> {
    let fd = open(path, OpenFlags::RDONLY);
    if fd < 0 { return Vec::new(); }
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
    names
}

fn run_script(dir: &str, script: &str) {
    let script_arg = String::from("./") + script + "\0";
    let args: [*const u8; 3] = [b"sh\0".as_ptr(), script_arg.as_ptr(), core::ptr::null()];
    let pid = fork();
    if pid == 0 {
        user_lib::chdir(dir);
        exec("/busybox\0", &args);
        exit(127);
    } else if pid > 0 { let mut ec = 0; waitpid(pid as usize, &mut ec); }
}

#[no_mangle]
fn main() -> i32 {
    println!("[initproc] === START ===");

    install_busybox();

    sys_mkdir(b"/mnt\0".as_ptr(), 0o777);
    let r = sys_mount("vda\0", "/mnt\0", "ext4\0", 0, &[]);
    println!("[initproc] mount vda -> /mnt: {}", r);

    // Scan /mnt for subdirectories containing *_testcode.sh (A20-style)
    let entries = list_dir("/mnt/\0");
    let mut found = false;
    for sub in &entries {
        if sub == "lost+found" { continue; }
        let subpath = String::from("/mnt/") + sub + "/\0";
        let scripts = list_dir(&subpath);
        for s in &scripts {
            if !s.ends_with("_testcode.sh") { continue; }
            found = true;
            println!("[initproc] [RUN] {}/{}", sub, s);
            run_script(&subpath, s);
        }
    }

    // Fallback: scripts at /mnt root
    if !found {
        for s in &entries {
            if !s.ends_with("_testcode.sh") { continue; }
            println!("[initproc] [RUN] /mnt/{}", s);
            run_script("/mnt/\0", s);
        }
    }

    println!("[initproc] === DONE ===");
    shutdown();
    0
}
