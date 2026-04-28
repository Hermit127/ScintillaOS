#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{get_time, sys_nanosleep, TimeSpec};


#[no_mangle]
pub fn main() -> i32 {
    println!("========== START test_nanosleep ==========");

    // test 1: sleep 100ms
    let start = get_time();
    let req = TimeSpec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };
    let ret = sys_nanosleep(&req, core::ptr::null_mut());
    let end = get_time();
    let delta = (end - start) as usize;
    println!(
        "test1: nanosleep 100ms, ret={}, delta={}ms",
        ret, delta
    );
    assert!(ret == 0);
    assert!(delta >= 80 && delta <= 200, "delta={} out of range", delta);

    // test 2: sleep 1.5s
    let start = get_time();
    let req = TimeSpec {
        tv_sec: 1,
        tv_nsec: 500_000_000,
    };
    let ret = sys_nanosleep(&req, core::ptr::null_mut());
    let end = get_time();
    let delta = (end - start) as usize;
    println!(
        "test2: nanosleep 1.5s, ret={}, delta={}ms",
        ret, delta
    );
    assert!(ret == 0);
    assert!(delta >= 1400 && delta <= 1800, "delta={} out of range", delta);

    // test 3: sleep with rem pointer (verify rem is cleared)
    let mut rem = TimeSpec {
        tv_sec: 999,
        tv_nsec: 999_999_999,
    };
    let req = TimeSpec {
        tv_sec: 0,
        tv_nsec: 50_000_000,
    };
    let ret = sys_nanosleep(&req, &mut rem);
    println!(
        "test3: nanosleep with rem, ret={}, rem.tv_sec={}, rem.tv_nsec={}",
        ret, rem.tv_sec, rem.tv_nsec
    );
    assert!(ret == 0);
    assert!(rem.tv_sec == 0 && rem.tv_nsec == 0);

    println!("========== END test_nanosleep ==========");
    0
}
