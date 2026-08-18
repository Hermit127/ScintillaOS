#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::sync::atomic::{AtomicUsize, Ordering};
use user_lib::{
    sys_clone, sys_exec, sys_exit, sys_getcpu, sys_mmap, sys_munmap, sys_sched_getaffinity,
    sys_sched_setaffinity, sys_waitpid, sys_yield,
};

const CLONE_VM: usize = 0x100;
const SIGCHLD: usize = 17;
const CPUSET_BYTES: usize = core::mem::size_of::<usize>();
// Keep the in-image correctness check short; repeated invocations provide
// the longer stress run without making an interactive QEMU regression flaky.
const ITERATIONS: usize = 4_096;
const PAGE_SIZE: usize = 4096;
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_PRIVATE: usize = 2;
const MAP_FIXED: usize = 0x10;
const MAP_ANONYMOUS: usize = 0x20;

#[repr(align(16))]
struct ChildStack([u8; 16 * 1024]);

static mut CHILD_STACK: ChildStack = ChildStack([0; 16 * 1024]);
static CHILD_STARTED: AtomicUsize = AtomicUsize::new(0);
static CHILD_DONE: AtomicUsize = AtomicUsize::new(0);
static CHILD_ERROR: AtomicUsize = AtomicUsize::new(0);
static CHILD_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
static SHARED_VALUE: AtomicUsize = AtomicUsize::new(0);
static TEST_MAPPING: AtomicUsize = AtomicUsize::new(0);
static TLB_READY: AtomicUsize = AtomicUsize::new(0);
static TLB_REMAP_DONE: AtomicUsize = AtomicUsize::new(0);

fn cpu_mask(cpu: usize) -> [u8; CPUSET_BYTES] {
    let mut mask = [0u8; CPUSET_BYTES];
    mask[cpu / 8] = 1 << (cpu % 8);
    mask
}

fn current_cpu() -> u32 {
    let mut cpu = u32::MAX;
    if sys_getcpu(&mut cpu, None) != 0 {
        return u32::MAX;
    }
    cpu
}

fn child_main() -> ! {
    let child_cpu = CHILD_CPU.load(Ordering::Acquire);
    let target = cpu_mask(child_cpu);
    let affinity_ret = sys_sched_setaffinity(0, &target);
    if affinity_ret != 0 {
        CHILD_ERROR.store(affinity_ret as usize, Ordering::Release);
        sys_exit(1);
    }

    for _ in 0..64 {
        let _ = sys_yield();
        if current_cpu() as usize == child_cpu {
            CHILD_STARTED.store(1, Ordering::Release);
            // Fill the secondary CPU's TLB with the old translation, then remain in user
            // mode while CPU0 unmaps and recreates this virtual page. This is
            // the regression point for remote sfence.vma and delayed frame
            // release: the later load must observe the replacement page.
            let mapping = TEST_MAPPING.load(Ordering::Acquire);
            if mapping == 0 || unsafe { (mapping as *const u8).read_volatile() } != 0x5a {
                CHILD_ERROR.store(3, Ordering::Release);
                sys_exit(3);
            }
            TLB_READY.store(1, Ordering::Release);
            while TLB_REMAP_DONE.load(Ordering::Acquire) == 0 {
                core::hint::spin_loop();
            }
            if unsafe { (mapping as *const u8).read_volatile() } != 0xa5 {
                CHILD_ERROR.store(4, Ordering::Release);
                sys_exit(4);
            }
            for i in 0..ITERATIONS {
                SHARED_VALUE.fetch_add(1, Ordering::AcqRel);
                if i % 64 == 0 {
                    let _ = sys_yield();
                }
            }
            CHILD_DONE.store(1, Ordering::Release);
            sys_exit(0);
        }
    }

    CHILD_ERROR.store(2, Ordering::Release);
    sys_exit(2);
}

#[no_mangle]
pub fn main() -> i32 {
    let mut online = [0u8; CPUSET_BYTES];
    assert_eq!(sys_sched_getaffinity(0, &mut online), CPUSET_BYTES as isize);
    let mut online_mask = 0usize;
    for (index, byte) in online.iter().enumerate() {
        online_mask |= (*byte as usize) << (index * 8);
    }
    let Some(child_cpu) = (1..usize::BITS as usize)
        .find(|cpu| online_mask & (1usize << cpu) != 0)
    else {
        println!("clonesmptest skipped: no secondary CPU is online");
        return 0;
    };
    CHILD_CPU.store(child_cpu, Ordering::Release);

    let cpu0 = cpu_mask(0);
    assert_eq!(sys_sched_setaffinity(0, &cpu0), 0);
    for _ in 0..16 {
        let _ = sys_yield();
        if current_cpu() == 0 {
            break;
        }
    }
    assert_eq!(current_cpu(), 0);

    let mapping = sys_mmap(
        0,
        PAGE_SIZE,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    assert!(mapping > 0);
    let mapping = mapping as usize;
    unsafe { (mapping as *mut u8).write_volatile(0x5a) };
    TEST_MAPPING.store(mapping, Ordering::Release);

    let child_stack = unsafe {
        (core::ptr::addr_of_mut!(CHILD_STACK) as *mut u8)
            .add(core::mem::size_of::<ChildStack>()) as usize
    };
    let child_pid = sys_clone(CLONE_VM | SIGCHLD, child_stack, 0, 0, 0);
    if child_pid == 0 {
        child_main();
    }
    assert!(child_pid > 0);
    println!("clonesmptest: waiting for CPU{} old TLB entry", child_cpu);

    for _ in 0..20_000 {
        if TLB_READY.load(Ordering::Acquire) != 0 || CHILD_ERROR.load(Ordering::Acquire) != 0 {
            break;
        }
        let _ = sys_yield();
    }
    if TLB_READY.load(Ordering::Acquire) != 1 {
        println!("clonesmptest failed: child did not cache the old mapping");
        return 1;
    }

    println!("clonesmptest: unmapping old page");
    assert_eq!(sys_munmap(mapping, PAGE_SIZE), 0);
    println!("clonesmptest: mapping replacement page");
    let remap = sys_mmap(
        mapping,
        PAGE_SIZE,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
        -1,
        0,
    );
    assert_eq!(remap, mapping as isize);
    unsafe { (mapping as *mut u8).write_volatile(0xa5) };
    TLB_REMAP_DONE.store(1, Ordering::Release);
    println!("clonesmptest: released CPU{}", child_cpu);

    let mut observed = 0usize;
    for _ in 0..20_000 {
        if CHILD_ERROR.load(Ordering::Acquire) != 0 {
            break;
        }
        observed = SHARED_VALUE.load(Ordering::Acquire);
        if CHILD_STARTED.load(Ordering::Acquire) != 0
            && CHILD_DONE.load(Ordering::Acquire) != 0
        {
            break;
        }
        let _ = sys_yield();
    }
    let child_error = CHILD_ERROR.load(Ordering::Acquire) as isize;
    if child_error != 0 {
        println!("clonesmptest failed: child affinity returned {}", child_error);
        return 1;
    }
    if CHILD_STARTED.load(Ordering::Acquire) != 1
        || CHILD_DONE.load(Ordering::Acquire) != 1
        || SHARED_VALUE.load(Ordering::Acquire) != ITERATIONS
        || observed > ITERATIONS
    {
        println!("clonesmptest failed: shared execution did not complete");
        return 1;
    }

    let mut exit_code = -1;
    if sys_waitpid(child_pid, &mut exit_code) != child_pid || exit_code != 0 {
        println!("clonesmptest failed: child wait status {}", exit_code);
        return 1;
    }

    // A non-CLONE_THREAD CLONE_VM child has a distinct TGID. exec must build
    // a replacement page table for that child without disturbing the parent
    // that still owns the original shared address space.
    let exec_pid = sys_clone(CLONE_VM | SIGCHLD, child_stack, 0, 0, 0);
    if exec_pid == 0 {
        let argv = [b"hello_world\0".as_ptr(), core::ptr::null()];
        let envp = [core::ptr::null()];
        let ret = sys_exec("hello_world\0", &argv, &envp);
        sys_exit(if ret == 0 { 90 } else { 91 });
    }
    if exec_pid <= 0 {
        println!("clonesmptest failed: exec clone returned {}", exec_pid);
        return 1;
    }
    exit_code = -1;
    if sys_waitpid(exec_pid, &mut exit_code) != exec_pid || exit_code != 0 {
        println!("clonesmptest failed: exec child status {}", exit_code);
        return 1;
    }
    if sys_waitpid(-1, &mut exit_code) != -10 {
        println!("clonesmptest failed: empty wait set did not return ECHILD");
        return 1;
    }
    println!("clonesmptest passed: parent CPU0, child CPU{}", child_cpu);
    0
}
