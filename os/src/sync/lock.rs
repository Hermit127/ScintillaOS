//! SMP-oriented kernel locks.
//!
//! `UPSafeCell` is intentionally kept for subsystems which have not yet been
//! audited.  New concurrent code must use one of the domain-specific locks in
//! this module instead of using `polyhal::utils::MutexNoIrq` directly.
//!
//! Lock order (outermost first):
//!
//! 0. [`CpuLocalLock`] -- current CPU state only; never shared between harts.
//! 1. [`PidRegistryLock`] -- only acquire/copy/remove an `Arc<TaskControlBlock>`.
//! 2. [`FutexLock`] -- futex wait-table registration only.
//! 3. [`SchedulerLock`] -- ready and wait queues.
//! 4. [`FilesLock`] -- shared descriptor-table mutation.
//! 5. [`TaskLock`] -- one task's mutable state.
//! 6. [`SignalActionsLock`] -- one process' shared signal dispositions.
//! 7. [`VmLock`] / [`VmRwLock`] -- address-space topology and shared-memory metadata.
//! 8. [`VmPagesLock`] -- resident pages owned by one VMA.
//! 9. [`VmPageTableRwLock`] -- concurrent walks and exclusive PTE updates.
//! 10. [`VfsLock`] -- mount, dentry and path-owner metadata.
//! 11. [`PageCacheRwLock`] -- shared immutable file pages.
//! 12. [`FrameRefLock`] -- shared physical-frame reference counts.
//! 13. [`FrameAllocatorLock`] -- physical-frame buddy allocator.
//! 14. [`FsLock`] -- one Ext4 instance.
//! 15. [`FsGlobalLock`] -- process-global lwext4 mount/device registry.
//! 16. [`DentryLock`] -- dentry child and inode caches.
//! 17. [`FsAuxLock`] -- post-filesystem quota/accounting metadata.
//! 18. [`DeviceLock`] -- one device or pipe ring.
//!
//! No lock may be held across scheduling, blocking I/O, user-address
//! translation, page-table changes, SBI calls, allocation which can re-enter
//! the kernel, or an external callback.  Locks are deliberately non-reentrant.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicUsize, Ordering};
use polyhal::utils::{
    MutexNoIrq, MutexNoIrqGuard, RwLockNoIrq, RwLockNoIrqReadGuard,
    RwLockNoIrqWriteGuard,
};

const NO_OWNER: usize = usize::MAX;
const LOCKDEP_ENABLED: bool = cfg!(any(
    debug_assertions,
    feature = "smp-diagnostics",
    feature = "lockdep"
));
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
const LOCK_CLASS_COUNT: usize = 19;
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static LOCK_ACQUISITIONS: [AtomicU64; LOCK_CLASS_COUNT] =
    [const { AtomicU64::new(0) }; LOCK_CLASS_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static LOCK_CONTENTIONS: [AtomicU64; LOCK_CLASS_COUNT] =
    [const { AtomicU64::new(0) }; LOCK_CLASS_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static LOCK_TRY_FAILURES: [AtomicU64; LOCK_CLASS_COUNT] =
    [const { AtomicU64::new(0) }; LOCK_CLASS_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static LOCK_SPIN_ITERATIONS: [AtomicU64; LOCK_CLASS_COUNT] =
    [const { AtomicU64::new(0) }; LOCK_CLASS_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static LOCK_WAIT_TOTAL_NS: [AtomicU64; LOCK_CLASS_COUNT] =
    [const { AtomicU64::new(0) }; LOCK_CLASS_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static LOCK_WAIT_MAX_NS: [AtomicU64; LOCK_CLASS_COUNT] =
    [const { AtomicU64::new(0) }; LOCK_CLASS_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
const NAMED_LOCK_COUNT: usize = 11;
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
const NAMED_LOCKS: [&str; NAMED_LOCK_COUNT] = [
    "Vm.address_space_areas",
    "Vm.detached_areas",
    "Vm.shared_memory_segments",
    "Vm.shared_file_mappings",
    "Vm.file_page_cache",
    "Vm.area_frames",
    "Vm.page_table",
    "Vm.frame_allocator",
    "Vm.frame_magazine",
    "VfsFsData",
    "VfsOpenFileState",
];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static NAMED_LOCK_CONTENTIONS: [AtomicU64; NAMED_LOCK_COUNT] =
    [const { AtomicU64::new(0) }; NAMED_LOCK_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static NAMED_LOCK_SPIN_ITERATIONS: [AtomicU64; NAMED_LOCK_COUNT] =
    [const { AtomicU64::new(0) }; NAMED_LOCK_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static NAMED_LOCK_WAIT_TOTAL_NS: [AtomicU64; NAMED_LOCK_COUNT] =
    [const { AtomicU64::new(0) }; NAMED_LOCK_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static NAMED_LOCK_WAIT_MAX_NS: [AtomicU64; NAMED_LOCK_COUNT] =
    [const { AtomicU64::new(0) }; NAMED_LOCK_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static VM_RW_READ_CONTENTIONS: AtomicU64 = AtomicU64::new(0);
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static VM_RW_READ_SPINS: AtomicU64 = AtomicU64::new(0);
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static VM_RW_WRITE_CONTENTIONS: AtomicU64 = AtomicU64::new(0);
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static VM_RW_WRITE_SPINS: AtomicU64 = AtomicU64::new(0);
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
const VM_WRITE_CALLSITE_COUNT: usize = 32;
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static VM_WRITE_CALLSITE_KEYS: [AtomicUsize; VM_WRITE_CALLSITE_COUNT] =
    [const { AtomicUsize::new(0) }; VM_WRITE_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static VM_WRITE_CALLSITE_FILE_PTRS: [AtomicUsize; VM_WRITE_CALLSITE_COUNT] =
    [const { AtomicUsize::new(0) }; VM_WRITE_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static VM_WRITE_CALLSITE_FILE_LENS: [AtomicUsize; VM_WRITE_CALLSITE_COUNT] =
    [const { AtomicUsize::new(0) }; VM_WRITE_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static VM_WRITE_CALLSITE_LINES: [AtomicUsize; VM_WRITE_CALLSITE_COUNT] =
    [const { AtomicUsize::new(0) }; VM_WRITE_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static VM_WRITE_CALLSITE_ACQUISITIONS: [AtomicU64; VM_WRITE_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; VM_WRITE_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static VM_WRITE_CALLSITE_TOTAL_NS: [AtomicU64; VM_WRITE_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; VM_WRITE_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static VM_WRITE_CALLSITE_MAX_NS: [AtomicU64; VM_WRITE_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; VM_WRITE_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
const FS_HOLD_CALLSITE_COUNT: usize = 64;
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_HOLD_CALLSITE_KEYS: [AtomicUsize; FS_HOLD_CALLSITE_COUNT] =
    [const { AtomicUsize::new(0) }; FS_HOLD_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_HOLD_CALLSITE_FILE_PTRS: [AtomicUsize; FS_HOLD_CALLSITE_COUNT] =
    [const { AtomicUsize::new(0) }; FS_HOLD_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_HOLD_CALLSITE_FILE_LENS: [AtomicUsize; FS_HOLD_CALLSITE_COUNT] =
    [const { AtomicUsize::new(0) }; FS_HOLD_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_HOLD_CALLSITE_LINES: [AtomicUsize; FS_HOLD_CALLSITE_COUNT] =
    [const { AtomicUsize::new(0) }; FS_HOLD_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_HOLD_CALLSITE_ACQUISITIONS: [AtomicU64; FS_HOLD_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; FS_HOLD_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_HOLD_CALLSITE_TOTAL_NS: [AtomicU64; FS_HOLD_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; FS_HOLD_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_HOLD_CALLSITE_MAX_NS: [AtomicU64; FS_HOLD_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; FS_HOLD_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_HOLD_CALLSITE_SLEEP_WAITS: [AtomicU64; FS_HOLD_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; FS_HOLD_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_HOLD_CALLSITE_IRQ_OFF_MISSES: [AtomicU64; FS_HOLD_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; FS_HOLD_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_HOLD_CALLSITE_SPINS: [AtomicU64; FS_HOLD_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; FS_HOLD_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_HOLD_CALLSITE_WAIT_TOTAL_NS: [AtomicU64; FS_HOLD_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; FS_HOLD_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_HOLD_CALLSITE_WAIT_MAX_NS: [AtomicU64; FS_HOLD_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; FS_HOLD_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
const FS_IRQ_CONTEXT_COUNT: usize = 32;
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_IRQ_CONTEXT_KEYS: [AtomicUsize; FS_IRQ_CONTEXT_COUNT] =
    [const { AtomicUsize::new(0) }; FS_IRQ_CONTEXT_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_IRQ_CONTEXT_FILE_PTRS: [AtomicUsize; FS_IRQ_CONTEXT_COUNT] =
    [const { AtomicUsize::new(0) }; FS_IRQ_CONTEXT_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_IRQ_CONTEXT_FILE_LENS: [AtomicUsize; FS_IRQ_CONTEXT_COUNT] =
    [const { AtomicUsize::new(0) }; FS_IRQ_CONTEXT_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_IRQ_CONTEXT_LINES: [AtomicUsize; FS_IRQ_CONTEXT_COUNT] =
    [const { AtomicUsize::new(0) }; FS_IRQ_CONTEXT_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_IRQ_CONTEXT_RANKS: [AtomicUsize; FS_IRQ_CONTEXT_COUNT] =
    [const { AtomicUsize::new(0) }; FS_IRQ_CONTEXT_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_IRQ_CONTEXT_EVENTS: [AtomicU64; FS_IRQ_CONTEXT_COUNT] =
    [const { AtomicU64::new(0) }; FS_IRQ_CONTEXT_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_IRQ_CONTEXT_MISSES: [AtomicU64; FS_IRQ_CONTEXT_COUNT] =
    [const { AtomicU64::new(0) }; FS_IRQ_CONTEXT_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static FS_IRQ_CONTEXT_SPINS: [AtomicU64; FS_IRQ_CONTEXT_COUNT] =
    [const { AtomicU64::new(0) }; FS_IRQ_CONTEXT_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
const TASK_CALLSITE_COUNT: usize = 64;
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static TASK_CALLSITE_KEYS: [AtomicUsize; TASK_CALLSITE_COUNT] =
    [const { AtomicUsize::new(0) }; TASK_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static TASK_CALLSITE_FILE_PTRS: [AtomicUsize; TASK_CALLSITE_COUNT] =
    [const { AtomicUsize::new(0) }; TASK_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static TASK_CALLSITE_FILE_LENS: [AtomicUsize; TASK_CALLSITE_COUNT] =
    [const { AtomicUsize::new(0) }; TASK_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static TASK_CALLSITE_LINES: [AtomicUsize; TASK_CALLSITE_COUNT] =
    [const { AtomicUsize::new(0) }; TASK_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static TASK_CALLSITE_ACQUISITIONS: [AtomicU64; TASK_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; TASK_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static TASK_CALLSITE_TOTAL_NS: [AtomicU64; TASK_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; TASK_CALLSITE_COUNT];
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
static TASK_CALLSITE_MAX_NS: [AtomicU64; TASK_CALLSITE_COUNT] =
    [const { AtomicU64::new(0) }; TASK_CALLSITE_COUNT];

/// The global ordering class for a kernel lock.
///
/// Locks in the same class are ordered by their stable in-memory identity.
/// Callers which need a semantic order such as PID order must arrange that
/// order before acquiring either object.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(usize)]
pub enum LockClass {
    CpuLocal = 0,
    PidRegistry = 1,
    /// The futex wait-table lock must precede SchedulerLock so a waiter can
    /// atomically publish both its key and its `Blocking` task state.
    Futex = 2,
    Scheduler = 3,
    Files = 4,
    Task = 5,
    SignalActions = 6,
    Vm = 7,
    VmPages = 8,
    VmPageTable = 9,
    Vfs = 10,
    PageCache = 11,
    FrameRef = 12,
    FrameAllocator = 13,
    Fs = 14,
    FsGlobal = 15,
    Dentry = 16,
    FsAux = 17,
    Device = 18,
}

impl LockClass {
    const fn rank(self) -> usize {
        self as usize
    }
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
fn record_lock_acquisition(
    class: LockClass,
    name: &'static str,
    misses: u64,
    spin_iterations: u64,
) {
    let index = class.rank();
    LOCK_ACQUISITIONS[index].fetch_add(1, Ordering::Relaxed);
    if misses != 0 {
        LOCK_CONTENTIONS[index].fetch_add(1, Ordering::Relaxed);
        LOCK_SPIN_ITERATIONS[index].fetch_add(spin_iterations, Ordering::Relaxed);
        if let Some(index) = NAMED_LOCKS.iter().position(|candidate| *candidate == name) {
            NAMED_LOCK_CONTENTIONS[index].fetch_add(1, Ordering::Relaxed);
            NAMED_LOCK_SPIN_ITERATIONS[index].fetch_add(spin_iterations, Ordering::Relaxed);
        }
    }
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
#[inline]
fn begin_lock_wait(wait_started_ns: &mut u64) {
    if *wait_started_ns == 0 {
        *wait_started_ns = (polyhal::time::Time::now().to_nsec() as u64).max(1);
    }
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
fn record_lock_wait(class: LockClass, name: &'static str, wait_started_ns: u64) -> u64 {
    if wait_started_ns == 0 {
        return 0;
    }
    let elapsed_ns = (polyhal::time::Time::now().to_nsec() as u64)
        .saturating_sub(wait_started_ns);
    let index = class.rank();
    LOCK_WAIT_TOTAL_NS[index].fetch_add(elapsed_ns, Ordering::Relaxed);
    LOCK_WAIT_MAX_NS[index].fetch_max(elapsed_ns, Ordering::Relaxed);
    if let Some(index) = NAMED_LOCKS.iter().position(|candidate| *candidate == name) {
        NAMED_LOCK_WAIT_TOTAL_NS[index].fetch_add(elapsed_ns, Ordering::Relaxed);
        NAMED_LOCK_WAIT_MAX_NS[index].fetch_max(elapsed_ns, Ordering::Relaxed);
    }
    elapsed_ns
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
fn record_vm_rw_contention(write: bool, misses: u64, spin_iterations: u64) {
    if misses == 0 {
        return;
    }
    let (contentions, spins) = if write {
        (&VM_RW_WRITE_CONTENTIONS, &VM_RW_WRITE_SPINS)
    } else {
        (&VM_RW_READ_CONTENTIONS, &VM_RW_READ_SPINS)
    };
    contentions.fetch_add(1, Ordering::Relaxed);
    spins.fetch_add(spin_iterations, Ordering::Relaxed);
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
fn register_vm_write_callsite(caller: &'static core::panic::Location<'static>) -> usize {
    let file = caller.file();
    let key = ((file.as_ptr() as usize).rotate_left(13) ^ caller.line() as usize).max(1);
    let start = key % VM_WRITE_CALLSITE_COUNT;
    for offset in 0..VM_WRITE_CALLSITE_COUNT {
        let slot = (start + offset) % VM_WRITE_CALLSITE_COUNT;
        let existing = VM_WRITE_CALLSITE_KEYS[slot].load(Ordering::Acquire);
        if existing == key {
            return slot;
        }
        if existing == 0
            && VM_WRITE_CALLSITE_KEYS[slot]
                .compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            VM_WRITE_CALLSITE_FILE_PTRS[slot]
                .store(file.as_ptr() as usize, Ordering::Relaxed);
            VM_WRITE_CALLSITE_FILE_LENS[slot].store(file.len(), Ordering::Relaxed);
            VM_WRITE_CALLSITE_LINES[slot].store(caller.line() as usize, Ordering::Release);
            return slot;
        }
    }
    usize::MAX
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
fn record_vm_write_hold(slot: usize, elapsed_ns: u64) {
    if slot == usize::MAX {
        return;
    }
    VM_WRITE_CALLSITE_ACQUISITIONS[slot].fetch_add(1, Ordering::Relaxed);
    VM_WRITE_CALLSITE_TOTAL_NS[slot].fetch_add(elapsed_ns, Ordering::Relaxed);
    VM_WRITE_CALLSITE_MAX_NS[slot].fetch_max(elapsed_ns, Ordering::Relaxed);
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
fn register_fs_hold_callsite(caller: &'static core::panic::Location<'static>) -> usize {
    let file = caller.file();
    let key = ((file.as_ptr() as usize).rotate_left(17) ^ caller.line() as usize).max(1);
    let start = key % FS_HOLD_CALLSITE_COUNT;
    for offset in 0..FS_HOLD_CALLSITE_COUNT {
        let slot = (start + offset) % FS_HOLD_CALLSITE_COUNT;
        let existing = FS_HOLD_CALLSITE_KEYS[slot].load(Ordering::Acquire);
        if existing == key {
            return slot;
        }
        if existing == 0
            && FS_HOLD_CALLSITE_KEYS[slot]
                .compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            FS_HOLD_CALLSITE_FILE_PTRS[slot]
                .store(file.as_ptr() as usize, Ordering::Relaxed);
            FS_HOLD_CALLSITE_FILE_LENS[slot].store(file.len(), Ordering::Relaxed);
            FS_HOLD_CALLSITE_LINES[slot].store(caller.line() as usize, Ordering::Release);
            return slot;
        }
    }
    usize::MAX
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
fn record_fs_hold(slot: usize, elapsed_ns: u64) {
    if slot == usize::MAX {
        return;
    }
    FS_HOLD_CALLSITE_ACQUISITIONS[slot].fetch_add(1, Ordering::Relaxed);
    FS_HOLD_CALLSITE_TOTAL_NS[slot].fetch_add(elapsed_ns, Ordering::Relaxed);
    FS_HOLD_CALLSITE_MAX_NS[slot].fetch_max(elapsed_ns, Ordering::Relaxed);
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
fn record_fs_wait(
    slot: usize,
    sleep_waits: u64,
    irq_off_misses: u64,
    spins: u64,
    wait_ns: u64,
) {
    if slot == usize::MAX {
        return;
    }
    FS_HOLD_CALLSITE_SLEEP_WAITS[slot].fetch_add(sleep_waits, Ordering::Relaxed);
    FS_HOLD_CALLSITE_IRQ_OFF_MISSES[slot].fetch_add(irq_off_misses, Ordering::Relaxed);
    FS_HOLD_CALLSITE_SPINS[slot].fetch_add(spins, Ordering::Relaxed);
    FS_HOLD_CALLSITE_WAIT_TOTAL_NS[slot].fetch_add(wait_ns, Ordering::Relaxed);
    FS_HOLD_CALLSITE_WAIT_MAX_NS[slot].fetch_max(wait_ns, Ordering::Relaxed);
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
fn register_fs_irq_context() -> usize {
    const NO_DOMAIN_LOCK: &str = "<no-domain-lock>";
    let context = debug_cpu().and_then(|cpu| {
        let depth = LOCK_DEPTH[cpu]
            .load(Ordering::Acquire)
            .min(LOCK_DEBUG_DEPTH);
        (0..depth).rev().find_map(|index| {
            let slot = rank_slot(cpu, index);
            let rank = LOCK_RANKS[slot].load(Ordering::Acquire);
            (rank != LockClass::CpuLocal.rank()).then(|| {
                (
                    LOCK_FILE_PTRS[slot].load(Ordering::Acquire),
                    LOCK_FILE_LENS[slot].load(Ordering::Acquire),
                    LOCK_LINES[slot].load(Ordering::Acquire),
                    rank,
                )
            })
        })
    });
    let (file_ptr, file_len, line, rank) = context.unwrap_or((
        NO_DOMAIN_LOCK.as_ptr() as usize,
        NO_DOMAIN_LOCK.len(),
        0,
        LOCK_CLASS_COUNT,
    ));
    let key = (file_ptr.rotate_left(11) ^ line.rotate_left(3) ^ rank).max(1);
    let start = key % FS_IRQ_CONTEXT_COUNT;
    for offset in 0..FS_IRQ_CONTEXT_COUNT {
        let slot = (start + offset) % FS_IRQ_CONTEXT_COUNT;
        let existing = FS_IRQ_CONTEXT_KEYS[slot].load(Ordering::Acquire);
        if existing == key {
            return slot;
        }
        if existing == 0
            && FS_IRQ_CONTEXT_KEYS[slot]
                .compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            FS_IRQ_CONTEXT_FILE_PTRS[slot].store(file_ptr, Ordering::Relaxed);
            FS_IRQ_CONTEXT_FILE_LENS[slot].store(file_len, Ordering::Relaxed);
            FS_IRQ_CONTEXT_LINES[slot].store(line, Ordering::Relaxed);
            FS_IRQ_CONTEXT_RANKS[slot].store(rank, Ordering::Release);
            return slot;
        }
    }
    usize::MAX
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
fn record_fs_irq_context(slot: usize, misses: u64, spins: u64) {
    if slot == usize::MAX || misses == 0 {
        return;
    }
    FS_IRQ_CONTEXT_EVENTS[slot].fetch_add(1, Ordering::Relaxed);
    FS_IRQ_CONTEXT_MISSES[slot].fetch_add(misses, Ordering::Relaxed);
    FS_IRQ_CONTEXT_SPINS[slot].fetch_add(spins, Ordering::Relaxed);
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
fn register_task_hold_callsite(caller: &'static core::panic::Location<'static>) -> usize {
    let file = caller.file();
    let key = ((file.as_ptr() as usize).rotate_left(19) ^ caller.line() as usize).max(1);
    let start = key % TASK_CALLSITE_COUNT;
    for offset in 0..TASK_CALLSITE_COUNT {
        let slot = (start + offset) % TASK_CALLSITE_COUNT;
        let existing = TASK_CALLSITE_KEYS[slot].load(Ordering::Acquire);
        if existing == key {
            TASK_CALLSITE_ACQUISITIONS[slot].fetch_add(1, Ordering::Relaxed);
            return slot;
        }
        if existing == 0
            && TASK_CALLSITE_KEYS[slot]
                .compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            TASK_CALLSITE_FILE_PTRS[slot].store(file.as_ptr() as usize, Ordering::Relaxed);
            TASK_CALLSITE_FILE_LENS[slot].store(file.len(), Ordering::Relaxed);
            TASK_CALLSITE_LINES[slot].store(caller.line() as usize, Ordering::Release);
            TASK_CALLSITE_ACQUISITIONS[slot].store(1, Ordering::Relaxed);
            return slot;
        }
    }
    usize::MAX
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
fn record_task_hold(slot: usize, elapsed_ns: u64) {
    if slot == usize::MAX {
        return;
    }
    TASK_CALLSITE_TOTAL_NS[slot].fetch_add(elapsed_ns, Ordering::Relaxed);
    TASK_CALLSITE_MAX_NS[slot].fetch_max(elapsed_ns, Ordering::Relaxed);
}

#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
pub fn dump_lock_contention_stats() {
    const NAMES: [&str; LOCK_CLASS_COUNT] = [
        "CpuLocal",
        "PidRegistry",
        "Futex",
        "Scheduler",
        "Files",
        "Task",
        "SignalActions",
        "Vm",
        "VmPages",
        "VmPageTable",
        "Vfs",
        "PageCache",
        "FrameRef",
        "FrameAllocator",
        "Fs",
        "FsGlobal",
        "Dentry",
        "FsAux",
        "Device",
    ];
    for (index, name) in NAMES.iter().enumerate() {
        let acquisitions = LOCK_ACQUISITIONS[index].load(Ordering::Relaxed);
        if acquisitions == 0 {
            continue;
        }
        crate::println!(
            "[lock-stats] class={} acquisitions={} contended={} try_failures={} spins={} wait_total_us={} wait_max_us={}",
            name,
            acquisitions,
            LOCK_CONTENTIONS[index].load(Ordering::Relaxed),
            LOCK_TRY_FAILURES[index].load(Ordering::Relaxed),
            LOCK_SPIN_ITERATIONS[index].load(Ordering::Relaxed),
            LOCK_WAIT_TOTAL_NS[index].load(Ordering::Relaxed) / 1_000,
            LOCK_WAIT_MAX_NS[index].load(Ordering::Relaxed) / 1_000,
        );
    }
    for (index, name) in NAMED_LOCKS.iter().enumerate() {
        let contentions = NAMED_LOCK_CONTENTIONS[index].load(Ordering::Relaxed);
        if contentions == 0 {
            continue;
        }
        crate::println!(
            "[lock-stats] name={} contended={} spins={} wait_total_us={} wait_max_us={}",
            name,
            contentions,
            NAMED_LOCK_SPIN_ITERATIONS[index].load(Ordering::Relaxed),
            NAMED_LOCK_WAIT_TOTAL_NS[index].load(Ordering::Relaxed) / 1_000,
            NAMED_LOCK_WAIT_MAX_NS[index].load(Ordering::Relaxed) / 1_000,
        );
    }
    crate::println!(
        "[lock-stats] vm-rw read_contended={} read_spins={} write_contended={} write_spins={}",
        VM_RW_READ_CONTENTIONS.load(Ordering::Relaxed),
        VM_RW_READ_SPINS.load(Ordering::Relaxed),
        VM_RW_WRITE_CONTENTIONS.load(Ordering::Relaxed),
        VM_RW_WRITE_SPINS.load(Ordering::Relaxed),
    );
    for slot in 0..VM_WRITE_CALLSITE_COUNT {
        let acquisitions = VM_WRITE_CALLSITE_ACQUISITIONS[slot].load(Ordering::Relaxed);
        if acquisitions == 0 {
            continue;
        }
        let file_ptr = VM_WRITE_CALLSITE_FILE_PTRS[slot].load(Ordering::Relaxed) as *const u8;
        let file_len = VM_WRITE_CALLSITE_FILE_LENS[slot].load(Ordering::Relaxed);
        let file = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(file_ptr, file_len))
        };
        crate::println!(
            "[lock-stats] vm-write site={}:{} acquisitions={} total_us={} max_us={}",
            file,
            VM_WRITE_CALLSITE_LINES[slot].load(Ordering::Acquire),
            acquisitions,
            VM_WRITE_CALLSITE_TOTAL_NS[slot].load(Ordering::Relaxed) / 1_000,
            VM_WRITE_CALLSITE_MAX_NS[slot].load(Ordering::Relaxed) / 1_000,
        );
    }
    for slot in 0..FS_HOLD_CALLSITE_COUNT {
        let acquisitions = FS_HOLD_CALLSITE_ACQUISITIONS[slot].load(Ordering::Relaxed);
        if acquisitions == 0 {
            continue;
        }
        let file_ptr = FS_HOLD_CALLSITE_FILE_PTRS[slot].load(Ordering::Relaxed) as *const u8;
        let file_len = FS_HOLD_CALLSITE_FILE_LENS[slot].load(Ordering::Relaxed);
        let file = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(file_ptr, file_len))
        };
        crate::println!(
            "[lock-stats] fs-hold site={}:{} acquisitions={} total_us={} max_us={} sleep_waits={} irq_off_misses={} spins={} wait_total_us={} wait_max_us={}",
            file,
            FS_HOLD_CALLSITE_LINES[slot].load(Ordering::Acquire),
            acquisitions,
            FS_HOLD_CALLSITE_TOTAL_NS[slot].load(Ordering::Relaxed) / 1_000,
            FS_HOLD_CALLSITE_MAX_NS[slot].load(Ordering::Relaxed) / 1_000,
            FS_HOLD_CALLSITE_SLEEP_WAITS[slot].load(Ordering::Relaxed),
            FS_HOLD_CALLSITE_IRQ_OFF_MISSES[slot].load(Ordering::Relaxed),
            FS_HOLD_CALLSITE_SPINS[slot].load(Ordering::Relaxed),
            FS_HOLD_CALLSITE_WAIT_TOTAL_NS[slot].load(Ordering::Relaxed) / 1_000,
            FS_HOLD_CALLSITE_WAIT_MAX_NS[slot].load(Ordering::Relaxed) / 1_000,
        );
    }
    for slot in 0..FS_IRQ_CONTEXT_COUNT {
        let events = FS_IRQ_CONTEXT_EVENTS[slot].load(Ordering::Relaxed);
        if events == 0 {
            continue;
        }
        let file_ptr = FS_IRQ_CONTEXT_FILE_PTRS[slot].load(Ordering::Relaxed) as *const u8;
        let file_len = FS_IRQ_CONTEXT_FILE_LENS[slot].load(Ordering::Relaxed);
        let file = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(file_ptr, file_len))
        };
        let rank = FS_IRQ_CONTEXT_RANKS[slot].load(Ordering::Acquire);
        let class = NAMES.get(rank).copied().unwrap_or("None");
        crate::println!(
            "[lock-stats] fs-irq-context class={} acquired_at={}:{} events={} misses={} spins={}",
            class,
            file,
            FS_IRQ_CONTEXT_LINES[slot].load(Ordering::Relaxed),
            events,
            FS_IRQ_CONTEXT_MISSES[slot].load(Ordering::Relaxed),
            FS_IRQ_CONTEXT_SPINS[slot].load(Ordering::Relaxed),
        );
    }
    for slot in 0..TASK_CALLSITE_COUNT {
        let acquisitions = TASK_CALLSITE_ACQUISITIONS[slot].load(Ordering::Relaxed);
        if acquisitions == 0 {
            continue;
        }
        let file_ptr = TASK_CALLSITE_FILE_PTRS[slot].load(Ordering::Relaxed) as *const u8;
        let file_len = TASK_CALLSITE_FILE_LENS[slot].load(Ordering::Relaxed);
        let file = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(file_ptr, file_len))
        };
        crate::println!(
            "[lock-stats] task site={}:{} acquisitions={} total_us={} max_us={}",
            file,
            TASK_CALLSITE_LINES[slot].load(Ordering::Acquire),
            acquisitions,
            TASK_CALLSITE_TOTAL_NS[slot].load(Ordering::Relaxed) / 1_000,
            TASK_CALLSITE_MAX_NS[slot].load(Ordering::Relaxed) / 1_000,
        );
    }
}

const LOCK_DEBUG_CPUS: usize = crate::config::MAX_CPUS;
const LOCK_DEBUG_DEPTH: usize = 16;

// The project currently targets four RISC-V harts.  CPUs outside this range
// (for example the still-single-core LoongArch build) simply do not participate
// in the debug stack; the lock itself remains fully functional.
static LOCK_DEPTH: [AtomicUsize; LOCK_DEBUG_CPUS] =
    [const { AtomicUsize::new(0) }; LOCK_DEBUG_CPUS];
static LOCK_RANKS: [AtomicUsize; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH] =
    [const { AtomicUsize::new(0) }; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH];
static LOCK_KEYS: [AtomicUsize; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH] =
    [const { AtomicUsize::new(0) }; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH];
static LOCK_NAME_PTRS: [AtomicUsize; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH] =
    [const { AtomicUsize::new(0) }; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH];
static LOCK_NAME_LENS: [AtomicUsize; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH] =
    [const { AtomicUsize::new(0) }; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH];
static LOCK_FILE_PTRS: [AtomicUsize; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH] =
    [const { AtomicUsize::new(0) }; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH];
static LOCK_FILE_LENS: [AtomicUsize; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH] =
    [const { AtomicUsize::new(0) }; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH];
static LOCK_LINES: [AtomicUsize; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH] =
    [const { AtomicUsize::new(0) }; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH];

const DEFERRED_WAKE_SLOTS: usize = 256;
const DEFERRED_WAKE_ALL: usize = 1;
static DEFERRED_LOCK_WAKES: [AtomicUsize; DEFERRED_WAKE_SLOTS] =
    [const { AtomicUsize::new(0) }; DEFERRED_WAKE_SLOTS];
static DEFERRED_LOCK_WAKE_PENDING: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_LOCK_WAKE_OVERFLOW: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn debug_cpu() -> Option<usize> {
    crate::task::try_current_hart_id().filter(|cpu| *cpu < LOCK_DEBUG_CPUS)
}

#[inline]
fn rank_slot(cpu: usize, depth: usize) -> usize {
    cpu * LOCK_DEBUG_DEPTH + depth
}

fn recorded_lock_name(slot: usize) -> &'static str {
    let name_ptr = LOCK_NAME_PTRS[slot].load(Ordering::Acquire);
    let name_len = LOCK_NAME_LENS[slot].load(Ordering::Acquire);
    if name_ptr == 0 || name_len == 0 {
        return "<updating>";
    }
    unsafe {
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(
            name_ptr as *const u8,
            name_len,
        ))
    }
}

fn recorded_lock_file(slot: usize) -> &'static str {
    let file_ptr = LOCK_FILE_PTRS[slot].load(Ordering::Acquire);
    let file_len = LOCK_FILE_LENS[slot].load(Ordering::Acquire);
    if file_ptr == 0 || file_len == 0 {
        return "<updating>";
    }
    unsafe {
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(
            file_ptr as *const u8,
            file_len,
        ))
    }
}

#[track_caller]
fn lockdep_before_acquire(class: LockClass, name: &'static str, key: usize) {
    if !LOCKDEP_ENABLED {
        return;
    }
    let Some(cpu) = debug_cpu() else {
        return;
    };
    let depth = LOCK_DEPTH[cpu].load(Ordering::Relaxed);
    if depth == 0 || class == LockClass::CpuLocal {
        // CpuLocalLock is a per-hart, local-IRQ-protected accessor rather than
        // a shared-resource dependency.  It is safe to snapshot the current
        // task while another domain lock is held.  Recursion is still rejected
        // by SpinNoIrq::check_not_recursive.
        return;
    }
    let caller = core::panic::Location::caller();
    // Check the complete held stack because CpuLocalLock is deliberately
    // rank-neutral and may temporarily sit above a shared lock.
    for index in 0..depth {
        let slot = rank_slot(cpu, index);
        let held = LOCK_RANKS[slot].load(Ordering::Relaxed);
        if held == LockClass::CpuLocal.rank() {
            continue;
        }
        let held_key = LOCK_KEYS[slot].load(Ordering::Relaxed);
        let held_name = recorded_lock_name(slot);
        let held_file = recorded_lock_file(slot);
        let held_line = LOCK_LINES[slot].load(Ordering::Relaxed);
        assert!(
            class.rank() >= held,
            "lock-order inversion on hart {} at {}:{}: acquire '{}' (rank {}) while holding '{}' at rank {}, acquired at {}:{}",
            cpu,
            caller.file(),
            caller.line(),
            name,
            class.rank(),
            held_name,
            held,
            held_file,
            held_line,
        );
        assert!(
            class.rank() != held || key > held_key,
            "same-class lock inversion on hart {} at {}:{}: acquire '{}' ({:#x}) after '{}' ({:#x}) at rank {}, acquired at {}:{}",
            cpu,
            caller.file(),
            caller.line(),
            name,
            key,
            held_name,
            held_key,
            held,
            held_file,
            held_line,
        );
    }
}

fn lockdep_acquired(
    class: LockClass,
    name: &'static str,
    key: usize,
    caller: &'static core::panic::Location<'static>,
) {
    if !LOCKDEP_ENABLED {
        return;
    }
    let Some(cpu) = debug_cpu() else {
        return;
    };
    let depth = LOCK_DEPTH[cpu].load(Ordering::Relaxed);
    assert!(
        depth < LOCK_DEBUG_DEPTH,
        "lock nesting overflow on hart {} while acquiring '{}'",
        cpu,
        name,
    );
    let slot = rank_slot(cpu, depth);
    LOCK_RANKS[slot].store(class.rank(), Ordering::Relaxed);
    LOCK_KEYS[slot].store(key, Ordering::Relaxed);
    LOCK_NAME_PTRS[slot].store(name.as_ptr() as usize, Ordering::Relaxed);
    LOCK_NAME_LENS[slot].store(name.len(), Ordering::Relaxed);
    LOCK_FILE_PTRS[slot].store(caller.file().as_ptr() as usize, Ordering::Relaxed);
    LOCK_FILE_LENS[slot].store(caller.file().len(), Ordering::Relaxed);
    LOCK_LINES[slot].store(caller.line() as usize, Ordering::Relaxed);
    LOCK_DEPTH[cpu].store(depth + 1, Ordering::Relaxed);
}

fn lockdep_released(class: LockClass, name: &'static str, key: usize) {
    if !LOCKDEP_ENABLED {
        return;
    }
    let Some(cpu) = debug_cpu() else {
        return;
    };
    let depth = LOCK_DEPTH[cpu].load(Ordering::Relaxed);
    assert!(
        depth > 0,
        "lock '{}' released without a matching acquire on hart {}",
        name,
        cpu,
    );
    let slot = rank_slot(cpu, depth - 1);
    let held = LOCK_RANKS[slot].load(Ordering::Relaxed);
    let held_key = LOCK_KEYS[slot].load(Ordering::Relaxed);
    assert!(
        held == class.rank() && held_key == key,
        "lock '{}' released out of order on hart {} (top rank/key {}/{:#x}, releasing {}/{:#x})",
        name,
        cpu,
        held,
        held_key,
        class.rank(),
        key,
    );
    LOCK_RANKS[slot].store(0, Ordering::Relaxed);
    LOCK_KEYS[slot].store(0, Ordering::Relaxed);
    LOCK_NAME_PTRS[slot].store(0, Ordering::Relaxed);
    LOCK_NAME_LENS[slot].store(0, Ordering::Relaxed);
    LOCK_FILE_PTRS[slot].store(0, Ordering::Relaxed);
    LOCK_FILE_LENS[slot].store(0, Ordering::Relaxed);
    LOCK_LINES[slot].store(0, Ordering::Relaxed);
    LOCK_DEPTH[cpu].store(depth - 1, Ordering::Relaxed);
}

/// Panic in a debug build if the current CPU still holds a domain lock.
///
/// Scheduler and blocking paths call this before a context switch.  It is a
/// guard rail, not a replacement for releasing the guard lexically.
#[track_caller]
pub fn assert_no_spin_locks_held(context: &'static str) {
    if !LOCKDEP_ENABLED {
        return;
    }
    if let Some(cpu) = debug_cpu() {
        let depth = LOCK_DEPTH[cpu].load(Ordering::Relaxed);
        assert!(
            depth == 0,
            "{} on hart {} while {} SMP lock(s) are held",
            context,
            cpu,
            depth,
        );
    }
}

/// Print each hart's held-lock stack without acquiring another kernel lock.
///
/// Names stored here are all `&'static str`, so reconstructing them from their
/// recorded address remains valid even when another hart owns the lock.
pub fn dump_lockdep_state() {
    if !LOCKDEP_ENABLED {
        return;
    }
    for cpu in 0..LOCK_DEBUG_CPUS {
        let depth = LOCK_DEPTH[cpu]
            .load(Ordering::Acquire)
            .min(LOCK_DEBUG_DEPTH);
        for index in 0..depth {
            let slot = rank_slot(cpu, index);
            let rank = LOCK_RANKS[slot].load(Ordering::Acquire);
            let key = LOCK_KEYS[slot].load(Ordering::Acquire);
            let name = recorded_lock_name(slot);
            let file = recorded_lock_file(slot);
            let line = LOCK_LINES[slot].load(Ordering::Acquire);
            println!(
                "[lockdep] hart={} depth={} rank={} key={:#x} name={} acquired_at={}:{}",
                cpu, index, rank, key, name, file, line
            );
        }
    }
}

fn defer_lock_waiter_wake(key: usize, wake_all: bool) {
    debug_assert_ne!(key, 0);
    debug_assert_eq!(key & DEFERRED_WAKE_ALL, 0);
    let encoded = key | usize::from(wake_all);
    let start = (key >> 3) % DEFERRED_WAKE_SLOTS;
    for offset in 0..DEFERRED_WAKE_SLOTS {
        let slot = &DEFERRED_LOCK_WAKES[(start + offset) % DEFERRED_WAKE_SLOTS];
        loop {
            let current = slot.load(Ordering::Acquire);
            if current == 0 {
                if slot
                    .compare_exchange(0, encoded, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    DEFERRED_LOCK_WAKE_PENDING.store(1, Ordering::Release);
                    return;
                }
                continue;
            }
            if current & !DEFERRED_WAKE_ALL == key {
                if wake_all && current & DEFERRED_WAKE_ALL == 0 {
                    if slot
                        .compare_exchange(
                            current,
                            current | DEFERRED_WAKE_ALL,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }
                }
                DEFERRED_LOCK_WAKE_PENDING.store(1, Ordering::Release);
                return;
            }
            break;
        }
    }
    DEFERRED_LOCK_WAKE_OVERFLOW.store(1, Ordering::Release);
    DEFERRED_LOCK_WAKE_PENDING.store(1, Ordering::Release);
}

/// Complete lock waiter wakeups only from a scheduler-safe point.
///
/// Releasing a VM or filesystem sleep lock can happen while a higher-level
/// task lock is still live. Publishing the key here mirrors deferred wakeup
/// work in Linux and avoids acquiring SchedulerLock from a guard destructor.
pub fn drain_deferred_lock_wakes() {
    if DEFERRED_LOCK_WAKE_PENDING.swap(0, Ordering::AcqRel) == 0 {
        return;
    }
    assert_no_spin_locks_held("deferred lock wake");
    if DEFERRED_LOCK_WAKE_OVERFLOW.swap(0, Ordering::AcqRel) != 0 {
        crate::task::wake_all_sleep_lock_waiters_any();
    }
    for slot in DEFERRED_LOCK_WAKES.iter() {
        let encoded = slot.swap(0, Ordering::AcqRel);
        if encoded == 0 {
            continue;
        }
        let key = encoded & !DEFERRED_WAKE_ALL;
        if encoded & DEFERRED_WAKE_ALL != 0 {
            crate::task::wake_all_sleep_lock_waiters(key);
        } else {
            crate::task::wake_sleep_lock_waiter(key);
        }
    }
}

/// A non-reentrant local-IRQ-disabling spin lock with a lock class and name.
///
/// The actual mutual exclusion primitive remains `polyhal::MutexNoIrq`; this
/// wrapper centralizes the SMP policy and optional lock-order diagnostics.
pub struct SpinNoIrq<T> {
    inner: MutexNoIrq<T>,
    name: &'static str,
    class: LockClass,
    owner: AtomicUsize,
}

unsafe impl<T: Send> Send for SpinNoIrq<T> {}
unsafe impl<T: Send> Sync for SpinNoIrq<T> {}

impl<T> SpinNoIrq<T> {
    pub const fn new(value: T, name: &'static str, class: LockClass) -> Self {
        Self {
            inner: MutexNoIrq::new(value),
            name,
            class,
            owner: AtomicUsize::new(NO_OWNER),
        }
    }

    #[inline]
    #[track_caller]
    fn check_not_recursive(&self) {
        if !LOCKDEP_ENABLED {
            return;
        }
        if let Some(cpu) = debug_cpu() {
            if self.owner.load(Ordering::Acquire) == cpu {
                let caller = core::panic::Location::caller();
                dump_lockdep_state();
                panic!(
                    "recursive acquisition of '{}' on hart {} requested at {}:{}",
                    self.name,
                    cpu,
                    caller.file(),
                    caller.line()
                );
            }
        }
    }

    #[inline]
    fn acquired(&self, caller: &'static core::panic::Location<'static>) {
        if let Some(cpu) = debug_cpu() {
            // The underlying spin::Mutex is the ownership authority.  This
            // atomic is deliberately snapshot-only: checking its previous
            // value after acquiring the real mutex races with the preceding
            // guard's diagnostic teardown and produced false SMP failures.
            // Recursive and ABBA acquisition are checked by the per-hart
            // lockdep stack before the mutex is entered.
            self.owner.store(cpu, Ordering::Release);
        }
        lockdep_acquired(
            self.class,
            self.name,
            self as *const Self as usize,
            caller,
        );
    }

    /// Acquire the lock, disabling local interrupts while it is held.
    #[track_caller]
    pub fn lock(&self) -> SpinNoIrqGuard<'_, T> {
        let caller = core::panic::Location::caller();
        self.check_not_recursive();
        lockdep_before_acquire(self.class, self.name, self as *const Self as usize);
        #[cfg(not(any(feature = "smp-diagnostics", feature = "lock-stats")))]
        let guard = self.inner.lock();
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let guard = {
            let mut backoff = 1u64;
            let mut misses = 0u64;
            let mut spin_iterations = 0u64;
            let mut wait_started_ns = 0u64;
            loop {
                if let Some(guard) = self.inner.try_lock() {
                    record_lock_acquisition(self.class, self.name, misses, spin_iterations);
                    record_lock_wait(self.class, self.name, wait_started_ns);
                    break guard;
                }
                begin_lock_wait(&mut wait_started_ns);
                misses += 1;
                spin_iterations += backoff;
                for _ in 0..backoff {
                    core::hint::spin_loop();
                }
                backoff = (backoff << 1).min(64);
            }
        };
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let stats_slot = if self.class == LockClass::Fs {
            register_fs_hold_callsite(caller)
        } else if self.class == LockClass::Task {
            register_task_hold_callsite(caller)
        } else {
            usize::MAX
        };
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let stats_started_ns = if stats_slot == usize::MAX {
            0
        } else {
            polyhal::time::Time::now().to_nsec() as u64
        };
        self.acquired(caller);
        SpinNoIrqGuard {
            lock: self,
            guard: Some(guard),
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            stats_slot,
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            stats_started_ns,
        }
    }

    /// Try to acquire the lock without waiting.
    #[track_caller]
    pub fn try_lock(&self) -> Option<SpinNoIrqGuard<'_, T>> {
        let caller = core::panic::Location::caller();
        if LOCKDEP_ENABLED
            && debug_cpu()
                .map(|cpu| self.owner.load(Ordering::Acquire) == cpu)
                .unwrap_or(false)
        {
            return None;
        }
        lockdep_before_acquire(self.class, self.name, self as *const Self as usize);
        let guard = match self.inner.try_lock() {
            Some(guard) => guard,
            None => {
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                LOCK_TRY_FAILURES[self.class.rank()].fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        record_lock_acquisition(self.class, self.name, 0, 0);
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let stats_slot = if self.class == LockClass::Fs {
            register_fs_hold_callsite(caller)
        } else if self.class == LockClass::Task {
            register_task_hold_callsite(caller)
        } else {
            usize::MAX
        };
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let stats_started_ns = if stats_slot == usize::MAX {
            0
        } else {
            polyhal::time::Time::now().to_nsec() as u64
        };
        self.acquired(caller);
        Some(SpinNoIrqGuard {
            lock: self,
            guard: Some(guard),
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            stats_slot,
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            stats_started_ns,
        })
    }

    pub fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }

    pub fn owner(&self) -> usize {
        self.owner.load(Ordering::Acquire)
    }

}

/// Guard returned by [`SpinNoIrq::lock`].
pub struct SpinNoIrqGuard<'a, T> {
    lock: &'a SpinNoIrq<T>,
    guard: Option<MutexNoIrqGuard<'a, T>>,
    #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
    stats_slot: usize,
    #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
    stats_started_ns: u64,
}

impl<T> Deref for SpinNoIrqGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

impl<T> DerefMut for SpinNoIrqGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().unwrap()
    }
}

impl<T> Drop for SpinNoIrqGuard<'_, T> {
    fn drop(&mut self) {
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        if self.stats_slot != usize::MAX {
            let elapsed_ns = (polyhal::time::Time::now().to_nsec() as u64)
                .saturating_sub(self.stats_started_ns);
            if self.lock.class == LockClass::Fs {
                record_fs_hold(self.stats_slot, elapsed_ns);
            } else if self.lock.class == LockClass::Task {
                record_task_hold(self.stats_slot, elapsed_ns);
            }
        }
        lockdep_released(
            self.lock.class,
            self.lock.name,
            self.lock as *const SpinNoIrq<T> as usize,
        );
        if debug_cpu().is_some() {
            self.lock.owner.store(NO_OWNER, Ordering::Release);
        }
        // Drop the underlying guard last: its Drop restores the saved local
        // interrupt state after the debug metadata has become consistent.
        self.guard.take();
    }
}

/// A writer-preferring, local-IRQ-disabling read/write lock for one address
/// space. Read guards keep page-table translations stable while allowing
/// concurrent user-buffer copies from CLONE_VM siblings. Mapping, COW and VMA
/// mutations use the exclusive write side.
pub struct VmRwLock<T> {
    inner: RwLockNoIrq<T>,
    name: &'static str,
    writer_owner: AtomicUsize,
    waiters: AtomicUsize,
    waiting_writers: AtomicUsize,
}

unsafe impl<T: Send + Sync> Send for VmRwLock<T> {}
unsafe impl<T: Send + Sync> Sync for VmRwLock<T> {}

impl<T> VmRwLock<T> {
    pub const fn new(value: T) -> Self {
        Self::new_named(value, "VmRwLock")
    }

    pub const fn new_named(value: T, name: &'static str) -> Self {
        Self {
            inner: RwLockNoIrq::new(value),
            name,
            writer_owner: AtomicUsize::new(NO_OWNER),
            waiters: AtomicUsize::new(0),
            waiting_writers: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn key(&self) -> usize {
        self as *const Self as usize
    }

    #[inline]
    fn can_sleep() -> bool {
        polyhal::irq::IRQ::int_enabled() && crate::task::current_task().is_some()
    }

    #[inline]
    #[track_caller]
    fn check_writer_not_recursive(&self) {
        if !LOCKDEP_ENABLED {
            return;
        }
        if let Some(cpu) = debug_cpu() {
            if self.writer_owner.load(Ordering::Acquire) == cpu {
                let caller = core::panic::Location::caller();
                dump_lockdep_state();
                panic!(
                    "recursive acquisition of '{}' on hart {} requested at {}:{}",
                    self.name,
                    cpu,
                    caller.file(),
                    caller.line()
                );
            }
        }
    }

    #[inline]
    fn acquired(&self, caller: &'static core::panic::Location<'static>, write: bool) {
        if write {
            if let Some(cpu) = debug_cpu() {
                self.writer_owner.store(cpu, Ordering::Release);
            }
        }
        lockdep_acquired(LockClass::Vm, self.name, self.key(), caller);
    }

    #[track_caller]
    pub fn read(&self) -> VmRwReadGuard<'_, T> {
        let caller = core::panic::Location::caller();
        self.check_writer_not_recursive();
        lockdep_before_acquire(LockClass::Vm, self.name, self.key());
        let can_sleep = Self::can_sleep();
        let guard = {
            let mut backoff = 1usize;
            let mut attempts = 0usize;
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            let mut misses = 0u64;
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            let mut spin_iterations = 0u64;
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            let mut wait_started_ns = 0u64;
            loop {
                if !can_sleep || self.waiting_writers.load(Ordering::Acquire) == 0 {
                    if let Some(guard) = self.inner.try_read() {
                        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                        {
                            record_lock_acquisition(
                                LockClass::Vm,
                                self.name,
                                misses,
                                spin_iterations,
                            );
                            record_lock_wait(LockClass::Vm, self.name, wait_started_ns);
                            record_vm_rw_contention(false, misses, spin_iterations);
                        }
                        break guard;
                    }
                }
                attempts += 1;
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                {
                    begin_lock_wait(&mut wait_started_ns);
                    misses += 1;
                }
                if can_sleep && attempts >= 4 {
                    self.waiters.fetch_add(1, Ordering::AcqRel);
                    crate::task::block_current_and_run_next_if(
                        crate::task::BlockReason::SleepLock(self.key()),
                        || {
                            self.waiting_writers.load(Ordering::Acquire) != 0
                                || self.inner.try_read().is_none()
                        },
                    );
                    self.waiters.fetch_sub(1, Ordering::AcqRel);
                    attempts = 0;
                    backoff = 1;
                    continue;
                }
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                {
                    spin_iterations += backoff as u64;
                }
                for _ in 0..backoff {
                    core::hint::spin_loop();
                }
                backoff = (backoff << 1).min(64);
            }
        };
        self.acquired(caller, false);
        VmRwReadGuard {
            lock: self,
            guard: Some(guard),
        }
    }

    #[track_caller]
    pub fn write(&self) -> VmRwWriteGuard<'_, T> {
        let caller = core::panic::Location::caller();
        self.check_writer_not_recursive();
        lockdep_before_acquire(LockClass::Vm, self.name, self.key());
        let can_sleep = Self::can_sleep();
        if can_sleep {
            self.waiting_writers.fetch_add(1, Ordering::AcqRel);
        }
        let guard = if can_sleep {
            let mut backoff = 1usize;
            let mut attempts = 0usize;
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            let mut misses = 0u64;
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            let mut spin_iterations = 0u64;
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            let mut wait_started_ns = 0u64;
            loop {
                if let Some(guard) = self.inner.try_write() {
                    #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                    {
                        record_lock_acquisition(
                            LockClass::Vm,
                            self.name,
                            misses,
                            spin_iterations,
                        );
                        record_lock_wait(LockClass::Vm, self.name, wait_started_ns);
                        record_vm_rw_contention(true, misses, spin_iterations);
                    }
                    break guard;
                }
                attempts += 1;
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                {
                    begin_lock_wait(&mut wait_started_ns);
                    misses += 1;
                }
                if attempts >= 4 {
                    self.waiters.fetch_add(1, Ordering::AcqRel);
                    crate::task::block_current_and_run_next_if(
                        crate::task::BlockReason::SleepLock(self.key()),
                        || self.inner.try_write().is_none(),
                    );
                    self.waiters.fetch_sub(1, Ordering::AcqRel);
                    attempts = 0;
                    backoff = 1;
                    continue;
                }
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                {
                    spin_iterations += backoff as u64;
                }
                for _ in 0..backoff {
                    core::hint::spin_loop();
                }
                backoff = (backoff << 1).min(64);
            }
        } else {
            let mut backoff = 1usize;
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            let mut misses = 0u64;
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            let mut spin_iterations = 0u64;
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            let mut wait_started_ns = 0u64;
            let guard = loop {
                if let Some(guard) = self.inner.try_upgradeable_read() {
                    #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                    {
                        record_lock_acquisition(
                            LockClass::Vm,
                            self.name,
                            misses,
                            spin_iterations,
                        );
                        record_lock_wait(LockClass::Vm, self.name, wait_started_ns);
                        record_vm_rw_contention(true, misses, spin_iterations);
                    }
                    break guard;
                }
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                {
                    begin_lock_wait(&mut wait_started_ns);
                    misses += 1;
                    spin_iterations += backoff as u64;
                }
                for _ in 0..backoff {
                    core::hint::spin_loop();
                }
                backoff = (backoff << 1).min(64);
            };
            guard.upgrade()
        };
        if can_sleep {
            self.waiting_writers.fetch_sub(1, Ordering::AcqRel);
        }
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let stats_slot = register_vm_write_callsite(caller);
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let stats_started_ns = polyhal::time::Time::now().to_nsec() as u64;
        self.acquired(caller, true);
        VmRwWriteGuard {
            lock: self,
            guard: Some(guard),
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            stats_slot,
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            stats_started_ns,
        }
    }

    #[track_caller]
    pub fn exclusive_access(&self) -> VmRwWriteGuard<'_, T> {
        self.write()
    }

    #[track_caller]
    pub fn try_read(&self) -> Option<VmRwReadGuard<'_, T>> {
        let caller = core::panic::Location::caller();
        self.check_writer_not_recursive();
        lockdep_before_acquire(LockClass::Vm, self.name, self.key());
        let guard = match (self.waiting_writers.load(Ordering::Acquire) == 0)
            .then(|| self.inner.try_read())
            .flatten()
        {
            Some(guard) => guard,
            None => {
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                LOCK_TRY_FAILURES[LockClass::Vm.rank()].fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        record_lock_acquisition(LockClass::Vm, self.name, 0, 0);
        self.acquired(caller, false);
        Some(VmRwReadGuard {
            lock: self,
            guard: Some(guard),
        })
    }

    #[track_caller]
    pub fn try_write(&self) -> Option<VmRwWriteGuard<'_, T>> {
        let caller = core::panic::Location::caller();
        self.check_writer_not_recursive();
        lockdep_before_acquire(LockClass::Vm, self.name, self.key());
        let guard = match self.inner.try_write() {
            Some(guard) => guard,
            None => {
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                LOCK_TRY_FAILURES[LockClass::Vm.rank()].fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        record_lock_acquisition(LockClass::Vm, self.name, 0, 0);
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let stats_slot = register_vm_write_callsite(caller);
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let stats_started_ns = polyhal::time::Time::now().to_nsec() as u64;
        self.acquired(caller, true);
        Some(VmRwWriteGuard {
            lock: self,
            guard: Some(guard),
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            stats_slot,
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            stats_started_ns,
        })
    }

    pub fn owner(&self) -> usize {
        self.writer_owner.load(Ordering::Acquire)
    }
}

pub struct VmRwReadGuard<'a, T> {
    lock: &'a VmRwLock<T>,
    guard: Option<RwLockNoIrqReadGuard<'a, T>>,
}

impl<T> Deref for VmRwReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

impl<T> Drop for VmRwReadGuard<'_, T> {
    fn drop(&mut self) {
        lockdep_released(LockClass::Vm, self.lock.name, self.lock.key());
        self.guard.take();
        if self.lock.waiters.load(Ordering::Acquire) != 0 {
            defer_lock_waiter_wake(self.lock.key(), true);
        }
    }
}

pub struct VmRwWriteGuard<'a, T> {
    lock: &'a VmRwLock<T>,
    guard: Option<RwLockNoIrqWriteGuard<'a, T>>,
    #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
    stats_slot: usize,
    #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
    stats_started_ns: u64,
}

impl<T> Deref for VmRwWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

impl<T> DerefMut for VmRwWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().unwrap()
    }
}

impl<T> Drop for VmRwWriteGuard<'_, T> {
    fn drop(&mut self) {
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        record_vm_write_hold(
            self.stats_slot,
            (polyhal::time::Time::now().to_nsec() as u64).saturating_sub(self.stats_started_ns),
        );
        lockdep_released(LockClass::Vm, self.lock.name, self.lock.key());
        if debug_cpu().is_some() {
            self.lock.writer_owner.store(NO_OWNER, Ordering::Release);
        }
        self.guard.take();
        if self.lock.waiters.load(Ordering::Acquire) != 0 {
            defer_lock_waiter_wake(self.lock.key(), true);
        }
    }
}

/// Read-mostly lock for lwext4's process-global mount and device registry.
/// Filesystem operations keep a shared registry reference while mount and
/// unmount paths take the exclusive side.
pub struct FsGlobalLock<T> {
    inner: RwLockNoIrq<T>,
    name: &'static str,
    writer_owner: AtomicUsize,
}

unsafe impl<T: Send + Sync> Send for FsGlobalLock<T> {}
unsafe impl<T: Send + Sync> Sync for FsGlobalLock<T> {}

impl<T> FsGlobalLock<T> {
    pub const fn new(value: T) -> Self {
        Self::new_named(value, "FsGlobalLock")
    }

    pub const fn new_named(value: T, name: &'static str) -> Self {
        Self {
            inner: RwLockNoIrq::new(value),
            name,
            writer_owner: AtomicUsize::new(NO_OWNER),
        }
    }

    #[inline]
    fn key(&self) -> usize {
        self as *const Self as usize
    }

    #[inline]
    #[track_caller]
    fn check_writer_not_recursive(&self) {
        if !LOCKDEP_ENABLED {
            return;
        }
        if let Some(cpu) = debug_cpu() {
            if self.writer_owner.load(Ordering::Acquire) == cpu {
                let caller = core::panic::Location::caller();
                dump_lockdep_state();
                panic!(
                    "recursive acquisition of '{}' on hart {} requested at {}:{}",
                    self.name,
                    cpu,
                    caller.file(),
                    caller.line()
                );
            }
        }
    }

    #[inline]
    fn acquired(&self, caller: &'static core::panic::Location<'static>, write: bool) {
        if write {
            if let Some(cpu) = debug_cpu() {
                self.writer_owner.store(cpu, Ordering::Release);
            }
        }
        lockdep_acquired(LockClass::FsGlobal, self.name, self.key(), caller);
    }

    #[track_caller]
    pub fn read(&self) -> FsGlobalReadGuard<'_, T> {
        let caller = core::panic::Location::caller();
        self.check_writer_not_recursive();
        lockdep_before_acquire(LockClass::FsGlobal, self.name, self.key());
        let mut backoff = 1u64;
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let mut misses = 0u64;
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let mut spin_iterations = 0u64;
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let mut wait_started_ns = 0u64;
        let guard = loop {
            if let Some(guard) = self.inner.try_read() {
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                {
                    record_lock_acquisition(
                        LockClass::FsGlobal,
                        self.name,
                        misses,
                        spin_iterations,
                    );
                    record_lock_wait(LockClass::FsGlobal, self.name, wait_started_ns);
                }
                break guard;
            }
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            {
                begin_lock_wait(&mut wait_started_ns);
                misses += 1;
                spin_iterations += backoff;
            }
            for _ in 0..backoff {
                core::hint::spin_loop();
            }
            backoff = (backoff << 1).min(64);
        };
        self.acquired(caller, false);
        FsGlobalReadGuard {
            lock: self,
            guard: Some(guard),
        }
    }

    #[track_caller]
    pub fn write(&self) -> FsGlobalGuard<'_, T> {
        let caller = core::panic::Location::caller();
        self.check_writer_not_recursive();
        lockdep_before_acquire(LockClass::FsGlobal, self.name, self.key());
        let mut backoff = 1u64;
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let mut misses = 0u64;
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let mut spin_iterations = 0u64;
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let mut wait_started_ns = 0u64;
        let guard = loop {
            if let Some(guard) = self.inner.try_upgradeable_read() {
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                {
                    record_lock_acquisition(
                        LockClass::FsGlobal,
                        self.name,
                        misses,
                        spin_iterations,
                    );
                    record_lock_wait(LockClass::FsGlobal, self.name, wait_started_ns);
                }
                break guard.upgrade();
            }
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            {
                begin_lock_wait(&mut wait_started_ns);
                misses += 1;
                spin_iterations += backoff;
            }
            for _ in 0..backoff {
                core::hint::spin_loop();
            }
            backoff = (backoff << 1).min(64);
        };
        self.acquired(caller, true);
        FsGlobalGuard {
            lock: self,
            guard: Some(guard),
        }
    }

    #[track_caller]
    pub fn exclusive_access(&self) -> FsGlobalGuard<'_, T> {
        self.write()
    }

    pub fn owner(&self) -> usize {
        self.writer_owner.load(Ordering::Acquire)
    }
}

pub struct FsGlobalReadGuard<'a, T> {
    lock: &'a FsGlobalLock<T>,
    guard: Option<RwLockNoIrqReadGuard<'a, T>>,
}

impl<T> Deref for FsGlobalReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

impl<T> Drop for FsGlobalReadGuard<'_, T> {
    fn drop(&mut self) {
        lockdep_released(LockClass::FsGlobal, self.lock.name, self.lock.key());
        self.guard.take();
    }
}

pub struct FsGlobalGuard<'a, T> {
    lock: &'a FsGlobalLock<T>,
    guard: Option<RwLockNoIrqWriteGuard<'a, T>>,
}

impl<T> Deref for FsGlobalGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

impl<T> DerefMut for FsGlobalGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().unwrap()
    }
}

impl<T> Drop for FsGlobalGuard<'_, T> {
    fn drop(&mut self) {
        lockdep_released(LockClass::FsGlobal, self.lock.name, self.lock.key());
        if debug_cpu().is_some() {
            self.lock.writer_owner.store(NO_OWNER, Ordering::Release);
        }
        self.guard.take();
    }
}

/// A scheduler-aware mutex for state that may be held across slow kernel I/O.
///
/// Unlike the domain spin locks, this lock leaves interrupts enabled. A task
/// which loses the fast-path race publishes a keyed wait before scheduling,
/// while boot and interrupt contexts retain a spin fallback. It must therefore
/// never be acquired while a domain spin lock is held.
pub struct SleepLock<T> {
    value: UnsafeCell<T>,
    locked: AtomicUsize,
    waiters: AtomicUsize,
    owner: AtomicUsize,
    name: &'static str,
    stats_class: usize,
}

unsafe impl<T: Send> Send for SleepLock<T> {}
unsafe impl<T: Send> Sync for SleepLock<T> {}

impl<T> SleepLock<T> {
    pub const fn new(value: T) -> Self {
        Self::new_named(value, "SleepLock")
    }

    pub const fn new_named(value: T, name: &'static str) -> Self {
        Self::new_domain(value, name, usize::MAX)
    }

    const fn new_domain(value: T, name: &'static str, stats_class: usize) -> Self {
        Self {
            value: UnsafeCell::new(value),
            locked: AtomicUsize::new(0),
            waiters: AtomicUsize::new(0),
            owner: AtomicUsize::new(NO_OWNER),
            name,
            stats_class,
        }
    }

    #[inline]
    fn key(&self) -> usize {
        self as *const Self as usize
    }

    #[inline]
    fn can_sleep() -> bool {
        polyhal::irq::IRQ::int_enabled() && crate::task::current_task().is_some()
    }

    #[track_caller]
    pub fn lock(&self) -> SleepGuard<'_, T> {
        let caller = core::panic::Location::caller();
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let stats_slot = if self.stats_class == LockClass::Fs.rank() {
            register_fs_hold_callsite(caller)
        } else {
            usize::MAX
        };
        let mut backoff = 1u64;
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let mut misses = 0u64;
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let mut spin_iterations = 0u64;
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let mut sleep_waits = 0u64;
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let mut irq_off_misses = 0u64;
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let mut wait_started_ns = 0u64;
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let mut irq_context_slot = None;
        loop {
            if self
                .locked
                .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            {
                begin_lock_wait(&mut wait_started_ns);
                misses += 1;
            }
            if Self::can_sleep() {
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                {
                    sleep_waits += 1;
                }
                self.waiters.fetch_add(1, Ordering::AcqRel);
                crate::task::block_current_and_run_next_if(
                    crate::task::BlockReason::SleepLock(self.key()),
                    || self.locked.load(Ordering::Acquire) != 0,
                );
                self.waiters.fetch_sub(1, Ordering::AcqRel);
                backoff = 1;
                continue;
            }
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            {
                if irq_context_slot.is_none() && self.stats_class == LockClass::Fs.rank() {
                    irq_context_slot = Some(register_fs_irq_context());
                }
                irq_off_misses += 1;
                spin_iterations += backoff;
            }
            for _ in 0..backoff {
                core::hint::spin_loop();
            }
            backoff = (backoff << 1).min(64);
        }
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let wait_ns = if self.stats_class != usize::MAX {
            record_lock_acquisition(
                LockClass::Fs,
                self.name,
                misses,
                spin_iterations,
            );
            record_lock_wait(LockClass::Fs, self.name, wait_started_ns)
        } else {
            0
        };
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        if self.stats_class == LockClass::Fs.rank() {
            record_fs_wait(
                stats_slot,
                sleep_waits,
                irq_off_misses,
                spin_iterations,
                wait_ns,
            );
            if let Some(slot) = irq_context_slot {
                record_fs_irq_context(slot, irq_off_misses, spin_iterations);
            }
        }
        self.owner
            .store(debug_cpu().unwrap_or(NO_OWNER), Ordering::Release);
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let stats_started_ns = if stats_slot == usize::MAX {
            0
        } else {
            polyhal::time::Time::now().to_nsec() as u64
        };
        SleepGuard {
            lock: self,
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            stats_slot,
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            stats_started_ns,
        }
    }

    #[track_caller]
    pub fn exclusive_access(&self) -> SleepGuard<'_, T> {
        self.lock()
    }

    #[track_caller]
    pub fn try_lock(&self) -> Option<SleepGuard<'_, T>> {
        let caller = core::panic::Location::caller();
        if self
            .locked
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            if self.stats_class != usize::MAX {
                LOCK_TRY_FAILURES[LockClass::Fs.rank()].fetch_add(1, Ordering::Relaxed);
            }
            return None;
        }
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        if self.stats_class != usize::MAX {
            record_lock_acquisition(LockClass::Fs, self.name, 0, 0);
        }
        self.owner
            .store(debug_cpu().unwrap_or(NO_OWNER), Ordering::Release);
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let stats_slot = if self.stats_class == LockClass::Fs.rank() {
            register_fs_hold_callsite(caller)
        } else {
            usize::MAX
        };
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        let stats_started_ns = if stats_slot == usize::MAX {
            0
        } else {
            polyhal::time::Time::now().to_nsec() as u64
        };
        Some(SleepGuard {
            lock: self,
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            stats_slot,
            #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
            stats_started_ns,
        })
    }

    #[track_caller]
    pub fn try_borrow(&self) -> Option<SleepGuard<'_, T>> {
        self.try_lock()
    }

    #[track_caller]
    pub fn try_borrow_mut(&self) -> Option<SleepGuard<'_, T>> {
        self.try_lock()
    }

    pub fn owner(&self) -> usize {
        self.owner.load(Ordering::Acquire)
    }

    pub fn has_waiters(&self) -> bool {
        self.waiters.load(Ordering::Acquire) != 0
    }
}

pub struct SleepGuard<'a, T> {
    lock: &'a SleepLock<T>,
    #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
    stats_slot: usize,
    #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
    stats_started_ns: u64,
}

impl<T> Deref for SleepGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SleepGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SleepGuard<'_, T> {
    fn drop(&mut self) {
        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
        if self.stats_slot != usize::MAX {
            record_fs_hold(
                self.stats_slot,
                (polyhal::time::Time::now().to_nsec() as u64)
                    .saturating_sub(self.stats_started_ns),
            );
        }
        self.lock.owner.store(NO_OWNER, Ordering::Release);
        self.lock.locked.store(0, Ordering::Release);
        if self.lock.waiters.load(Ordering::Acquire) != 0 {
            defer_lock_waiter_wake(self.lock.key(), false);
        }
    }
}

pub struct FsLock<T>(SleepLock<T>);

impl<T> FsLock<T> {
    pub const fn new(value: T) -> Self {
        Self::new_named(value, "FsLock")
    }

    pub const fn new_named(value: T, name: &'static str) -> Self {
        Self(SleepLock::new_domain(value, name, LockClass::Fs.rank()))
    }

    #[track_caller]
    pub fn lock(&self) -> FsGuard<'_, T> {
        self.0.lock()
    }

    #[track_caller]
    pub fn exclusive_access(&self) -> FsGuard<'_, T> {
        self.lock()
    }

    #[track_caller]
    pub fn try_lock(&self) -> Option<FsGuard<'_, T>> {
        self.0.try_lock()
    }

    pub fn owner(&self) -> usize {
        self.0.owner()
    }

    pub fn has_waiters(&self) -> bool {
        self.0.has_waiters()
    }
}

pub type FsGuard<'a, T> = SleepGuard<'a, T>;

macro_rules! define_domain_lock {
    ($type_name:ident, $guard_name:ident, $class:expr) => {
        pub struct $type_name<T>(SpinNoIrq<T>);

        impl<T> $type_name<T> {
            pub const fn new(value: T) -> Self {
                Self(SpinNoIrq::new(value, stringify!($type_name), $class))
            }

            pub const fn new_named(value: T, name: &'static str) -> Self {
                Self(SpinNoIrq::new(value, name, $class))
            }

            #[track_caller]
            pub fn lock(&self) -> $guard_name<'_, T> {
                self.0.lock()
            }

            #[track_caller]
            pub fn exclusive_access(&self) -> $guard_name<'_, T> {
                self.lock()
            }

            #[track_caller]
            pub fn try_lock(&self) -> Option<$guard_name<'_, T>> {
                self.0.try_lock()
            }

            #[track_caller]
            pub fn try_borrow(&self) -> Option<$guard_name<'_, T>> {
                self.try_lock()
            }

            #[track_caller]
            pub fn try_borrow_mut(&self) -> Option<$guard_name<'_, T>> {
                self.try_lock()
            }

            pub fn owner(&self) -> usize {
                self.0.owner()
            }
        }

        pub type $guard_name<'a, T> = SpinNoIrqGuard<'a, T>;
    };
}

macro_rules! define_domain_rw_lock {
    ($type_name:ident, $read_guard:ident, $write_guard:ident, $class:expr) => {
        pub struct $type_name<T> {
            inner: RwLockNoIrq<T>,
            name: &'static str,
            writer_owner: AtomicUsize,
        }

        impl<T> $type_name<T> {
            pub const fn new_named(value: T, name: &'static str) -> Self {
                Self {
                    inner: RwLockNoIrq::new(value),
                    name,
                    writer_owner: AtomicUsize::new(NO_OWNER),
                }
            }

            #[inline]
            fn key(&self) -> usize {
                self as *const Self as usize
            }

            #[track_caller]
            pub fn read(&self) -> $read_guard<'_, T> {
                let caller = core::panic::Location::caller();
                lockdep_before_acquire($class, self.name, self.key());
                let mut backoff = 1u64;
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                let mut misses = 0u64;
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                let mut spin_iterations = 0u64;
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                let mut wait_started_ns = 0u64;
                let guard = loop {
                    if let Some(guard) = self.inner.try_read() {
                        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                        {
                            record_lock_acquisition($class, self.name, misses, spin_iterations);
                            record_lock_wait($class, self.name, wait_started_ns);
                        }
                        break guard;
                    }
                    #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                    {
                        begin_lock_wait(&mut wait_started_ns);
                        misses += 1;
                        spin_iterations += backoff;
                    }
                    for _ in 0..backoff {
                        core::hint::spin_loop();
                    }
                    backoff = (backoff << 1).min(64);
                };
                lockdep_acquired($class, self.name, self.key(), caller);
                $read_guard {
                    lock: self,
                    guard: Some(guard),
                }
            }

            #[track_caller]
            pub fn write(&self) -> $write_guard<'_, T> {
                let caller = core::panic::Location::caller();
                lockdep_before_acquire($class, self.name, self.key());
                let mut backoff = 1u64;
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                let mut misses = 0u64;
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                let mut spin_iterations = 0u64;
                #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                let mut wait_started_ns = 0u64;
                let guard = loop {
                    if let Some(guard) = self.inner.try_upgradeable_read() {
                        #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                        {
                            record_lock_acquisition($class, self.name, misses, spin_iterations);
                            record_lock_wait($class, self.name, wait_started_ns);
                        }
                        break guard.upgrade();
                    }
                    #[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
                    {
                        begin_lock_wait(&mut wait_started_ns);
                        misses += 1;
                        spin_iterations += backoff;
                    }
                    for _ in 0..backoff {
                        core::hint::spin_loop();
                    }
                    backoff = (backoff << 1).min(64);
                };
                if let Some(cpu) = debug_cpu() {
                    self.writer_owner.store(cpu, Ordering::Release);
                }
                lockdep_acquired($class, self.name, self.key(), caller);
                $write_guard {
                    lock: self,
                    guard: Some(guard),
                }
            }
        }

        pub struct $read_guard<'a, T> {
            lock: &'a $type_name<T>,
            guard: Option<RwLockNoIrqReadGuard<'a, T>>,
        }

        impl<T> Deref for $read_guard<'_, T> {
            type Target = T;

            fn deref(&self) -> &Self::Target {
                self.guard.as_ref().unwrap()
            }
        }

        impl<T> Drop for $read_guard<'_, T> {
            fn drop(&mut self) {
                lockdep_released($class, self.lock.name, self.lock.key());
                self.guard.take();
            }
        }

        pub struct $write_guard<'a, T> {
            lock: &'a $type_name<T>,
            guard: Option<RwLockNoIrqWriteGuard<'a, T>>,
        }

        impl<T> Deref for $write_guard<'_, T> {
            type Target = T;

            fn deref(&self) -> &Self::Target {
                self.guard.as_ref().unwrap()
            }
        }

        impl<T> DerefMut for $write_guard<'_, T> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                self.guard.as_mut().unwrap()
            }
        }

        impl<T> Drop for $write_guard<'_, T> {
            fn drop(&mut self) {
                lockdep_released($class, self.lock.name, self.lock.key());
                if debug_cpu().is_some() {
                    self.lock.writer_owner.store(NO_OWNER, Ordering::Release);
                }
                self.guard.take();
            }
        }
    };
}

define_domain_lock!(PidRegistryLock, PidRegistryGuard, LockClass::PidRegistry);
define_domain_lock!(CpuLocalLock, CpuLocalGuard, LockClass::CpuLocal);
define_domain_lock!(FutexLock, FutexGuard, LockClass::Futex);
define_domain_lock!(SchedulerLock, SchedulerGuard, LockClass::Scheduler);
define_domain_lock!(FilesLock, FilesGuard, LockClass::Files);
define_domain_lock!(TaskLock, TaskGuard, LockClass::Task);
define_domain_lock!(
    SignalActionsLock,
    SignalActionsGuard,
    LockClass::SignalActions
);
define_domain_lock!(VmLock, VmGuard, LockClass::Vm);
define_domain_lock!(VmPagesLock, VmPagesGuard, LockClass::VmPages);
define_domain_rw_lock!(
    VmPageTableRwLock,
    VmPageTableReadGuard,
    VmPageTableWriteGuard,
    LockClass::VmPageTable
);
define_domain_lock!(PageCacheLock, PageCacheGuard, LockClass::PageCache);
define_domain_rw_lock!(
    PageCacheRwLock,
    PageCacheReadGuard,
    PageCacheWriteGuard,
    LockClass::PageCache
);
define_domain_lock!(FrameRefLock, FrameRefGuard, LockClass::FrameRef);
define_domain_lock!(
    FrameAllocatorLock,
    FrameAllocatorGuard,
    LockClass::FrameAllocator
);
define_domain_lock!(VfsLock, VfsGuard, LockClass::Vfs);
define_domain_lock!(DentryLock, DentryGuard, LockClass::Dentry);
define_domain_lock!(FsAuxLock, FsAuxGuard, LockClass::FsAux);
define_domain_lock!(DeviceLock, DeviceGuard, LockClass::Device);
