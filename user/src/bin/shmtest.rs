#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{sys_shmat, sys_shmdt, sys_shmget};

#[no_mangle]
pub fn main() -> i32 {
    // IPC_PRIVATE always creates a fresh segment.  Two attaches in this task
    // must refer to the same physical frames, not two copied mappings.
    let shmid = sys_shmget(0, 4096, 0);
    assert!(shmid > 0);
    let first = sys_shmat(shmid as usize, 0, 0);
    let second = sys_shmat(shmid as usize, 0, 0);
    assert!(first > 0 && second > 0 && first != second);

    unsafe {
        let first_word = first as *mut u32;
        let second_word = second as *mut u32;
        first_word.write_volatile(0x1234_5678);
        assert_eq!(second_word.read_volatile(), 0x1234_5678);
        second_word.write_volatile(0x8765_4321);
        assert_eq!(first_word.read_volatile(), 0x8765_4321);
    }

    assert_eq!(sys_shmdt(first as usize), 0);
    assert_eq!(sys_shmdt(second as usize), 0);
    println!("shmtest passed!");
    0
}
