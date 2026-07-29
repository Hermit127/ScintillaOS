use core::cell::{RefCell, RefMut};
use core::panic::Location;
use core::sync::atomic::Ordering;
/// Wrap a static data structure inside it so that we are
/// able to access it without any `unsafe`.
///
/// We should only use it in uniprocessor.
///
/// In order to get mutable reference of inner data, call
/// `exclusive_access`.
pub struct UPSafeCell<T> {
    inner: RefCell<T>,
    name: &'static str,
}

unsafe impl<T> Sync for UPSafeCell<T> {}

impl<T> UPSafeCell<T> {
    pub unsafe fn new(value: T) -> Self {
        Self {
            inner: RefCell::new(value),
            name: "unknown",
        }
    }
    pub unsafe fn new_named(value: T, name: &'static str) -> Self {
        Self {
            inner: RefCell::new(value),
            name,
        }
    }
    /// Panic if the data has been borrowed.
    #[track_caller]
    pub fn exclusive_access(&self) -> RefMut<'_, T> {
        match self.inner.try_borrow_mut() {
            Ok(g) => g,
            Err(_) => {
                let caller = Location::caller();
                panic!(
                    "UPSafeCell '{}' already borrowed at {}:{} last_syscall={} pid={} args=({:#x}, {:#x}, {:#x})",
                    self.name,
                    caller.file(),
                    caller.line(),
                    crate::syscall::LAST_SYSCALL_ID.load(Ordering::Relaxed),
                    crate::syscall::LAST_SYSCALL_PID.load(Ordering::Relaxed),
                    crate::syscall::LAST_SYSCALL_ARG0.load(Ordering::Relaxed),
                    crate::syscall::LAST_SYSCALL_ARG1.load(Ordering::Relaxed),
                    crate::syscall::LAST_SYSCALL_ARG2.load(Ordering::Relaxed),
                )
            }
        }
    }
    /// Try to borrow, returns None if already borrowed
    pub fn try_borrow_mut(&self) -> Option<RefMut<'_, T>> {
        match self.inner.try_borrow_mut() {
            Ok(g) => Some(g),
            Err(_) => None,
        }
    }
    pub fn try_borrow(&self) -> Option<core::cell::Ref<'_, T>> {
        match self.inner.try_borrow() {
            Ok(g) => Some(g),
            Err(_) => None,
        }
    }
}

