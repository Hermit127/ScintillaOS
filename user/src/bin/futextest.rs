#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{get_time, sys_futex, TimeSpec};

const FUTEX_WAIT_PRIVATE: usize = 0x80;
const FUTEX_WAKE_PRIVATE: usize = 0x81;

#[no_mangle]
pub fn main() -> i32 {
    let mut word = 1u32;

    // A mismatched expected value must not publish a waiter.
    assert_eq!(
        sys_futex(&mut word, FUTEX_WAIT_PRIVATE, 0, 0, core::ptr::null_mut(), 0),
        -11
    );
    println!("futextest: mismatch path passed");

    // This exercises futex registration, the scheduler's Blocking -> Blocked
    // handoff, and timer-driven wake-up.  Keep the interval generous enough
    // for QEMU's timer granularity while still making the test quick.
    let timeout = TimeSpec {
        tv_sec: 0,
        tv_nsec: 20_000_000,
    };
    let start = get_time();
    println!("futextest: entering timed wait");
    assert_eq!(
        sys_futex(
            &mut word,
            FUTEX_WAIT_PRIVATE,
            1,
            &timeout as *const TimeSpec as usize,
            core::ptr::null_mut(),
            0,
        ),
        -110
    );
    assert!(get_time() - start >= 10);

    // The resumed waiter must have removed its table entry.
    assert_eq!(
        sys_futex(
            &mut word,
            FUTEX_WAKE_PRIVATE,
            1,
            0,
            core::ptr::null_mut(),
            0,
        ),
        0
    );
    println!("futextest passed!");
    0
}
