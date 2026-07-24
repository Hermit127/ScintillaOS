#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{sys_getcpu, sys_sched_getaffinity, sys_sched_setaffinity, sys_yield};

const CPUSET_BYTES: usize = core::mem::size_of::<usize>();

fn current_cpu() -> u32 {
    let mut cpu = u32::MAX;
    let mut node = u32::MAX;
    assert_eq!(sys_getcpu(&mut cpu, Some(&mut node)), 0);
    assert_eq!(node, 0);
    cpu
}

fn cpuset(mask: usize) -> [u8; CPUSET_BYTES] {
    let mut bytes = [0u8; CPUSET_BYTES];
    for index in 0..CPUSET_BYTES {
        bytes[index] = (mask >> (index * 8)) as u8;
    }
    bytes
}

fn cpuset_mask(bytes: &[u8; CPUSET_BYTES]) -> usize {
    let mut mask = 0usize;
    for index in 0..CPUSET_BYTES {
        mask |= (bytes[index] as usize) << (index * 8);
    }
    mask
}

fn wait_until_cpu(expected: u32) {
    // A yield completes the task's Running -> Ready -> Running handoff. The
    // scheduler must then choose a CPU contained in the mask set below.
    for _ in 0..64 {
        let _ = sys_yield();
        if current_cpu() == expected {
            return;
        }
    }
    panic!("task did not migrate to requested CPU");
}

#[no_mangle]
pub fn main() -> i32 {
    let mut initial = [0u8; CPUSET_BYTES];
    assert_eq!(
        sys_sched_getaffinity(0, &mut initial),
        CPUSET_BYTES as isize
    );
    let online = cpuset_mask(&initial);
    assert_ne!(online, 0);
    assert_eq!(sys_sched_setaffinity(0, &[0]), -22);
    assert_eq!(sys_sched_getaffinity(usize::MAX, &mut initial), -3);
    if CPUSET_BYTES > 1 {
        let mut too_short = [0u8; 1];
        assert_eq!(sys_sched_getaffinity(0, &mut too_short), -22);
    }

    // Pin to every online hart in turn. This detects a CPU-local scheduler
    // error that a CPU0-only test or a single migration cannot expose.
    let mut observed = [0u8; CPUSET_BYTES];
    let mut tested_cpus = 0usize;
    let mut two_cpu_mask = 0usize;
    for cpu in 0..(CPUSET_BYTES * 8) {
        let mask = 1usize << cpu;
        if online & mask == 0 {
            continue;
        }
        let one_cpu = cpuset(mask);
        assert_eq!(sys_sched_setaffinity(0, &one_cpu), 0);
        assert_eq!(
            sys_sched_getaffinity(0, &mut observed),
            CPUSET_BYTES as isize
        );
        assert_eq!(observed, one_cpu);
        wait_until_cpu(cpu as u32);
        if tested_cpus < 2 {
            two_cpu_mask |= mask;
        }
        tested_cpus += 1;
    }
    assert_ne!(tested_cpus, 0);

    // A multi-CPU mask is retained exactly, rather than being silently
    // reduced to the hart on which the caller happens to be running.
    if tested_cpus >= 2 {
        let two_cpus = cpuset(two_cpu_mask);
        assert_eq!(sys_sched_setaffinity(0, &two_cpus), 0);
        assert_eq!(
            sys_sched_getaffinity(0, &mut observed),
            CPUSET_BYTES as isize
        );
        assert_eq!(observed, two_cpus);
    }

    // The four-hart kernel must reject an offline CPU bit. Searching the
    // returned online mask keeps this image valid for the one-hart baseline.
    for cpu in 0..(CPUSET_BYTES * 8) {
        let mask = 1usize << cpu;
        if online & mask == 0 {
            let offline = cpuset(mask);
            assert_eq!(sys_sched_setaffinity(0, &offline), -22);
            break;
        }
    }

    let all_online = cpuset(online);
    assert_eq!(sys_sched_setaffinity(0, &all_online), 0);
    assert_eq!(
        sys_sched_getaffinity(0, &mut observed),
        CPUSET_BYTES as isize
    );
    assert_eq!(observed, all_online);
    println!(
        "affinitytest passed: online={:#x}, tested={} cpu(s), current={}",
        online,
        tested_cpus,
        current_cpu()
    );
    0
}
