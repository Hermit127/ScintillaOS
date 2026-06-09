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

fn run_script(variant: &str, mount: &str, script: &str, bb: &str) {
    let dir = String::from(mount) + "/" + variant;
    let lib_path = String::from("LD_LIBRARY_PATH=") + &dir + "/lib:/lib\0";
    let envs: [*const u8; 2] = [lib_path.as_ptr(), core::ptr::null()];
    let script_nul = String::from(script) + "\0";
    let args: [*const u8; 3] = [b"sh\0".as_ptr(), script_nul.as_ptr(), core::ptr::null()];
    let pid = fork();
    if pid == 0 { chdir(&dir); execve(bb, &args, &envs); exit(127); }
    else if pid > 0 { let mut ec = 0; waitpid(pid as usize, &mut ec); }
}

fn skip_group(name: &str) -> bool {
    matches!(name, "libctest" | "libcbench" | "unixbench" | "cyclictest" | "netperf" | "iperf" | "ltp")
}

fn try_mount(mp: &str, dev: &str) -> bool {
    sys_mkdir(mp.as_ptr(), 0o777);
    sys_mount(dev, mp, "ext4", 0, &[]) == 0
}

fn scan_variants(mount: &str) -> Vec<(String, String, String)> {
    // Returns (variant, dir_path, script_name)
    let mut results = Vec::new();
    for variant in ["musl", "glibc"] {
        let dir_nul = String::from(mount) + "/" + variant + "\0";
        for f in list_dir(&dir_nul) {
            if f.ends_with("_testcode.sh") && !skip_group(f.trim_end_matches("_testcode.sh")) {
                results.push((String::from(variant), String::from(mount), f));
            }
        }
    }
    // Fallback: scan mount root for flat _testcode.sh
    let root_nul = String::from(mount) + "/\0";
    for f in list_dir(&root_nul) {
        if f.ends_with("_testcode.sh") {
            let group = f.trim_end_matches("_testcode.sh");
            if !skip_group(group) {
                results.push((String::from("root"), String::from(mount), f));
            }
        }
    }
    results
}

fn run_all(scripts: &[(String, String, String)]) {
    for (variant, mount, script) in scripts {
        let bb = if variant == "glibc" { "/busybox-glibc\0" } else { "/busybox\0" };
        let dir = if variant == "root" { mount.clone() } else { String::from(mount) + "/" + variant };
        println!("[RUN] {} ({})", script.trim_end_matches("_testcode.sh"), variant);
        run_script(variant, mount, script, bb);
    }
}

#[no_mangle]
fn main() -> i32 {
    let mut scripts = Vec::new();

    // Try mounting vda (bus 0)
    if try_mount("/mnt\0", "vda\0") {
        scripts = scan_variants("/mnt");
    }
    // If no tests found, try vdb (bus 1)
    if scripts.is_empty() && try_mount("/mnt2\0", "vdb\0") {
        scripts = scan_variants("/mnt2");
    }
    // If still no tests, fallback to rootfs scripts
    if scripts.is_empty() {
        println!("[initproc] no contest disk, using rootfs fallback");
        scripts = scan_variants("/");
    }

    println!("[initproc] found {} test scripts", scripts.len());
    run_all(&scripts);
    shutdown();
    0
}
