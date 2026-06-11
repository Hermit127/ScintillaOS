#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{close, exec, exit, fork, getdents64, open, shutdown, symlinkat, sys_mkdir, sys_mount, waitpid, OpenFlags};

fn bind_loaders() {
    sys_mkdir(b"/lib\0".as_ptr(), 0o777);
    sys_mkdir(b"/lib64\0".as_ptr(), 0o777);

    #[cfg(target_arch = "riscv64")]
    let mappings: [(&str, &str); 8] = [
        ("/mnt/glibc/lib/ld-linux-riscv64-lp64d.so.1", "/lib/ld-linux-riscv64-lp64d.so.1"),
        ("/mnt/glibc/lib/libc.so.6", "/lib/libc.so.6"),
        ("/mnt/glibc/lib/libc.so", "/lib/libc.so.6"),
        ("/mnt/glibc/lib/libm.so.6", "/lib/libm.so.6"),
        ("/mnt/glibc/lib/libm.so", "/lib/libm.so.6"),
        ("/mnt/musl/lib/libc.so", "/lib/ld-musl-riscv64-sf.so.1"),
        ("/mnt/musl/lib/libc.so", "/lib/ld-musl-riscv64.so.1"),
        ("/mnt/musl/lib/libc.so", "/lib/libc.so"),
    ];
    #[cfg(not(target_arch = "riscv64"))]
    let mappings: [(&str, &str); 0] = [];

    for (src, dst) in &mappings {
        symlinkat(src, dst);
        let dst64 = alloc::string::String::from("/lib64/") + dst.trim_start_matches("/lib/");
        symlinkat(src, &dst64);
    }
}

fn install_busybox() {
    sys_mkdir(b"/bin\0".as_ptr(), 0o777);
    let ret = symlinkat("/busybox\0", "/bin/sh\0");
    if ret == 0 { println!("[initproc] /bin/sh created"); }

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
        println!("[initproc] busybox --install exit: {}", ec);
    }
}

fn list_dir(path: &str) -> Vec<String> {
    let fd = open(path, OpenFlags::RDONLY);
    if fd < 0 { return Vec::new(); }
    let mut buf = [0u8; 4096];
    let mut names = Vec::new();
    let mut rounds = 0;
    loop {
        rounds += 1;
        if rounds > 64 { break; }
        let n = getdents64(fd as usize, &mut buf);
        if n <= 0 { break; }
        let mut pos = 0;
        while pos < n as usize {
            if pos + 19 > n as usize { break; }
            let d_reclen = u16::from_le_bytes([buf[pos+16], buf[pos+17]]) as usize;
            if d_reclen < 19 || pos + d_reclen > n as usize { break; }
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

fn join_path(dir: &str, name: &str) -> String {
    let mut base = String::from(dir.trim_end_matches('\0'));
    if !base.ends_with('/') {
        base.push('/');
    }
    base.push_str(name);
    base
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

fn scan_and_run(dir: &str, depth: usize) -> bool {
    let entries = list_dir(dir);
    println!(
        "[initproc] {} has {} entries",
        dir.trim_end_matches('\0'),
        entries.len()
    );

    let mut found = false;
    for entry in &entries {
        if entry.ends_with("_testcode.sh") {
            println!("[initproc] [RUN] {}{}", dir.trim_end_matches('\0'), entry);
            run_script(dir, entry);
            found = true;
        }
    }

    if depth == 0 {
        return found;
    }

    for entry in &entries {
        if entry == "lost+found" || entry.ends_with(".sh") || entry.ends_with(".txt") {
            continue;
        }
        let mut next = join_path(dir, entry);
        next.push_str("/\0");
        if scan_and_run(&next, depth - 1) {
            found = true;
        }
    }
    found
}

#[no_mangle]
fn main() -> i32 {
    println!("[initproc] === START ===");

    install_busybox();

    sys_mkdir(b"/mnt\0".as_ptr(), 0o777);
    let r = sys_mount("vda\0", "/mnt\0", "ext4\0", 0, &[]);
    println!("[initproc] mount vda -> /mnt: {}", r);

    if r == 0 {
        bind_loaders();
    }

    #[cfg(target_arch = "riscv64")]
    let candidate_dirs: [&str; 6] = [
        "/mnt/riscv/musl/\0",
        "/mnt/riscv/glibc/\0",
        "/mnt/musl/\0",
        "/mnt/glibc/\0",
        "/mnt/basic/\0",
        "/mnt/\0",
    ];
    #[cfg(target_arch = "loongarch64")]
    let candidate_dirs: [&str; 6] = [
        "/mnt/loongarch/musl/\0",
        "/mnt/loongarch/glibc/\0",
        "/mnt/musl/\0",
        "/mnt/glibc/\0",
        "/mnt/basic/\0",
        "/mnt/\0",
    ];
    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    let candidate_dirs: [&str; 4] = ["/mnt/musl/\0", "/mnt/glibc/\0", "/mnt/basic/\0", "/mnt/\0"];

    let mut found = false;
    for dir in &candidate_dirs {
        if scan_and_run(dir, 0) {
            found = true;
        }
    }
    if !found {
        found = scan_and_run("/mnt/\0", 4);
    }
    if !found {
        println!("[initproc] no test scripts found");
    }

    println!("[initproc] === DONE ===");
    shutdown();
    0
}
