#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

const SYSCALL_CLOCK_GETTIME: usize = 113;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TimeSpec {
    tv_sec: usize,
    tv_nsec: usize,
}

fn clock_gettime(clk_id: usize, tp: *mut TimeSpec) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") clk_id => ret,
            in("x11") tp as usize,
            in("x17") SYSCALL_CLOCK_GETTIME
        );
    }
    ret
}

#[no_mangle]
pub fn main() -> i32 {
    let mut ts = TimeSpec::default();
    let mut failed = false;

    // Test CLOCK_REALTIME
    let ret = clock_gettime(0, &mut ts);
    println!("CLOCK_REALTIME: ret={} sec={} nsec={}", ret, ts.tv_sec, ts.tv_nsec);
    if ret != 0 { failed = true; println!("  FAIL: expected 0"); }
    if ts.tv_sec == 0 { failed = true; println!("  FAIL: tv_sec should be >0"); }

    // Sleep a bit then check time advanced
    let sec_before = ts.tv_sec;
    user_lib::sleep(1100); // sleep ~1.1 seconds
    let ret = clock_gettime(0, &mut ts);
    println!("CLOCK_REALTIME after sleep: ret={} sec={} nsec={}", ret, ts.tv_sec, ts.tv_nsec);
    if ts.tv_sec <= sec_before && ts.tv_nsec == 0 {
        failed = true;
        println!("  FAIL: time should have advanced");
    }

    // Test CLOCK_MONOTONIC
    let ret = clock_gettime(1, &mut ts);
    println!("CLOCK_MONOTONIC: ret={} sec={} nsec={}", ret, ts.tv_sec, ts.tv_nsec);
    if ret != 0 { failed = true; println!("  FAIL: expected 0"); }

    // Test bad clk_id
    let ret = clock_gettime(99, &mut ts);
    println!("CLOCK bad id: ret={}", ret);
    if ret != -22 { failed = true; println!("  FAIL: expected -22 EINVAL"); }

    // Test null pointer
    let ret = clock_gettime(0, core::ptr::null_mut());
    println!("CLOCK null ptr: ret={}", ret);
    if ret != -22 { failed = true; println!("  FAIL: expected -22 EINVAL"); }

    if failed {
        println!("\n!!! SOME TESTS FAILED !!!");
        -1
    } else {
        println!("\n=== CLOCK_GETTIME ALL PASSED ===");
        0
    }
}
#[cfg(target_arch = "riscv64")]
fn main() {}
