use core::cell::{RefCell, RefMut};
use core::panic::Location;

/// Wrap a static data structure inside it so that we are
/// able to access it without any `unsafe`.
///
/// We should only use it in uniprocessor.
///
/// In order to get mutable reference of inner data, call
/// `exclusive_access`.
pub struct UPSafeCell<T> {
    /// inner data
    inner: RefCell<T>,
}

unsafe impl<T> Sync for UPSafeCell<T> {}

impl<T> UPSafeCell<T> {
    /// User is responsible to guarantee that inner struct is only used in
    /// uniprocessor.
    pub unsafe fn new(value: T) -> Self {
        Self {
            inner: RefCell::new(value),
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
                    "already borrowed at {}:{}",
                    caller.file(),
                    caller.line()
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
