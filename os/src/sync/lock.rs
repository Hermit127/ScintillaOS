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
//! 7. [`VmLock`] -- address-space and shared-memory metadata.
//! 8. [`FrameRefLock`] -- shared physical-frame reference counts.
//! 9. [`FrameAllocatorLock`] -- physical-frame buddy allocator.
//! 10. [`VfsLock`] -- open-file, mount and path-owner metadata.
//! 11. [`FsGlobalLock`] -- process-global lwext4 state.
//! 12. [`FsLock`] -- one Ext4 instance.
//! 13. [`DentryLock`] -- dentry child and inode caches.
//! 14. [`FsAuxLock`] -- post-filesystem quota/accounting metadata.
//! 15. [`DeviceLock`] -- one device or pipe ring.
//!
//! No lock may be held across scheduling, blocking I/O, user-address
//! translation, page-table changes, SBI calls, allocation which can re-enter
//! the kernel, or an external callback.  Locks are deliberately non-reentrant.

use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicUsize, Ordering};
use polyhal::utils::{MutexNoIrq, MutexNoIrqGuard};

const NO_OWNER: usize = usize::MAX;
const LOCKDEP_ENABLED: bool = cfg!(any(debug_assertions, feature = "smp-diagnostics"));

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
    PageCache = 8,
    FrameRef = 9,
    FrameAllocator = 10,
    Vfs = 11,
    FsGlobal = 12,
    Fs = 13,
    Dentry = 14,
    FsAux = 15,
    Device = 16,
}

impl LockClass {
    const fn rank(self) -> usize {
        self as usize
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
        let guard = self.inner.lock();
        self.acquired(caller);
        SpinNoIrqGuard {
            lock: self,
            guard: Some(guard),
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
        let guard = self.inner.try_lock()?;
        self.acquired(caller);
        Some(SpinNoIrqGuard {
            lock: self,
            guard: Some(guard),
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
define_domain_lock!(PageCacheLock, PageCacheGuard, LockClass::PageCache);
define_domain_lock!(FrameRefLock, FrameRefGuard, LockClass::FrameRef);
define_domain_lock!(
    FrameAllocatorLock,
    FrameAllocatorGuard,
    LockClass::FrameAllocator
);
define_domain_lock!(VfsLock, VfsGuard, LockClass::Vfs);
define_domain_lock!(DentryLock, DentryGuard, LockClass::Dentry);
define_domain_lock!(FsGlobalLock, FsGlobalGuard, LockClass::FsGlobal);
define_domain_lock!(FsLock, FsGuard, LockClass::Fs);
define_domain_lock!(FsAuxLock, FsAuxGuard, LockClass::FsAux);
define_domain_lock!(DeviceLock, DeviceGuard, LockClass::Device);
