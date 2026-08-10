// 这里负责操作系统的锁和同步：导出单核容器以及逐步迁移中的 SMP 锁域。
mod lock;
mod up;

#[allow(unused_imports)]
pub use lock::{
    assert_no_spin_locks_held, drain_deferred_lock_wakes, dump_lockdep_state, CpuLocalLock,
    DentryLock, DeviceLock, FilesLock, FrameAllocatorLock, FrameRefLock, FsAuxLock, FsGlobalGuard, FsGlobalLock,
    FsGlobalReadGuard, FsGuard, FsLock, FutexLock, PageCacheLock, PageCacheRwLock,
    PidRegistryLock, SchedulerLock, SignalActionsLock, SleepGuard, SleepLock, SpinNoIrq,
    TaskGuard, TaskLock, VfsLock, VmLock, VmPageTableRwLock, VmPagesGuard, VmPagesLock, VmRwLock,
};
#[cfg(any(feature = "smp-diagnostics", feature = "lock-stats"))]
pub use lock::dump_lock_contention_stats;
pub use up::UPSafeCell;
