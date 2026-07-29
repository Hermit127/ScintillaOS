#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{sys_mmap, sys_mprotect, sys_munmap};

const PAGE_SIZE: usize = 4096;
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_PRIVATE: usize = 2;
const MAP_ANONYMOUS: usize = 0x20;

#[no_mangle]
pub fn main() -> i32 {
    let addr = sys_mmap(
        0,
        PAGE_SIZE,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    assert!(addr > 0);
    let addr = addr as usize;

    // The first write demands the lazy page.  The read-only transition below
    // changes the existing PTE and is the mprotect/TLB regression target.
    unsafe { (addr as *mut u8).write_volatile(0x5a) };
    assert_eq!(sys_mprotect(addr, PAGE_SIZE, PROT_READ), 0);
    assert_eq!(unsafe { (addr as *const u8).read_volatile() }, 0x5a);

    assert_eq!(sys_mprotect(addr, PAGE_SIZE, PROT_READ | PROT_WRITE), 0);
    unsafe { (addr as *mut u8).write_volatile(0xa5) };
    assert_eq!(unsafe { (addr as *const u8).read_volatile() }, 0xa5);
    assert_eq!(sys_munmap(addr, PAGE_SIZE), 0);
    println!("mprotecttest passed!");
    0
}
