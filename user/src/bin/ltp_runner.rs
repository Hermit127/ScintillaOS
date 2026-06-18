#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use user_lib::{
    close, exec, fork, get_time, getdents64, kill, open, read, sleep, waitpid_nb, OpenFlags,
};

const DEFAULT_LTP_DIR: &str = "/ltp/testcases/bin/";
const CASE_TIMEOUT_MS: isize = 30_000;

const SUBSET_LIST: &[&str] = &[
    "close01",
    "close02",
    "open01",
    "open02",
    "read01",
    "read02",
    "write01",
    "write02",
    "lseek01",
    "lseek02",
    "fstat01",
    "fstat02",
    "fstat03",
    "dup01",
    "dup02",
    "dup201",
    "dup202",
    "pipe01",
    "pipe02",
    "brk01",
    "brk02",
    "mmap01",
    "mmap02",
    "munmap01",
    "munmap02",
    "fork01",
    "fork02",
    "fork03",
    "exit_group01",
    "wait01",
    "wait02",
    "getpid01",
    "getpid02",
    "getcwd01",
    "getcwd02",
    "chdir01",
    "chdir04",
    "times01",
    "uname01",
    "uname02",
    "nanosleep01",
    "kill01",
    "kill02",
    "gettimeofday01",
    "gettimeofday02",
    "umask01",
    "mkdirat01",
    "mkdirat02",
    "rename01",
    "rename02",
    "unlink01",
    "unlink02",
    "mprotect01",
    "mprotect02",
    "fcntl01",
    "fcntl02",
    "access01",
];

fn nul_path(path: &str) -> String {
    String::from(path.trim_end_matches('\0')) + "\0"
}

fn case_path(base: &str, name: &str) -> String {
    let mut path = String::from(base.trim_end_matches('\0'));
    if !path.ends_with('/') {
        path.push('/');
    }
    path.push_str(name);
    path.push('\0');
    path
}

fn is_elf(base: &str, name: &str) -> bool {
    let path = case_path(base, name);
    let fd = open(path.as_str(), OpenFlags::RDONLY);
    if fd < 0 {
        return false;
    }
    let mut magic = [0u8; 4];
    let n = read(fd as usize, &mut magic);
    close(fd as usize);
    n == 4 && magic[0] == 0x7f && magic[1] == b'E' && magic[2] == b'L' && magic[3] == b'F'
}

fn run_test(base: &str, name: &str) {
    if !is_elf(base, name) {
        println!("SKIP LTP CASE {} : missing", name);
        return;
    }

    let path = case_path(base, name);
    println!("RUN LTP CASE {}", name);
    let pid = fork();
    if pid == 0 {
        let args: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
        exec(path.as_str(), &args);
        user_lib::exit(-1);
    }
    if pid < 0 {
        println!("FINISH LTP CASE {} : -1", name);
        return;
    }

    let mut exit_code: i32 = 0;
    let start = get_time();
    loop {
        let r = waitpid_nb(pid as usize, &mut exit_code);
        if r == pid {
            let status = exit_code >> 8;
            println!("FINISH LTP CASE {} : {}", name, status);
            return;
        }
        if get_time() - start > CASE_TIMEOUT_MS {
            kill(pid as usize, 9);
            for _ in 0..20 {
                if waitpid_nb(pid as usize, &mut exit_code) == pid {
                    break;
                }
                sleep(10);
            }
            println!("TIMEOUT LTP CASE {} : {}", name, CASE_TIMEOUT_MS);
            return;
        }
        sleep(10);
    }
}

fn is_full_mode() -> bool {
    let fd = open("/ltp_full\0", OpenFlags::RDONLY);
    if fd >= 0 {
        close(fd as usize);
        true
    } else {
        false
    }
}

fn skip_name(name: &str) -> bool {
    name == "." || name == ".." || name.ends_with(".sh") || name.ends_with(".py")
}

fn for_each_dirent(buf: &[u8], n: isize, mut f: impl FnMut(&str)) {
    let n = n as usize;
    let mut pos: usize = 0;
    while pos + 19 <= n {
        let reclen = (buf[pos + 16] as usize) | ((buf[pos + 17] as usize) << 8);
        if reclen < 19 || pos + reclen > n {
            return;
        }
        let name_start = pos + 19;
        let mut name_end = name_start;
        while name_end < pos + reclen && buf[name_end] != 0 {
            name_end += 1;
        }
        if let Ok(name) = core::str::from_utf8(&buf[name_start..name_end]) {
            if !skip_name(name) {
                f(name);
            }
        }
        pos += reclen;
    }
}

fn run_full(base: &str) -> usize {
    let dir = nul_path(base);
    let fd = open(dir.as_str(), OpenFlags::RDONLY | OpenFlags::DIRECTORY);
    if fd < 0 {
        println!("LTP_DIR_NOT_FOUND: {}", base.trim_end_matches('\0'));
        return 0;
    }
    let mut buf = [0u8; 4096];
    let mut total: usize = 0;
    loop {
        let n = getdents64(fd as usize, &mut buf);
        if n <= 0 {
            break;
        }
        for_each_dirent(&buf, n, |name| {
            if is_elf(base, name) {
                run_test(base, name);
                total += 1;
            }
        });
    }
    close(fd as usize);
    total
}

fn run_subset(base: &str) -> usize {
    let mut total = 0usize;
    for test in SUBSET_LIST {
        run_test(base, test);
        total += 1;
    }
    total
}

#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {
    let base = if argc > 1 { argv[1] } else { DEFAULT_LTP_DIR };
    let label = if argc > 2 { argv[2] } else { "ltp-musl" };
    let full = is_full_mode();

    println!("========================================");
    println!("  LTP RUNNER STARTING");
    println!("========================================");
    println!("#### OS COMP TEST GROUP START {} ####", label);
    let total = if full {
        run_full(base)
    } else {
        run_subset(base)
    };
    println!("total LTP cases: {}", total);
    println!("#### OS COMP TEST GROUP END {} ####", label);
    0
}
