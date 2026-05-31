use core::cell::{RefCell, RefMut};

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
    pub fn exclusive_access(&self) -> RefMut<'_, T> {
        self.inner.try_borrow_mut()
            .unwrap_or_else(|_| panic!("UPSafeCell[{}] already borrowed", self.name))
    }
}
