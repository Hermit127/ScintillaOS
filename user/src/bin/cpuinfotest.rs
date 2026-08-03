#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use alloc::format;
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
    let online = affinity
        .iter()
        .enumerate()
        .fold(0usize, |mask, (index, byte)| mask | ((*byte as usize) << (index * 8)));
    for hart in 0..usize::BITS as usize {
        let processor = format!("processor\t: {}\n", hart);
        let present = contains(content, processor.as_bytes());
        assert_eq!(present, online & (1usize << hart) != 0);

        #[cfg(target_arch = "riscv64")]
        if present {
            let hart_line = format!("hart\t\t: {}\n", hart);
            assert!(contains(content, hart_line.as_bytes()));
        }
    }
    println!("cpuinfotest passed: online mask {:#x}", online);
    0
}
