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
    let mappings: [(&str, &str); 6] = [
        ("/mnt/glibc/lib/ld-linux-riscv64-lp64d.so.1", "/lib/ld-linux-riscv64-lp64d.so.1"),
        ("/mnt/glibc/lib/libc.so.6", "/lib/libc.so.6"),
        ("/mnt/glibc/lib/libm.so.6", "/lib/libm.so.6"),
        ("/mnt/musl/lib/libc.so", "/lib/ld-musl-riscv64-sf.so.1"),
        ("/mnt/musl/lib/libc.so", "/lib/ld-musl-riscv64.so.1"),
        ("/mnt/musl/lib/libc.so", "/lib/libc.so"),
    ];
    #[cfg(not(target_arch = "riscv64"))]
    let mappings: [(&str, &str); 0] = [];

    for (src, dst) in &mappings {
        let src_z = String::from(*src) + "\0";
        let dst_z = String::from(*dst) + "\0";
        symlinkat(&src_z, &dst_z);
        let dst64 = String::from("/lib64/") + dst.trim_start_matches("/lib/") + "\0";
        symlinkat(&src_z, &dst64);
    }
}

fn install_busybox() {
    sys_mkdir(b"/bin\0".as_ptr(), 0o777);
    let ret = symlinkat("/busybox\0", "/bin/sh\0");
    println!("[initproc] symlink /bin/sh -> /busybox: {}", ret);
    // Also try busybox's own install for full applet set
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

fn run_script(dir: &str, script: &str, bb: &str) -> i32 {
    println!("[initproc] run {} in {} with {}", script, dir, bb);
    let script_arg = String::from("./") + script + "\0";
    let args: [*const u8; 3] = [b"sh\0".as_ptr(), script_arg.as_ptr(), core::ptr::null()];
    let pid = fork();
    if pid == 0 {
        let cd = user_lib::chdir(dir);
        if cd != 0 {
            println!("[initproc] chdir({}) failed: {}", dir, cd);
            exit(126);
        }
        let er = exec(bb, &args);
        println!("[initproc] exec({}) failed: {}", bb, er);
        exit(127);
    } else if pid > 0 {
        let mut ec = 0;
        let wp = waitpid(pid as usize, &mut ec);
        println!("[initproc] script {} waitpid={} exit={}", script, wp, ec);
        ec
    } else {
        println!("[initproc] fork failed for {}: {}", script, pid);
        127
    }
}

fn run_busybox_fallback(glibc: bool) -> i32 {
    let (bb, script, name) = if glibc {
        ("/busybox-glibc\0", "/busybox_glibc_testcode.sh\0", "busybox-glibc")
    } else {
        ("/busybox\0", "/busybox_testcode.sh\0", "busybox-musl")
    };
    println!("[initproc] fallback run {} from rootfs", name);
    let args: [*const u8; 4] = [b"busybox\0".as_ptr(), b"sh\0".as_ptr(), script.as_ptr(), core::ptr::null()];
    let pid = fork();
    if pid == 0 {
        let cd = user_lib::chdir("/\0");
        if cd != 0 {
            println!("[initproc] chdir(/) failed: {}", cd);
            exit(126);
        }
        let er = exec(bb, &args);
        println!("[initproc] fallback exec({}) failed: {}", bb, er);
        exit(127);
    } else if pid > 0 {
        let mut ec = 0;
        let wp = waitpid(pid as usize, &mut ec);
        println!("[initproc] fallback {} waitpid={} exit={}", name, wp, ec);
        ec
    } else {
        println!("[initproc] fallback fork failed for {}: {}", name, pid);
        127
    }
}

fn run_program(path: &str, name: &str) -> i32 {
    println!("[initproc] fallback run {}", name);
    let args: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
    let pid = fork();
    if pid == 0 {
        let er = exec(path, &args);
        println!("[initproc] exec({}) failed: {}", path, er);
        exit(127);
    } else if pid > 0 {
        let mut ec = 0;
        let wp = waitpid(pid as usize, &mut ec);
        println!("[initproc] fallback {} waitpid={} exit={}", name, wp, ec);
        ec
    } else {
        println!("[initproc] fallback fork failed for {}: {}", name, pid);
        127
    }
}

fn run_basic_fallback() {
    run_program("/contest_runner_musl\0", "basic-musl");
    run_program("/contest_runner_glibc\0", "basic-glibc");
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

    // Scan /mnt for subdirectories containing *_testcode.sh (A20-style)
    let entries = list_dir("/mnt/\0");
    let mut found = false;
    for sub in &entries {
        if sub == "lost+found" { continue; }
        let subpath = String::from("/mnt/") + sub + "/\0";
        let scripts = list_dir(&subpath);
        let has_busybox = scripts.iter().any(|name| name == "busybox");
        let has_busybox_cmd = scripts.iter().any(|name| name == "busybox_cmd.txt");
        for s in &scripts {
            if !s.ends_with("_testcode.sh") { continue; }
            found = true;
            let bb = if sub == "glibc" { "/busybox-glibc\0" } else { "/busybox\0" };
            println!("[initproc] [RUN] {}/{}", sub, s);
            if s == "busybox_testcode.sh" && (!has_busybox || !has_busybox_cmd) {
                println!(
                    "[initproc] {}/busybox layout incomplete: busybox={}, cmd={}; using rootfs fallback",
                    sub, has_busybox, has_busybox_cmd
                );
                run_busybox_fallback(sub == "glibc");
                continue;
            }
            let code = run_script(&subpath, s, bb);
            if s == "busybox_testcode.sh" && code != 0 {
                println!("[initproc] external busybox script exited {}; trying rootfs fallback", code);
                run_busybox_fallback(sub == "glibc");
            }
        }
    }

    // Fallback: scripts at /mnt root
    if !found {
        for s in &entries {
            if !s.ends_with("_testcode.sh") { continue; }
            println!("[initproc] [RUN] /mnt/{}", s);
            if s == "basic_testcode.sh" {
                println!("[initproc] root basic_testcode.sh uses arch-suffixed groups; using rootfs basic runners");
                run_basic_fallback();
                continue;
            }
            run_script("/mnt/\0", s, "/busybox\0");
        }
    }

    println!("[initproc] === DONE ===");
    shutdown();
    0
}
