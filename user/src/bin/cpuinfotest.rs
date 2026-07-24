#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{close, open, read, sys_sched_getaffinity, OpenFlags};

const CPUSET_BYTES: usize = core::mem::size_of::<usize>();

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[no_mangle]
pub fn main() -> i32 {
    // The user library forwards this byte slice directly to the Linux-style
    // pathname syscall, so retain the terminating NUL explicitly.
    let fd = open("/proc/cpuinfo\0", OpenFlags::RDONLY);
    assert!(fd >= 0);
    let mut content = [0u8; 1024];
    let len = read(fd as usize, &mut content);
    assert!(len > 0);
    assert_eq!(close(fd as usize), 0);
    let content = &content[..len as usize];

    let mut affinity = [0u8; CPUSET_BYTES];
    assert_eq!(
        sys_sched_getaffinity(0, &mut affinity),
        CPUSET_BYTES as isize
    );
    let online = affinity[0];
    assert!(contains(content, b"processor\t: 0\nhart\t\t: 0"));
    for hart in 1..4 {
        let present = contains(
            content,
            match hart {
                1 => b"processor\t: 1\nhart\t\t: 1",
                2 => b"processor\t: 2\nhart\t\t: 2",
                3 => b"processor\t: 3\nhart\t\t: 3",
                _ => unreachable!(),
            },
        );
        assert_eq!(present, online & (1 << hart) != 0);
    }
    println!("cpuinfotest passed: online mask {:#x}", online);
    0
}
