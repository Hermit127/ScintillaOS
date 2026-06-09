#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{chdir, close, execve, exit, fork, getdents64, open, shutdown, sys_mkdir, sys_mount, waitpid, OpenFlags};

const NULL: *const u8 = core::ptr::null::<u8>();

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

fn try_mount(mp: &str, dev: &str) -> bool {
    sys_mkdir(mp.as_ptr(), 0o777);
    sys_mount(dev, mp, "ext4", 0, &[]) == 0
}

fn bind_loaders(mount: &str) {
    // Copy dynamic loaders from contest disk to /lib and /lib64
    let mappings: &[(&str, &str)] = &[
        ("glibc/lib/ld-linux-riscv64-lp64d.so.1", "/lib/ld-linux-riscv64-lp64d.so.1"),
        ("glibc/lib/libc.so.6", "/lib/libc.so.6"),
        ("glibc/lib/libm.so.6", "/lib/libm.so.6"),
        ("musl/lib/libc.so", "/lib/ld-musl-riscv64-sf.so.1"),
        ("musl/lib/libc.so", "/lib/ld-musl-riscv64.so.1"),
        ("musl/lib/libc.so", "/lib/libc.so"),
    ];
    for (src, dst) in mappings {
        let src_path = String::from(mount) + "/" + src + "\0";
        if open(&src_path, OpenFlags::RDONLY) >= 0 {
            println!("[initproc] bind {} -> {}", src, dst);
            // Use busybox cp (it's at /busybox from rootfs)
            let pid = fork();
            if pid == 0 {
                let bb = "/busybox\0";
                let args: [*const u8; 4] = [b"cp\0".as_ptr(), src_path.as_ptr(), dst.as_ptr(), NULL];
                execve(bb, &args, &[NULL]);
                exit(0);
            } else if pid > 0 {
                let mut ec = 0; waitpid(pid as usize, &mut ec);
            }
        }
    }
}

fn run_script(variant: &str, mount: &str, script: &str) -> i32 {
    let dir = String::from(mount) + "/" + variant;
    let dir_nul = dir.clone() + "\0";
    let lib_path = String::from("LD_LIBRARY_PATH=") + &dir + "/lib:/lib:/lib64\0";
    let envs: [*const u8; 2] = [lib_path.as_ptr(), NULL];
    
    let script_arg = String::from("./") + script + "\0";
    let args: [*const u8; 3] = [b"sh\0".as_ptr(), script_arg.as_ptr(), NULL];

    let pid = fork();
    if pid == 0 {
        chdir(&dir_nul);
        execve("./busybox\0", &args, &envs);
        // Fallback: try /busybox
        execve("/busybox\0", &args, &envs);
        exit(127);
    } else if pid > 0 {
        let mut ec = 0;
        waitpid(pid as usize, &mut ec);
        return ec;
    }
    -1
}

fn skip_group(name: &str) -> bool {
    matches!(name, "libctest" | "libcbench" | "unixbench" | "cyclictest" | "netperf" | "iperf")
}

#[no_mangle]
fn main() -> i32 {
    // Try mounting vda (bus 0) at /mnt
    if !try_mount("/mnt\0", "vda\0") {
        // If vda fails, try vdb (bus 1)
        if !try_mount("/mnt\0", "vdb\0") {
            println!("[initproc] mount failed, no tests");
            shutdown();
            return 0;
        }
    }

    // Bind dynamic loaders
    bind_loaders("/mnt");

    // Scan variants
    for variant in ["musl", "glibc"] {
        let dir_nul = String::from("/mnt/") + variant + "\0";
        let scripts = list_dir(&dir_nul);
        println!("[initproc] /mnt/{}/ has {} scripts", variant, scripts.len());

        for s in &scripts {
            if !s.ends_with("_testcode.sh") { continue; }
            let group = s.trim_end_matches("_testcode.sh");
            if skip_group(group) { println!("[SKIP] {} ({})", group, variant); continue; }

            println!("[RUN] {} ({})", group, variant);
            let ec = run_script(variant, "/mnt", s);
            // Ensure END marker is always printed
            println!("#### OS COMP TEST GROUP END {}-{} ####", group, variant);
        }
    }

    shutdown();
    0
}
