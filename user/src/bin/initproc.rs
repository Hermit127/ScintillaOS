#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{
    chroot, close, execve, exit, fork, get_time, getdents64, kill, open, shutdown, sleep,
    symlinkat, sys_mkdir, sys_mount, sys_waitpid_options, unlinkat, waitpid, waitpid_nb, OpenFlags,
};

const BUSYBOX_APPLETS: [&str; 68] = [
    "busybox", "sh", "ash", "ls", "cat", "echo", "mkdir", "rm", "rmdir", "mv", "cp", "true",
    "false", "env", "pwd", "wc", "grep", "head", "tail", "sort", "uniq", "tr", "find", "touch",
    "test", "[", "[[", "stat", "sleep", "kill", "basename", "dirname", "cut", "expr", "seq", "id",
    "printf", "date", "chmod", "chown", "ln", "dd", "sync", "cmp", "od", "awk", "sed", "xargs",
    "readlink", "mkfifo", "du", "df", "mount", "umount", "chattr", "getconf", "tee", "yes",
    "mknod", "mktemp", "chgrp", "dmesg", "ps", "free", "hostname", "md5sum", "diff", "unlink",
];

const KNOWN_TEST_SCRIPTS: [&str; 7] = [
    "basic_testcode.sh",
    "busybox_testcode.sh",
    "lua_testcode.sh",
    "iozone_testcode.sh",
    "lmbench_testcode.sh",
    "iperf_testcode.sh",
    "libctest_testcode.sh",
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

fn nul_terminated_path(path: &str) -> String {
    String::from(path.trim_end_matches('\0')) + "\0"
}

fn path_exists(path: &str) -> bool {
    let p = nul_terminated_path(path);
    let fd = open(&p, OpenFlags::RDONLY);
    if fd >= 0 {
        close(fd as usize);
        true
    } else {
        false
    }
}

fn ensure_symlink(target: &str, linkpath: &str) -> isize {
    if path_exists(linkpath) {
        0
    } else {
        force_symlink(target, linkpath)
    }
}

fn mkdir_path(path: &str) {
    let p = String::from(path.trim_end_matches('\0')) + "\0";
    sys_mkdir(p.as_ptr(), 0o777);
}

fn bind_library(chroot_root: &str, src_root: &str, rel_path: &str, dst: &str) {
    let chroot_root = chroot_root.trim_end_matches('\0').trim_end_matches('/');
    let src_root = src_root.trim_end_matches('\0').trim_end_matches('/');
    let src = String::from(src_root) + rel_path;
    force_symlink(&src, dst);
    let dst64 = String::from("/lib64/") + dst.trim_start_matches("/lib/");
    force_symlink(&src, &dst64);
    let inside_src = if let Some(suffix) = src_root.strip_prefix(chroot_root) {
        String::from(suffix) + rel_path
    } else {
        src.clone()
    };
    let local_dst = String::from(chroot_root) + dst;
    force_symlink(&inside_src, &local_dst);
    let local_dst64 = String::from(chroot_root) + "/lib64/" + dst.trim_start_matches("/lib/");
    force_symlink(&inside_src, &local_dst64);
}

fn setup_runtime_dirs(root: &str) {
    let root = root.trim_end_matches('\0').trim_end_matches('/');
    mkdir_path(&(String::from(root) + "/bin"));
    mkdir_path(&(String::from(root) + "/usr"));
    mkdir_path(&(String::from(root) + "/usr/bin"));
    mkdir_path(&(String::from(root) + "/tmp"));
    mkdir_path(&(String::from(root) + "/mnt"));
    mkdir_path(&(String::from(root) + "/lib"));
    mkdir_path(&(String::from(root) + "/lib64"));
}

fn setup_lmbench_wrapper_paths(chroot_root: &str, chroot_cwd: &str) {
    let chroot_root = chroot_root.trim_end_matches('\0').trim_end_matches('/');
    let chroot_cwd = chroot_cwd.trim_end_matches('\0').trim_end_matches('/');
    let prefixes = [
        "/code/lmbench_src/bin/build",
        "/coursegrader/submit/testsuits-for-oskernel/lmbench_src/bin/build",
    ];
    for prefix in prefixes {
        let mut path = String::from(chroot_root);
        for part in prefix.trim_matches('/').split('/') {
            path.push('/');
            path.push_str(part);
            mkdir_path(&path);
        }
        let target = String::from(chroot_cwd) + "/lmbench_all";
        let link = path + "/lmbench_all\0";
        force_symlink(&target, &link);
    }
}

fn setup_busybox_applets_at(root: &str) {
    let root = root.trim_end_matches('\0').trim_end_matches('/');
    setup_runtime_dirs(root);
    ensure_symlink("/busybox", &(String::from(root) + "/bin/busybox\0"));
    ensure_symlink("/busybox", &(String::from(root) + "/bin/sh\0"));
    for applet in BUSYBOX_APPLETS {
        let path = String::from(root) + "/bin/" + applet + "\0";
        ensure_symlink("/busybox", &path);
    }
}

fn setup_busybox_applets() {
    sys_mkdir(b"/bin\0".as_ptr(), 0o777);
    ensure_symlink("/busybox", "/bin/busybox\0");
    ensure_symlink("/busybox", "/bin/sh\0");
    for applet in BUSYBOX_APPLETS {
        let path = String::from("/bin/") + applet + "\0";
        ensure_symlink("/busybox", &path);
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

fn chroot_root_and_cwd(dir: &str) -> (String, String) {
    let root = dir.trim_end_matches('\0').trim_end_matches('/');
    if let Some(prefix) = root.strip_suffix("/musl") {
        (String::from(prefix), String::from("/musl"))
    } else if let Some(prefix) = root.strip_suffix("/glibc") {
        (String::from(prefix), String::from("/glibc"))
    } else {
        (String::from(root), String::from("/"))
    }
}

fn setup_runtime_for_dir(dir: &str, script: &str) {
    let root = dir.trim_end_matches('\0').trim_end_matches('/');
    let (musl_root, glibc_root) = runtime_roots(root);
    let (chroot_root, chroot_cwd) = chroot_root_and_cwd(root);
    let setup_musl = !root.ends_with("/glibc");
    let setup_glibc = !root.ends_with("/musl");
    println!(
        "[initproc] runtime root {} chroot {} cwd {} musl {} glibc {}",
        root, chroot_root, chroot_cwd, musl_root, glibc_root
    );
    setup_busybox_applets();
    sys_mkdir(b"/lib\0".as_ptr(), 0o777);
    sys_mkdir(b"/lib64\0".as_ptr(), 0o777);
    if setup_musl {
        setup_runtime_dirs(&musl_root);
    }
    if setup_glibc {
        setup_runtime_dirs(&glibc_root);
    }
    setup_runtime_dirs(&chroot_root);
    let busybox = chroot_cwd.clone() + "/busybox";
    force_symlink(&busybox, &(chroot_root.clone() + "/busybox\0"));
    setup_busybox_applets_at(&chroot_root);
    if script.starts_with("lmbench_") {
        setup_lmbench_wrapper_paths(&chroot_root, &chroot_cwd);
    }

    #[cfg(target_arch = "riscv64")]
    {
        if setup_glibc {
            bind_library(
                &chroot_root,
                &glibc_root,
                "/lib/ld-linux-riscv64-lp64d.so.1",
                "/lib/ld-linux-riscv64-lp64d.so.1",
            );
            bind_library(
                &chroot_root,
                &glibc_root,
                "/lib/ld-linux-riscv64-lp64d.so.1",
                "/lib/ld-linux-riscv64-lp64.so.1",
            );
            bind_library(&chroot_root, &glibc_root, "/lib/libc.so", "/lib/libc.so.6");
            bind_library(&chroot_root, &glibc_root, "/lib/libm.so", "/lib/libm.so.6");
        }
        if setup_musl {
            bind_library(
                &chroot_root,
                &musl_root,
                "/lib/libc.so",
                "/lib/ld-musl-riscv64-sf.so.1",
            );
            bind_library(
                &chroot_root,
                &musl_root,
                "/lib/libc.so",
                "/lib/ld-musl-riscv64.so.1",
            );
            bind_library(&chroot_root, &musl_root, "/lib/libc.so", "/lib/libc.so");
        }
    }
    #[cfg(target_arch = "loongarch64")]
    {
        if setup_glibc {
            bind_library(
                &chroot_root,
                &glibc_root,
                "/lib/ld-linux-loongarch-lp64d.so.1",
                "/lib/ld-linux-loongarch-lp64d.so.1",
            );
            bind_library(
                &chroot_root,
                &glibc_root,
                "/lib/libc.so.6",
                "/lib/libc.so.6",
            );
            bind_library(
                &chroot_root,
                &glibc_root,
                "/lib/libm.so.6",
                "/lib/libm.so.6",
            );
        }
        if setup_musl {
            bind_library(
                &chroot_root,
                &musl_root,
                "/lib/libc.so",
                "/lib/ld-musl-loongarch64.so.1",
            );
            bind_library(
                &chroot_root,
                &musl_root,
                "/lib/libc.so",
                "/lib/ld-musl-loongarch64-lp64d.so.1",
            );
            bind_library(
                &chroot_root,
                &musl_root,
                "/lib/libc.so",
                "/lib/ld-musl-loongarch-lp64d.so.1",
            );
            bind_library(&chroot_root, &musl_root, "/lib/libc.so", "/lib/libc.so");
        }
    }
}

fn install_busybox() {
    setup_busybox_applets();
}

fn reap_zombies() {
    let mut exit_code = 0;
    loop {
        let pid = sys_waitpid_options(-1, &mut exit_code as *mut i32, 1);
        if pid <= 0 {
            break;
        }
    }
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
    } else if script.starts_with("lua_") {
        2
    } else if script.starts_with("iozone_") {
        3
    } else if script.starts_with("lmbench_") {
        4
    } else if script.starts_with("iperf_") {
        5
    } else if script.starts_with("libctest_") {
        6
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
        || script.starts_with("iperf_")
        || script.starts_with("libctest_")
}

fn script_budget(script: &str) -> &'static str {
    if script.starts_with("basic_") {
        "90"
    } else if script.starts_with("lua_") {
        "90"
    } else if script.starts_with("busybox_") {
        "180"
    } else if script.starts_with("iozone_") {
        "720"
    } else if script.starts_with("lmbench_") {
        "1800"
    } else if script.starts_with("iperf_") {
        "300"
    } else if script.starts_with("libctest_") {
        "600"
    } else {
        "90"
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

struct TestScript {
    dir: String,
    script: String,
}

fn has_entry(entries: &[String], name: &str) -> bool {
    entries.iter().any(|entry| entry == name)
}

fn script_path(dir: &str, script: &str) -> String {
    let mut path = String::from(dir.trim_end_matches('\0'));
    if !path.ends_with('/') {
        path.push('/');
    }
    path.push_str(script);
    path
}

fn collect_known_scripts(dir: &str, out: &mut Vec<TestScript>) -> usize {
    println!("[initproc] scan known {}", dir.trim_end_matches('\0'));
    let entries = list_dir(dir);
    let mut found = 0usize;
    for script in KNOWN_TEST_SCRIPTS {
        if !entries.iter().any(|entry| entry == script) {
            continue;
        }
        let path = script_path(dir, script);
        if out
            .iter()
            .any(|old| script_path(&old.dir, &old.script) == path)
        {
            continue;
        }
        out.push(TestScript {
            dir: String::from(dir),
            script: String::from(script),
        });
        found += 1;
    }
    println!(
        "[initproc] {} has {} known test scripts",
        dir.trim_end_matches('\0'),
        found
    );
    found
}

fn run_known_dir(dir: &str) -> bool {
    let mut scripts = Vec::new();
    collect_known_scripts(dir, &mut scripts);
    run_collected_scripts(scripts)
}

fn run_existing_standard_dirs() -> bool {
    let root_entries = list_dir("/mnt/\0");
    println!("[initproc] /mnt/ root has {} entries", root_entries.len());
    let mut found = false;

    if has_entry(&root_entries, "musl") {
        found |= run_known_dir("/mnt/musl/\0");
    }

    #[cfg(target_arch = "riscv64")]
    let arch_dir = "riscv";
    #[cfg(target_arch = "loongarch64")]
    let arch_dir = "loongarch";
    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    let arch_dir = "";

    let mut arch_entries = Vec::new();
    if !arch_dir.is_empty() && has_entry(&root_entries, arch_dir) {
        let arch_root = String::from("/mnt/") + arch_dir + "/\0";
        arch_entries = list_dir(&arch_root);
        println!(
            "[initproc] /mnt/{}/ root has {} entries",
            arch_dir,
            arch_entries.len()
        );
        if has_entry(&arch_entries, "musl") {
            found |= run_known_dir(&(String::from("/mnt/") + arch_dir + "/musl/\0"));
        }
    }

    if has_entry(&root_entries, "glibc") {
        found |= run_known_dir("/mnt/glibc/\0");
    }
    if !arch_dir.is_empty() && has_entry(&arch_entries, "glibc") {
        found |= run_known_dir(&(String::from("/mnt/") + arch_dir + "/glibc/\0"));
    }
    found |= run_known_dir("/mnt/\0");

    found
}

fn run_script(dir: &str, script: &str) {
    let (chroot_root, chroot_cwd) = chroot_root_and_cwd(dir);
    let script_arg = String::from("./") + script + "\0";
    let args: [*const u8; 3] = [b"sh\0".as_ptr(), script_arg.as_ptr(), core::ptr::null()];
    let envs: [*const u8; 6] = [
        b"PATH=/bin:/usr/bin:.\0".as_ptr(),
        b"LD_LIBRARY_PATH=/lib:/lib64:.\0".as_ptr(),
        b"HOME=/\0".as_ptr(),
        b"TMPDIR=/tmp\0".as_ptr(),
        b"ENOUGH=50000\0".as_ptr(),
        core::ptr::null(),
    ];
    let pid = fork();
    if pid == 0 {
        let chroot_arg = chroot_root.clone() + "\0";
        let chdir_arg = chroot_cwd.clone() + "\0";
        if chroot(&chroot_arg) != 0 {
            exit(126);
        }
        if user_lib::chdir(&chdir_arg) != 0 {
            exit(126);
        }
        execve("/busybox\0", &args, &envs);
        exit(127);
    } else if pid > 0 {
        let mut ec = 0;
        let budget_ms = script_budget(script).parse::<isize>().unwrap_or(10) * 1000;
        let start = get_time();
        loop {
            let r = waitpid_nb(pid as usize, &mut ec);
            if r == pid {
                println!(
                    "[initproc] [DONE] {}{} exit {}",
                    dir.trim_end_matches('\0'),
                    script,
                    ec
                );
                reap_zombies();
                break;
            }
            if get_time() - start > budget_ms {
                println!(
                    "[initproc] [TIMEOUT] {}{} after {}ms",
                    dir.trim_end_matches('\0'),
                    script,
                    budget_ms
                );
                kill(pid as usize, 9);
                waitpid(pid as usize, &mut ec);
                reap_zombies();
                break;
            }
            sleep(20);
        }
    }
}

fn collect_scripts(dir: &str, depth: usize, out: &mut Vec<TestScript>) {
    let entries = list_dir(dir);
    println!(
        "[initproc] {} has {} entries",
        dir.trim_end_matches('\0'),
        entries.len()
    );

    let mut scripts: Vec<String> = entries
        .iter()
        .filter(|entry| entry.ends_with("_testcode.sh"))
        .filter(|entry| should_run_script(entry))
        .cloned()
        .collect();
    sort_scripts(&mut scripts);
    for entry in &scripts {
        let path = script_path(dir, entry);
        if out
            .iter()
            .any(|old| script_path(&old.dir, &old.script) == path)
        {
            continue;
        }
        out.push(TestScript {
            dir: String::from(dir),
            script: entry.clone(),
        });
    }

    if depth == 0 {
        return;
    }

    for entry in &entries {
        if entry == "lost+found" || entry.ends_with(".sh") || entry.ends_with(".txt") {
            continue;
        }
        let mut next = join_path(dir, entry);
        next.push_str("/\0");
        collect_scripts(&next, depth - 1, out);
    }
}

fn sort_collected_scripts(scripts: &mut Vec<TestScript>) {
    scripts.sort_by(|a, b| {
        let pa = script_priority(&a.script);
        let pb = script_priority(&b.script);
        if pa != pb {
            return pa.cmp(&pb);
        }
        let sa = a.script.as_str();
        let sb = b.script.as_str();
        if sa != sb {
            return sa.cmp(sb);
        }
        a.dir.as_str().cmp(b.dir.as_str())
    });
}

fn run_collected_scripts(mut scripts: Vec<TestScript>) -> bool {
    if scripts.is_empty() {
        return false;
    }
    sort_collected_scripts(&mut scripts);
    for item in scripts {
        reap_zombies();
        let path = script_path(&item.dir, &item.script);
        println!("[initproc] [PREP] {}", path);
        setup_runtime_for_dir(&item.dir, &item.script);
        println!("[initproc] [RUN] {}", path);
        run_script(&item.dir, &item.script);
    }
    true
}

#[no_mangle]
fn main() -> i32 {
    println!("[initproc] === START ===");

    install_busybox();

    sys_mkdir(b"/mnt\0".as_ptr(), 0o777);
    let r = sys_mount("vda\0", "/mnt\0", "ext4\0", 0, &[]);
    println!("[initproc] mount vda -> /mnt: {}", r);

    let mut found = run_existing_standard_dirs();
    if !found {
        let mut scripts = Vec::new();
        collect_scripts("/mnt/\0", 4, &mut scripts);
        found = run_collected_scripts(scripts);
    }
    if !found {
        println!("[initproc] no test scripts found");
    }
    reap_zombies();

    println!("[initproc] === DONE ===");
    shutdown();
    0
}
