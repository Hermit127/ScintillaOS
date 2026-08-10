use core::ops::{Deref, DerefMut};
use spin::{
    Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockUpgradableGuard, RwLockWriteGuard,
};

use crate::components::irq::IRQ;

pub struct MutexNoIrq<T: ?Sized> {
    lock: Mutex<T>,
}

pub struct RwLockNoIrq<T: ?Sized> {
    lock: RwLock<T>,
}

/// Irq Status Struct.
/// This structure contains the status of the current IRQ
/// And it will restore irq status after dropping.
struct IrqStatus {
    irq_enabled: bool,
}

/// Restore the IRQ status when dropping
impl Drop for IrqStatus {
    fn drop(&mut self) {
        if self.irq_enabled {
            IRQ::int_enable();
        }
    }
}

/// Implement Sync for MutexNoIrq
unsafe impl<T: ?Sized + Send> Sync for MutexNoIrq<T> {}
/// Implement Send for MutexNoIrq
unsafe impl<T: ?Sized + Send> Send for MutexNoIrq<T> {}

unsafe impl<T: ?Sized + Send + Sync> Sync for RwLockNoIrq<T> {}
unsafe impl<T: ?Sized + Send + Sync> Send for RwLockNoIrq<T> {}

impl<T> MutexNoIrq<T> {
    pub const fn new(data: T) -> MutexNoIrq<T> {
        MutexNoIrq {
            lock: Mutex::new(data),
        }
    }

    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.lock.into_inner()
    }
}

impl<T: ?Sized> MutexNoIrq<T> {
    #[inline]
    pub fn lock(&self) -> MutexNoIrqGuard<T> {
        let mut backoff = 1usize;
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            for _ in 0..backoff {
                core::hint::spin_loop();
            }
            backoff = (backoff << 1).min(64);
        }
    }

    #[inline]
    pub fn is_locked(&self) -> bool {
        self.lock.is_locked()
    }

    /// Force unlock the mutex.
    ///
    /// # Safety
    ///
    /// Ensures that the mutex is not locked by any thread.
    pub unsafe fn force_unlock(&self) {
        self.lock.force_unlock()
    }

    #[inline]
    pub fn try_lock(&self) -> Option<MutexNoIrqGuard<T>> {
        if self.lock.is_locked() {
            return None;
        }
        let _irq_status = IrqStatus {
            irq_enabled: IRQ::int_enabled(),
        };
        IRQ::int_disable();
        self.lock
            .try_lock()
            .map(|guard| MutexNoIrqGuard { guard, _irq_status })
    }
}

impl<T> RwLockNoIrq<T> {
    pub const fn new(data: T) -> Self {
        Self {
            lock: RwLock::new(data),
        }
    }

    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.lock.into_inner()
    }
}

impl<T: ?Sized> RwLockNoIrq<T> {
    #[inline]
    pub fn read(&self) -> RwLockNoIrqReadGuard<'_, T> {
        let mut backoff = 1usize;
        loop {
            if let Some(guard) = self.try_read() {
                return guard;
            }
            for _ in 0..backoff {
                core::hint::spin_loop();
            }
            backoff = (backoff << 1).min(64);
        }
    }

    #[inline]
    pub fn write(&self) -> RwLockNoIrqWriteGuard<'_, T> {
        let mut backoff = 1usize;
        loop {
            if let Some(guard) = self.try_upgradeable_read() {
                return guard.upgrade();
            }
            for _ in 0..backoff {
                core::hint::spin_loop();
            }
            backoff = (backoff << 1).min(64);
        }
    }

    #[inline]
    pub fn try_read(&self) -> Option<RwLockNoIrqReadGuard<'_, T>> {
        let irq_status = IrqStatus {
            irq_enabled: IRQ::int_enabled(),
        };
        IRQ::int_disable();
        self.lock
            .try_read()
            .map(|guard| RwLockNoIrqReadGuard {
                guard,
                _irq_status: irq_status,
            })
    }

    #[inline]
    pub fn try_write(&self) -> Option<RwLockNoIrqWriteGuard<'_, T>> {
        let irq_status = IrqStatus {
            irq_enabled: IRQ::int_enabled(),
        };
        IRQ::int_disable();
        self.lock
            .try_write()
            .map(|guard| RwLockNoIrqWriteGuard {
                guard,
                _irq_status: irq_status,
            })
    }

    #[inline]
    pub fn try_upgradeable_read(&self) -> Option<RwLockNoIrqUpgradableGuard<'_, T>> {
        let irq_status = IrqStatus {
            irq_enabled: IRQ::int_enabled(),
        };
        IRQ::int_disable();
        self.lock
            .try_upgradeable_read()
            .map(|guard| RwLockNoIrqUpgradableGuard {
                guard,
                _irq_status: irq_status,
            })
    }
}

/// The Mutex Guard.
pub struct MutexNoIrqGuard<'a, T: ?Sized + 'a> {
    guard: MutexGuard<'a, T>,
    _irq_status: IrqStatus,
}

impl<'a, T: ?Sized> Deref for MutexNoIrqGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &(self.guard)
    }
}

impl<'a, T: ?Sized> DerefMut for MutexNoIrqGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut (self.guard)
    }
}

pub struct RwLockNoIrqReadGuard<'a, T: ?Sized + 'a> {
    guard: RwLockReadGuard<'a, T>,
    _irq_status: IrqStatus,
}

impl<'a, T: ?Sized> Deref for RwLockNoIrqReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

pub struct RwLockNoIrqWriteGuard<'a, T: ?Sized + 'a> {
    guard: RwLockWriteGuard<'a, T>,
    _irq_status: IrqStatus,
}

impl<'a, T: ?Sized> Deref for RwLockNoIrqWriteGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<'a, T: ?Sized> DerefMut for RwLockNoIrqWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

pub struct RwLockNoIrqUpgradableGuard<'a, T: ?Sized + 'a> {
    guard: RwLockUpgradableGuard<'a, T>,
    _irq_status: IrqStatus,
}

impl<'a, T: ?Sized> RwLockNoIrqUpgradableGuard<'a, T> {
    #[inline]
    pub fn upgrade(self) -> RwLockNoIrqWriteGuard<'a, T> {
        let Self {
            guard,
            _irq_status,
        } = self;
        RwLockNoIrqWriteGuard {
            guard: guard.upgrade(),
            _irq_status,
        }
    }
}

impl<'a, T: ?Sized> Deref for RwLockNoIrqUpgradableGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}
