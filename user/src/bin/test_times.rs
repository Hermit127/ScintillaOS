// user/src/bin/test_times.rs

#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{sleep, times, yield_, Tms};

#[no_mangle]
pub fn main() -> i32 {
    let mut tms = Tms {
        tms_utime: 0,
        tms_stime: 0,
        tms_cutime: 0,
        tms_cstime: 0,
    };

    println!("[test_times] Testing times() syscall...");

    // 1. 获取初始时间
    let start_tick = times(&mut tms);
    let start_utime = tms.tms_utime;
    let start_stime = tms.tms_stime;
    println!(
        ">>> Start: total_tick={}, utime={}, stime={}",
        start_tick, start_utime, start_stime
    );

    // 2. 模拟用户态密集计算 (纯算力消耗，不陷入内核)
    println!("[test_times] Doing heavy user-space computation...");
    let mut sum: usize = 0;
    // 这里的循环次数可以根据你模拟器的运行速度微调，确保能消耗几毫秒以上的时间
    for i in 0..5_000_000 {
        sum = sum.wrapping_add(i % 10007);
    }
    println!("[test_times] Computation done. (sum={})", sum);

    // 3. 中途获取一次时间，观察 utime 的增长
    let mid_tick = times(&mut tms);
    println!(
        ">>> Mid: total_tick={}, utime={}, stime={}",
        mid_tick, tms.tms_utime, tms.tms_stime
    );

    // 4. 模拟频繁的内核态调用 (频繁陷入内核，消耗 stime)
    println!("[test_times] Doing heavy kernel-space tasks (yield & sleep)...");
    for _ in 0..100 {
        yield_(); // 让出 CPU，触发内核调度器
    }
    sleep(100); // 睡眠 100 毫秒

    // 5. 获取最终时间
    let end_tick = times(&mut tms);
    println!(
        ">>> End: total_tick={}, utime={}, stime={}",
        end_tick, tms.tms_utime, tms.tms_stime
    );

    // 6. 打印统计结果
    println!("\n[test_times] --- Summary ---");
    println!("Total elapsed ticks : {}", end_tick - start_tick);
    println!("User space time     : {}", tms.tms_utime - start_utime);
    println!("Kernel space time   : {}", tms.tms_stime - start_stime);

    // 简单校验
    if tms.tms_utime > start_utime && tms.tms_stime > start_stime {
        println!("[test_times] SUCCESS! Both utime and stime increased.");
    } else {
        println!("[test_times] WARNING: Time did not increase as expected. Check your trap hooks.");
    }

    0
}
