#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{chdir, close, exec, exit, fork, getdents64, open, shutdown, sys_mkdir, sys_mount, waitpid, OpenFlags};

const NULL: *const u8 = core::ptr::null::<u8>();

fn open_dir(path: &str) -> isize {
    open(path, OpenFlags::RDONLY)
}

fn list_dir(path: &str) -> Vec<String> {
    let fd = open_dir(path);
    if fd < 0 { return Vec::new(); }
    let mut buf = [0u8; 4096];
    let mut names = Vec::new();
    loop {
        let n = getdents64(fd as usize, &mut buf);
        if n <= 0 { break; }
        let mut pos = 0;
        while pos < n as usize {
            let d_reclen = u16::from_le_bytes([buf[pos+16], buf[pos+17]]) as usize;
            let d_type = buf[pos+18];
            let name_start = pos + 19;
            let mut name_len = 0;
            while name_start + name_len < pos + d_reclen && buf[name_start + name_len] != 0 {
                name_len += 1;
            }
            let name = core::str::from_utf8(&buf[name_start..name_start+name_len]).unwrap_or("");
            if name != "." && name != ".." {
                names.push(String::from(name));
            }
            if d_reclen == 0 { break; }
            pos += d_reclen;
        }
    }
    close(fd as usize);
    names
}

fn run_cmd(dir: &str, bin: &str, args: &[*const u8]) {
    let pid = fork();
    if pid == 0 {
        if !dir.is_empty() { chdir(dir); }
        exec(bin, args);
        exit(127);
    } else if pid > 0 {
        let mut ec = 0;
        waitpid(pid as usize, &mut ec);
    }
}

fn skip_group(name: &str) -> bool {
    matches!(name, "libctest" | "libcbench" | "unixbench" | "cyclictest" | "netperf" | "iperf" | "ltp")
}

#[no_mangle]
fn main() -> i32 {
    sys_mkdir(b"/mnt\0".as_ptr(), 0o777);
    sys_mount("", "/mnt", "ext4", 0, &[]);

    // Copy loaders
    let _ = list_dir("/mnt/musl/lib\0");
    let _ = list_dir("/mnt/glibc/lib\0");

    for variant in ["musl", "glibc"] {
        let dir = String::from("/mnt/") + variant;
        let dir_nul = String::from("/mnt/") + variant + "\0";
        let files = list_dir(&dir_nul);
        if files.is_empty() { continue; }

        let bb = if variant == "glibc" && open("/busybox-glibc\0", OpenFlags::RDONLY) >= 0 {
            "/busybox-glibc\0"
        } else {
            "/busybox\0"
        };
        let sh: &[u8] = b"sh\0";

        for f in &files {
            if !f.ends_with("_testcode.sh") { continue; }
            let group = f.trim_end_matches("_testcode.sh");
            if skip_group(group) {
                println!("[SKIP] {} ({})", group, variant);
                continue;
            }
            println!("[RUN] {} ({})", group, variant);

            let script = String::from("./") + f + "\0";
            run_cmd(&dir, bb, &[sh.as_ptr(), script.as_ptr(), NULL]);
        }
    }

    shutdown();
    0
}
