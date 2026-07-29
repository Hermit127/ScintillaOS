#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::sync::atomic::{AtomicUsize, Ordering};
use user_lib::{
    exit, sys_clone, sys_getcpu, sys_sched_getaffinity, sys_waitpid, sys_yield,
};

const CLONE_VM: usize = 0x100;
const SIGCHLD: usize = 17;
const CPUSET_BYTES: usize = core::mem::size_of::<usize>();
const WORKERS: usize = 6;
const ITERATIONS: usize = 100_000;
const CHILD_STACK_SIZE: usize = 16 * 1024;

#[repr(align(16))]
struct ChildStack([u8; CHILD_STACK_SIZE]);

// The workers deliberately use CLONE_VM and independent user stacks.  A
// fork-only test gives each child a private COW copy of its progress flags,
// and short-lived children can finish one by one on the first idle hart.  The
// shared start barrier below forces several Ready/Running tasks to exist at
// once, exercising all available scheduler queues.
static mut CHILD_STACKS: [ChildStack; WORKERS] =
    [const { ChildStack([0; CHILD_STACK_SIZE]) }; WORKERS];
static READY: AtomicUsize = AtomicUsize::new(0);
static START: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_CPUS: AtomicUsize = AtomicUsize::new(0);
static WORK: AtomicUsize = AtomicUsize::new(0);

fn current_cpu() -> usize {
    let mut cpu = u32::MAX;
    assert_eq!(sys_getcpu(&mut cpu, None), 0);
    cpu as usize
}

fn cpu_work() -> usize {
    let mut seen_cpus = 1usize << current_cpu();
    for value in 0..ITERATIONS {
        WORK.fetch_add(value.wrapping_mul(3).wrapping_add(1), Ordering::Relaxed);
        if value % 4096 == 0 {
            let _ = sys_yield();
            seen_cpus |= 1usize << current_cpu();
        }
    }
    seen_cpus
}

fn worker() -> ! {
    READY.fetch_add(1, Ordering::Release);
    while START.load(Ordering::Acquire) == 0 {
        core::hint::spin_loop();
    }
    let seen_cpus = cpu_work();
    OBSERVED_CPUS.fetch_or(seen_cpus, Ordering::AcqRel);
    println!(
        "smpstress worker pid={} visited={:#x}",
        user_lib::getpid(), seen_cpus
    );
    exit(0);
}

#[no_mangle]
pub fn main() -> i32 {
    let mut online = [0u8; CPUSET_BYTES];
    assert_eq!(
        sys_sched_getaffinity(0, &mut online),
        CPUSET_BYTES as isize
    );
    let online_mask = online[0] as usize;
    assert_ne!(online_mask, 0);

    let mut children = [0isize; WORKERS];
    for (index, pid_slot) in children.iter_mut().enumerate() {
        let child_stack = unsafe {
            (core::ptr::addr_of_mut!(CHILD_STACKS[index]) as *mut u8)
                .add(core::mem::size_of::<ChildStack>()) as usize
        };
        let pid = sys_clone(CLONE_VM | SIGCHLD, child_stack, 0, 0, 0);
        assert!(pid >= 0);
        if pid == 0 {
            worker();
        }
        *pid_slot = pid;
    }

    let mut all_ready = false;
    for _ in 0..200_000 {
        if READY.load(Ordering::Acquire) == WORKERS {
            all_ready = true;
            break;
        }
        let _ = sys_yield();
    }
    // Always release child workers, even after a timeout, so a failure never
    // leaves a shell command spinning forever.
    START.store(1, Ordering::Release);

    let parent_seen = cpu_work();
    OBSERVED_CPUS.fetch_or(parent_seen, Ordering::AcqRel);

    let mut children_ok = true;
    for pid in children {
        let mut status = -1;
        if sys_waitpid(pid, &mut status) != pid || status != 0 {
            children_ok = false;
        }
    }

    let observed_mask = OBSERVED_CPUS.load(Ordering::Acquire);
    if !all_ready || !children_ok || observed_mask & online_mask != online_mask {
        println!(
            "smpstress failed: ready={}/{} online={:#x}, observed={:#x}",
            READY.load(Ordering::Acquire),
            WORKERS,
            online_mask,
            observed_mask
        );
        return 1;
    }
    println!(
        "smpstress passed: online={:#x}, observed={:#x}",
        online_mask, observed_mask
    );
    0
}
