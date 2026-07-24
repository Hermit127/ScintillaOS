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
//! 4. [`TaskLock`] -- one task's mutable state.
//! 5. [`VmLock`] -- global shared-memory, VM cache and physical-frame metadata.
//! 6. [`VfsLock`] -- mount, dentry and inode metadata.
//! 7. [`FsLock`] -- an Ext4 instance.
//! 8. [`DeviceLock`] -- one device or pipe ring.
//!
//! No lock may be held across scheduling, blocking I/O, user-address
//! translation, page-table changes, SBI calls, allocation which can re-enter
//! the kernel, or an external callback.  Locks are deliberately non-reentrant.

use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicUsize, Ordering};
use polyhal::utils::{MutexNoIrq, MutexNoIrqGuard};

const NO_OWNER: usize = usize::MAX;

/// The global ordering class for a kernel lock.
///
/// Equal classes are allowed by the checker because object identity is not
/// available to a generic lock.  Callers that need two objects of the same
/// class must acquire them in stable order (PID, then address/device ID).
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(usize)]
pub enum LockClass {
    CpuLocal = 0,
    PidRegistry = 1,
    /// The futex wait-table lock must precede SchedulerLock so a waiter can
    /// atomically publish both its key and its `Blocking` task state.
    Futex = 2,
    Scheduler = 3,
    Task = 4,
    Vm = 5,
    Vfs = 6,
    Fs = 7,
    Device = 8,
}

impl LockClass {
    const fn rank(self) -> usize {
        self as usize
    }
}

const LOCK_DEBUG_CPUS: usize = 4;
const LOCK_DEBUG_DEPTH: usize = 16;

// The project currently targets four RISC-V harts.  CPUs outside this range
// (for example the still-single-core LoongArch build) simply do not participate
// in the debug stack; the lock itself remains fully functional.
static LOCK_DEPTH: [AtomicUsize; LOCK_DEBUG_CPUS] =
    [const { AtomicUsize::new(0) }; LOCK_DEBUG_CPUS];
static LOCK_RANKS: [AtomicUsize; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH] =
    [const { AtomicUsize::new(0) }; LOCK_DEBUG_CPUS * LOCK_DEBUG_DEPTH];

#[inline]
fn debug_cpu() -> Option<usize> {
    let cpu = crate::task::current_hart_id();
    (cpu < LOCK_DEBUG_CPUS).then_some(cpu)
}

#[inline]
fn rank_slot(cpu: usize, depth: usize) -> usize {
    cpu * LOCK_DEBUG_DEPTH + depth
}

#[cfg(debug_assertions)]
fn lockdep_before_acquire(class: LockClass, name: &'static str) {
    let Some(cpu) = debug_cpu() else {
        return;
    };
    let depth = LOCK_DEPTH[cpu].load(Ordering::Relaxed);
    if depth == 0 {
        return;
    }
    let held = LOCK_RANKS[rank_slot(cpu, depth - 1)].load(Ordering::Relaxed);
    assert!(
        class.rank() >= held,
        "lock-order inversion on hart {}: acquire '{}' (rank {}) while holding rank {}",
        cpu,
        name,
        class.rank(),
        held,
    );
}

#[cfg(not(debug_assertions))]
#[inline]
fn lockdep_before_acquire(_: LockClass, _: &'static str) {}

#[cfg(debug_assertions)]
fn lockdep_acquired(class: LockClass, name: &'static str) {
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
    LOCK_RANKS[rank_slot(cpu, depth)].store(class.rank(), Ordering::Relaxed);
    LOCK_DEPTH[cpu].store(depth + 1, Ordering::Relaxed);
}

#[cfg(not(debug_assertions))]
#[inline]
fn lockdep_acquired(_: LockClass, _: &'static str) {}

#[cfg(debug_assertions)]
fn lockdep_released(class: LockClass, name: &'static str) {
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
    assert!(
        held == class.rank(),
        "lock '{}' released out of order on hart {} (top rank {}, releasing {})",
        name,
        cpu,
        held,
        class.rank(),
    );
    LOCK_RANKS[slot].store(0, Ordering::Relaxed);
    LOCK_DEPTH[cpu].store(depth - 1, Ordering::Relaxed);
}

#[cfg(not(debug_assertions))]
#[inline]
fn lockdep_released(_: LockClass, _: &'static str) {}

/// Panic in a debug build if the current CPU still holds a domain lock.
///
/// Scheduler and blocking paths call this before a context switch.  It is a
/// guard rail, not a replacement for releasing the guard lexically.
#[track_caller]
pub fn assert_no_spin_locks_held(context: &'static str) {
    #[cfg(debug_assertions)]
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
    fn check_not_recursive(&self) {
        #[cfg(debug_assertions)]
        if let Some(cpu) = debug_cpu() {
            assert!(
                self.owner.load(Ordering::Acquire) != cpu,
                "recursive acquisition of '{}' on hart {}",
                self.name,
                cpu,
            );
        }
    }

    #[inline]
    fn acquired(&self) {
        if let Some(cpu) = debug_cpu() {
            let previous = self.owner.swap(cpu, Ordering::AcqRel);
            debug_assert_eq!(previous, NO_OWNER, "lock '{}' owner corruption", self.name);
        }
        lockdep_acquired(self.class, self.name);
    }

    /// Acquire the lock, disabling local interrupts while it is held.
    #[track_caller]
    pub fn lock(&self) -> SpinNoIrqGuard<'_, T> {
        self.check_not_recursive();
        lockdep_before_acquire(self.class, self.name);
        let guard = self.inner.lock();
        self.acquired();
        SpinNoIrqGuard {
            lock: self,
            guard: Some(guard),
        }
    }

    /// Try to acquire the lock without waiting.
    pub fn try_lock(&self) -> Option<SpinNoIrqGuard<'_, T>> {
        #[cfg(debug_assertions)]
        if let Some(cpu) = debug_cpu() {
            if self.owner.load(Ordering::Acquire) == cpu {
                return None;
            }
        }
        lockdep_before_acquire(self.class, self.name);
        let guard = self.inner.try_lock()?;
        self.acquired();
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
        lockdep_released(self.lock.class, self.lock.name);
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

            pub fn try_lock(&self) -> Option<$guard_name<'_, T>> {
                self.0.try_lock()
            }

            pub fn try_borrow(&self) -> Option<$guard_name<'_, T>> {
                self.try_lock()
            }

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
define_domain_lock!(TaskLock, TaskGuard, LockClass::Task);
define_domain_lock!(VmLock, VmGuard, LockClass::Vm);
define_domain_lock!(VfsLock, VfsGuard, LockClass::Vfs);
define_domain_lock!(FsLock, FsGuard, LockClass::Fs);
define_domain_lock!(DeviceLock, DeviceGuard, LockClass::Device);
