#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{
    close, execve, exit, fork, getdents64, open, shutdown, symlinkat, sys_mkdir, sys_mount,
    unlinkat, waitpid, OpenFlags,
};

const BUSYBOX_APPLETS: [&str; 68] = [
    "busybox", "sh", "ash", "ls", "cat", "echo", "mkdir", "rm", "rmdir", "mv", "cp",
    "true", "false", "env", "pwd", "wc", "grep", "head", "tail", "sort", "uniq", "tr", "find", "touch",
    "test", "[", "[[", "stat", "sleep", "kill", "basename", "dirname", "cut", "expr", "seq", "id",
    "printf", "date", "chmod", "chown", "ln", "dd", "sync", "cmp", "od", "awk", "sed", "xargs",
    "readlink", "mkfifo", "du", "df", "mount", "umount", "chattr", "getconf", "tee", "yes",
    "mknod", "mktemp", "chgrp", "dmesg", "ps", "free", "hostname", "md5sum", "diff", "unlink",
];

fn force_symlink(target: &str, linkpath: &str) -> isize {
    unlinkat(linkpath);
    let r1 = symlinkat(target, linkpath);
    if r1 != 0 {
        unlinkat(linkpath);
        symlinkat(target, linkpath)
    } else {
        0
    }
}

fn bind_library(root: &str, rel_path: &str, dst: &str) {
    let src = String::from(root) + rel_path;
    force_symlink(&src, dst);
    let dst64 = String::from("/lib64/") + dst.trim_start_matches("/lib/");
    force_symlink(&src, &dst64);
}

fn setup_busybox_applets() {
    sys_mkdir(b"/bin\0".as_ptr(), 0o777);
    force_symlink("/busybox", "/bin/busybox\0");
    force_symlink("/busybox", "/bin/sh\0");
    for applet in BUSYBOX_APPLETS {
        let path = String::from("/bin/") + applet + "\0";
        force_symlink("/busybox", &path);
    }
}

fn runtime_roots(root: &str) -> (String, String) {
    if let Some(prefix) = root.strip_suffix("/musl") {
        (String::from(root), String::from(prefix) + "/glibc")
    } else if let Some(prefix) = root.strip_suffix("/glibc") {
        (String::from(prefix) + "/musl", String::from(root))
    } else {
        (String::from(root), String::from(root))
    }
}

fn setup_runtime_for_dir(dir: &str) {
    setup_busybox_applets();
    sys_mkdir(b"/lib\0".as_ptr(), 0o777);
    sys_mkdir(b"/lib64\0".as_ptr(), 0o777);

    let root = dir.trim_end_matches('\0').trim_end_matches('/');
    let (musl_root, glibc_root) = runtime_roots(root);
    println!(
        "[initproc] runtime root {} musl {} glibc {}",
        root, musl_root, glibc_root
    );

    #[cfg(target_arch = "riscv64")]
    {
        bind_library(
            &glibc_root,
            "/lib/ld-linux-riscv64-lp64d.so.1",
            "/lib/ld-linux-riscv64-lp64d.so.1",
        );
        bind_library(&glibc_root, "/lib/libc.so.6", "/lib/libc.so.6");
        bind_library(&glibc_root, "/lib/libm.so.6", "/lib/libm.so.6");
        bind_library(&musl_root, "/lib/libc.so", "/lib/ld-musl-riscv64-sf.so.1");
        bind_library(&musl_root, "/lib/libc.so", "/lib/ld-musl-riscv64.so.1");
        bind_library(&musl_root, "/lib/libc.so", "/lib/libc.so");
    }
    #[cfg(target_arch = "loongarch64")]
    {
        bind_library(
            &glibc_root,
            "/lib/ld-linux-loongarch-lp64d.so.1",
            "/lib/ld-linux-loongarch-lp64d.so.1",
        );
        bind_library(&glibc_root, "/lib/libc.so.6", "/lib/libc.so.6");
        bind_library(&glibc_root, "/lib/libm.so.6", "/lib/libm.so.6");
        bind_library(&musl_root, "/lib/libc.so", "/lib/ld-musl-loongarch64.so.1");
        bind_library(
            &musl_root,
            "/lib/libc.so",
            "/lib/ld-musl-loongarch-lp64d.so.1",
        );
        bind_library(&musl_root, "/lib/libc.so", "/lib/libc.so");
    }
}

fn install_busybox() {
    setup_busybox_applets();
}

fn list_dir(path: &str) -> Vec<String> {
    let fd = open(path, OpenFlags::RDONLY);
    if fd < 0 {
        return Vec::new();
    }
    let mut buf = [0u8; 4096];
    let mut names = Vec::new();
    let mut rounds = 0;
    loop {
        rounds += 1;
        if rounds > 64 {
            break;
        }
        let n = getdents64(fd as usize, &mut buf);
        if n <= 0 {
            break;
        }
        let mut pos = 0;
        while pos < n as usize {
            if pos + 19 > n as usize {
                break;
            }
            let d_reclen = u16::from_le_bytes([buf[pos + 16], buf[pos + 17]]) as usize;
            if d_reclen < 19 || pos + d_reclen > n as usize {
                break;
            }
            let name_start = pos + 19;
            let mut name_len = 0;
            while name_start + name_len < pos + d_reclen && buf[name_start + name_len] != 0 {
                name_len += 1;
            }
            let name = core::str::from_utf8(&buf[name_start..name_start + name_len]).unwrap_or("");
            if name != "." && name != ".." {
                names.push(String::from(name));
            }
            if d_reclen == 0 {
                break;
            }
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

fn script_priority(script: &str) -> u8 {
    if script.starts_with("basic_") {
        0
    } else if script.starts_with("busybox_") {
        1
    } else if script.starts_with("iozone_") {
        2
    } else if script.starts_with("lua_") {
        3
    } else if script.starts_with("lmbench_") {
        4
    } else {
        50
    }
}

fn should_run_script(script: &str) -> bool {
    script.starts_with("basic_")
        || script.starts_with("busybox_")
        || script.starts_with("lua_")
        || script.starts_with("iozone_")
        || script.starts_with("lmbench_")
}

fn script_budget(script: &str) -> &'static str {
    if script.starts_with("basic_") {
        "30"
    } else if script.starts_with("lua_") {
        "10"
    } else if script.starts_with("busybox_") {
        "45"
    } else if script.starts_with("iozone_") {
        "40"
    } else if script.starts_with("lmbench_") {
        "60"
    } else {
        "10"
    }
}

fn sort_scripts(scripts: &mut Vec<String>) {
    scripts.sort_by(|a, b| {
        let pa = script_priority(a);
        let pb = script_priority(b);
        if pa == pb {
            a.as_str().cmp(b.as_str())
        } else {
            pa.cmp(&pb)
        }
    });
}

fn run_script(dir: &str, script: &str) {
    let script_arg = String::from("./") + script + "\0";
    let budget = script_budget(script);
    let _budget_arg = String::from(budget) + "\0";
    let ld_path = String::from("LD_LIBRARY_PATH=")
        + dir.trim_end_matches('\0').trim_end_matches('/')
        + "/lib:/lib:/lib64\0";
    let path = b"PATH=/bin:/\0";
    let args: [*const u8; 3] = [b"sh\0".as_ptr(), script_arg.as_ptr(), core::ptr::null()];
    let envs: [*const u8; 6] = [
        ld_path.as_ptr(),
        path.as_ptr() as *const u8,
        b"ENOUGH=1000\0".as_ptr(),
        b"TIMING_O=0\0".as_ptr(),
        b"LOOP_O=0\0".as_ptr(),
        core::ptr::null(),
    ];
    let pid = fork();
    if pid == 0 {
        user_lib::chdir(dir);
        execve("/busybox\0", &args, &envs);
        exit(127);
    } else if pid > 0 {
        let mut ec = 0;
        waitpid(pid as usize, &mut ec);
        println!(
            "[initproc] [DONE] {}{} exit {}",
            dir.trim_end_matches('\0'),
            script,
            ec
        );
    }
}

fn scan_and_run(dir: &str, depth: usize) -> bool {
    let entries = list_dir(dir);
    println!(
        "[initproc] {} has {} entries",
        dir.trim_end_matches('\0'),
        entries.len()
    );

    let mut found = false;
    let mut scripts: Vec<String> = entries
        .iter()
        .filter(|entry| entry.ends_with("_testcode.sh"))
        .filter(|entry| should_run_script(entry))
        .cloned()
        .collect();
    sort_scripts(&mut scripts);
    for entry in &scripts {
        if entry.ends_with("_testcode.sh") {
            setup_runtime_for_dir(dir);
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
    println!("[initproc] scan begin");

    let mut found = false;
    for dir in ["/mnt/musl/\0", "/mnt/glibc/\0"] {
        if scan_and_run(dir, 0) {
            found = true;
        }
    }
    #[cfg(target_arch = "riscv64")]
    let arch_dirs: [&str; 2] = ["/mnt/riscv/musl/\0", "/mnt/riscv/glibc/\0"];
    #[cfg(target_arch = "loongarch64")]
    let arch_dirs: [&str; 2] = ["/mnt/loongarch/musl/\0", "/mnt/loongarch/glibc/\0"];
    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    let arch_dirs: [&str; 0] = [];
    if !found {
        for dir in arch_dirs {
            if scan_and_run(dir, 0) {
                found = true;
            }
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
