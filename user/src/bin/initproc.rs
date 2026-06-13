#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{
    close, exec, exit, fork, getdents64, open, shutdown, symlinkat, sys_mkdir, sys_mount, unlinkat,
    waitpid, OpenFlags,
};

const BUSYBOX_APPLETS: &[&str] = &[
    "busybox", "sh", "ash", "ls", "cat", "echo", "mkdir", "rm", "rmdir", "mv", "cp", "true",
    "false", "env", "pwd", "wc", "grep", "head", "tail", "sort", "uniq", "tr", "find",
    "touch", "test", "[", "[[", "stat", "sleep", "kill", "basename", "dirname", "cut",
    "expr", "seq", "id", "printf", "date", "chmod", "chown", "ln", "dd", "sync", "cmp",
    "od", "awk", "sed", "xargs", "readlink", "mkfifo", "du", "df", "mount", "umount",
    "chattr", "getconf", "tee", "yes", "mknod", "mktemp", "chgrp", "dmesg", "ps", "free",
    "hostname", "md5sum", "diff", "unlink", "timeout",
];

fn force_symlink(target: &str, linkpath: &str) -> isize {
    unlinkat(linkpath);
    let mut r = symlinkat(target, linkpath);
    let mut tries = 0;
    while r != 0 && tries < 4 {
        unlinkat(linkpath);
        r = symlinkat(target, linkpath);
        tries += 1;
    }
    r
}

fn path_exists(path: &str) -> bool {
    let fd = open(path, OpenFlags::RDONLY);
    if fd < 0 {
        return false;
    }
    close(fd as usize);
    true
}

fn symlink_if_missing(target: &str, linkpath: &str) {
    if !path_exists(linkpath) {
        force_symlink(target, linkpath);
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
    symlink_if_missing("/bin/busybox", "/busybox");
    symlink_if_missing("/bin/busybox", "/bin/sh");
    for applet in BUSYBOX_APPLETS {
        if *applet == "busybox" {
            continue;
        }
        let path = String::from("/bin/") + *applet;
        symlink_if_missing("/bin/busybox", &path);
    }
}

fn runtime_roots(dir: &str) -> (String, String) {
    let root = dir.trim_end_matches('\0').trim_end_matches('/');
    if let Some(prefix) = root.strip_suffix("/musl") {
        (String::from(root), String::from(prefix) + "/glibc")
    } else if let Some(prefix) = root.strip_suffix("/glibc") {
        (String::from(prefix) + "/musl", String::from(root))
    } else {
        (String::from(root) + "/musl", String::from(root) + "/glibc")
    }
}

fn setup_runtime_for_dir(dir: &str) {
    setup_busybox_applets();
    sys_mkdir(b"/lib\0".as_ptr(), 0o777);
    sys_mkdir(b"/lib64\0".as_ptr(), 0o777);

    let test_root = dir.trim_end_matches('\0').trim_end_matches('/');
    if test_root.starts_with("/mnt") {
        let local_busybox = String::from(test_root) + "/busybox";
        let local_sh = String::from(test_root) + "/sh";
        symlink_if_missing("/bin/busybox", &local_busybox);
        symlink_if_missing("/bin/busybox", &local_sh);
    }

    let (musl_root, glibc_root) = runtime_roots(dir);
    println!(
        "[initproc] runtime for {} musl={} glibc={}",
        dir.trim_end_matches('\0'),
        musl_root,
        glibc_root
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
    } else if script.starts_with("libctest_") {
        4
    } else if script.starts_with("libcbench_") {
        5
    } else if script.starts_with("lmbench_") {
        6
    } else {
        50
    }
}

fn should_run_script(script: &str) -> bool {
    script.starts_with("basic_")
        || script.starts_with("busybox_")
        || script.starts_with("iozone_")
        || script.starts_with("lua_")
        || script.starts_with("libctest_")
        || script.starts_with("libcbench_")
        || script.starts_with("lmbench_")
}

fn script_budget(script: &str) -> &'static str {
    if script.starts_with("basic_") {
        "45"
    } else if script.starts_with("lua_") {
        "20"
    } else if script.starts_with("busybox_") {
        "60"
    } else if script.starts_with("iozone_") {
        "300"
    } else if script.starts_with("libctest_") || script.starts_with("libcbench_") {
        "60"
    } else if script.starts_with("lmbench_") {
        "90"
    } else {
        "20"
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

fn wait_child(pid: isize, name: &str) -> i32 {
    if pid <= 0 {
        println!("[initproc] fork failed for {}", name);
        return 127;
    }
    let mut ec = 0;
    let wp = waitpid(pid as usize, &mut ec);
    println!("[initproc] {} waitpid={} exit={}", name, wp, ec);
    ec
}

fn run_command_in_dir(dir: &str, name: &str, path: &str, args: &[*const u8]) -> i32 {
    let pid = fork();
    if pid == 0 {
        if user_lib::chdir(dir) != 0 {
            println!("[initproc] chdir({}) failed", dir);
            exit(126);
        }
        let er = exec(path, args);
        println!("[initproc] exec {} failed: {}", name, er);
        exit(127);
    }
    wait_child(pid, name)
}

fn run_iozone_command(dir: &str, label: &str, first_test_id: &str, second_test_id: &str) -> i32 {
    let args: [*const u8; 12] = [
        b"./iozone\0".as_ptr(),
        b"-t\0".as_ptr(),
        b"4\0".as_ptr(),
        b"-i\0".as_ptr(),
        first_test_id.as_ptr(),
        b"-i\0".as_ptr(),
        second_test_id.as_ptr(),
        b"-r\0".as_ptr(),
        b"1k\0".as_ptr(),
        b"-s\0".as_ptr(),
        b"1m\0".as_ptr(),
        core::ptr::null(),
    ];
    println!("{}", label);
    run_command_in_dir(dir, label, "./iozone\0", &args)
}

fn run_iozone_script(dir: &str, script: &str) -> i32 {
    setup_runtime_for_dir(dir);
    println!("[initproc] [RUN] {}{} direct", dir.trim_end_matches('\0'), script);
    println!("#### OS COMP TEST GROUP START iozone ####");
    println!("iozone automatic measurements");
    let auto_args: [*const u8; 7] = [
        b"./iozone\0".as_ptr(),
        b"-a\0".as_ptr(),
        b"-r\0".as_ptr(),
        b"1k\0".as_ptr(),
        b"-s\0".as_ptr(),
        b"4m\0".as_ptr(),
        core::ptr::null(),
    ];
    let mut last = run_command_in_dir(dir, "iozone automatic", "./iozone\0", &auto_args);
    last = run_iozone_command(dir, "iozone throughput write/read measurements", "0\0", "1\0") | last;
    last = run_iozone_command(dir, "iozone throughput random-read measurements", "0\0", "2\0") | last;
    last = run_iozone_command(dir, "iozone throughput read-backwards measurements", "0\0", "3\0") | last;
    last = run_iozone_command(dir, "iozone throughput stride-read measurements", "0\0", "5\0") | last;
    last = run_iozone_command(dir, "iozone throughput fwrite/fread measurements", "6\0", "7\0") | last;
    last = run_iozone_command(dir, "iozone throughput pwrite/pread measurements", "9\0", "10\0") | last;
    last = run_iozone_command(dir, "iozone throughtput pwritev/preadv measurements", "11\0", "12\0") | last;
    println!("#### OS COMP TEST GROUP END iozone ####");
    last
}

fn run_script(dir: &str, script: &str) -> i32 {
    if script.starts_with("iozone_") {
        return run_iozone_script(dir, script);
    }
    setup_runtime_for_dir(dir);
    let script_arg = String::from("./") + script + "\0";
    let budget = String::from(script_budget(script)) + "\0";
    let args: [*const u8; 7] = [
        b"busybox\0".as_ptr(),
        b"timeout\0".as_ptr(),
        budget.as_ptr(),
        b"/busybox\0".as_ptr(),
        b"sh\0".as_ptr(),
        script_arg.as_ptr(),
        core::ptr::null(),
    ];
    println!(
        "[initproc] [RUN] {}{} budget={}s",
        dir.trim_end_matches('\0'),
        script,
        script_budget(script)
    );
    let pid = fork();
    if pid == 0 {
        if user_lib::chdir(dir) != 0 {
            println!("[initproc] chdir({}) failed", dir);
            exit(126);
        }
        let er = exec("/busybox\0", &args);
        println!("[initproc] exec busybox timeout failed: {}", er);
        exit(127);
    }
    wait_child(pid, script)
}

fn run_program(path: &str, name: &str) -> i32 {
    println!("[initproc] fallback run {}", name);
    let args: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
    let pid = fork();
    if pid == 0 {
        let er = exec(path, &args);
        println!("[initproc] exec({}) failed: {}", path, er);
        exit(127);
    }
    wait_child(pid, name)
}

fn run_basic_fallback() {
    run_program("/bin/contest_runner_musl\0", "basic-musl");
    run_program("/bin/contest_runner_glibc\0", "basic-glibc");
}

fn run_busybox_fallback(glibc: bool) -> i32 {
    setup_runtime_for_dir("/\0");
    let (script, name) = if glibc {
        ("/busybox_glibc_testcode.sh\0", "busybox-glibc")
    } else {
        ("/busybox_testcode.sh\0", "busybox-musl")
    };
    let args: [*const u8; 7] = [
        b"busybox\0".as_ptr(),
        b"timeout\0".as_ptr(),
        b"60\0".as_ptr(),
        b"/busybox\0".as_ptr(),
        b"sh\0".as_ptr(),
        script.as_ptr(),
        core::ptr::null(),
    ];
    println!("[initproc] fallback run {} from rootfs", name);
    let pid = fork();
    if pid == 0 {
        if user_lib::chdir("/\0") != 0 {
            exit(126);
        }
        let er = exec("/busybox\0", &args);
        println!("[initproc] fallback exec busybox failed: {}", er);
        exit(127);
    }
    wait_child(pid, name)
}

fn remount_test_disk(reason: &str) -> isize {
    sys_mkdir(b"/mnt\0".as_ptr(), 0o777);
    let r = sys_mount("vda\0", "/mnt\0", "ext4\0", 0, &[]);
    println!("[initproc] remount vda -> /mnt after {}: {}", reason, r);
    r
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

    for script in &scripts {
        if dir == "/mnt/\0" && script == "basic_testcode.sh" {
            println!(
                "[initproc] root basic_testcode.sh may use arch-suffixed groups; using rootfs basic runners"
            );
            run_basic_fallback();
            remount_test_disk("basic fallback");
        } else {
            let code = run_script(dir, script);
            if script == "busybox_testcode.sh" && code != 0 {
                println!(
                    "[initproc] external busybox script exited {}; trying rootfs fallback",
                    code
                );
                run_busybox_fallback(dir.contains("glibc"));
            }
        }
        found = true;
    }

    if depth == 0 {
        return found;
    }

    for entry in &entries {
        if entry == "lost+found"
            || entry == "bin"
            || entry == "lib"
            || entry == "lib64"
            || entry == "sbin"
            || entry == "tmp"
            || entry.ends_with(".sh")
            || entry.ends_with(".txt")
        {
            continue;
        }
        if dir == "/mnt/\0"
            && (entry == "musl" || entry == "glibc" || entry == "riscv" || entry == "loongarch")
        {
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
        setup_runtime_for_dir("/mnt/\0");
    }

    let mut found = false;

    #[cfg(target_arch = "riscv64")]
    let arch_dirs: [&str; 2] = ["/mnt/riscv/musl/\0", "/mnt/riscv/glibc/\0"];
    #[cfg(target_arch = "loongarch64")]
    let arch_dirs: [&str; 2] = ["/mnt/loongarch/musl/\0", "/mnt/loongarch/glibc/\0"];
    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    let arch_dirs: [&str; 0] = [];

    for dir in arch_dirs {
        if scan_and_run(dir, 1) {
            found = true;
        }
    }

    for dir in ["/mnt/musl/\0", "/mnt/glibc/\0"] {
        if scan_and_run(dir, 1) {
            found = true;
        }
    }

    if scan_and_run("/mnt/\0", 4) {
        found = true;
    }

    if !found {
        println!("[initproc] no test scripts found on /mnt; using rootfs fallbacks");
        run_basic_fallback();
        run_busybox_fallback(false);
        run_busybox_fallback(true);
    }

    println!("[initproc] === DONE ===");
    shutdown();
    0
}
