// 这里负责操作系统的进程管理：实现 pid 分配和 PidHandle 的自动回收。
use crate::sync::PidRegistryLock;
use alloc::vec::Vec;
use lazy_static::*;

struct PidAllocator {
    current: usize,
    recycled: Vec<usize>,
}

impl PidAllocator {
    // 初始化 pid 分配器，pid 从 1 开始发。
    // 执行时把 current 设为 1，并准备空的回收池，后续 alloc 会先发 pid 1。
    pub fn new() -> Self {
        PidAllocator {
            current: 1,
            recycled: Vec::new(),
        }
    }
    // 优先复用回收池里的 pid，否则顺着 current 往后分配。
    // 执行时先检查可用资源，再分配编号、页帧或缓冲区，最后记录状态并返回给调用者。
    pub fn alloc(&mut self) -> PidHandle {
        if let Some(pid) = self.recycled.pop() {
            PidHandle(pid)
        } else {
            self.current += 1;
            PidHandle(self.current - 1)
        }
    }
    // 回收一个 pid，并检查是否重复释放。
    // 执行时先检查可用资源，再分配编号、页帧或缓冲区，最后记录状态并返回给调用者。
    pub fn dealloc(&mut self, pid: usize) {
        assert!(pid < self.current);
        debug_assert!(
            !self.recycled.iter().any(|ppid| *ppid == pid),
            "pid {} has been deallocated!",
            pid
        );
        self.recycled.push(pid);
    }
}

lazy_static! {
    static ref PID_ALLOCATOR: PidRegistryLock<PidAllocator> =
        PidRegistryLock::new_named(PidAllocator::new(), "PidAllocator");
}

pub struct PidHandle(pub usize);

impl Drop for PidHandle {
    // PidHandle 生命周期结束时自动把 pid 放回分配器。
    // 执行时把自己持有的 pid 归还到全局分配器，避免任务退出后 pid 永久泄漏。
    fn drop(&mut self) {
        PID_ALLOCATOR.exclusive_access().dealloc(self.0);
    }
}

// 对外的 pid 分配入口。
// 执行时先检查可用资源，再分配编号、页帧或缓冲区，最后记录状态并返回给调用者。
pub fn pid_alloc() -> PidHandle {
    PID_ALLOCATOR.exclusive_access().alloc()
}
