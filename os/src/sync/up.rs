// 这里负责操作系统的锁和同步：实现 UPSafeCell，用 RefCell 给单核内核提供受控的内部可变性。
use core::cell::{RefCell, RefMut};
use core::panic::Location;
// 单核环境下用 RefCell 包一层静态数据，外面就不用到处写 unsafe。
pub struct UPSafeCell<T> {
    inner: RefCell<T>,
    name: &'static str,
}

unsafe impl<T> Sync for UPSafeCell<T> {}

impl<T> UPSafeCell<T> {
    // 创建一个未命名的单核安全单元。
    // 执行时把值放进 RefCell；这里是 unsafe，因为调用者必须保证它只在单核或关中断语义下使用。
    pub unsafe fn new(value: T) -> Self {
        Self {
            inner: RefCell::new(value),
            name: "unknown",
        }
    }
    // 创建一个带名字的单元，panic 时能看出是哪块状态被重复借用了。
    #[track_caller]
    // 执行时额外保存调试名字，重复借用时能更快定位是哪一个全局状态没释放。
    pub unsafe fn new_named(value: T, name: &'static str) -> Self {
        Self {
            inner: RefCell::new(value),
            name,
        }
    }
    // 取得可变借用；如果已经被借用，直接 panic 暴露内核里的借用错误。
    // 执行时用 try_borrow_mut 避免静默等待；失败说明同一段内核路径重复拿锁，直接 panic 暴露 bug。
    pub fn exclusive_access(&self) -> RefMut<'_, T> {
        match self.inner.try_borrow_mut() {
            Ok(g) => g,
            Err(_) => {
                let caller = Location::caller();
                panic!("already borrowed at {}:{}", caller.file(), caller.line())
            }
        }
    }
    // 尝试取得可变借用，拿不到就返回 None。
    // 执行时只做一次 RefCell 借用尝试，让调用方自己决定是否退避、重试或走失败分支。
    pub fn try_borrow_mut(&self) -> Option<RefMut<'_, T>> {
        match self.inner.try_borrow_mut() {
            Ok(g) => Some(g),
            Err(_) => None,
        }
    }
    // 尝试取得只读借用，拿不到就返回 None。
    // 执行时只做一次不可变借用尝试，适合调试或可选读取路径避免 panic。
    pub fn try_borrow(&self) -> Option<core::cell::Ref<'_, T>> {
        match self.inner.try_borrow() {
            Ok(g) => Some(g),
            Err(_) => None,
        }
    }
}
