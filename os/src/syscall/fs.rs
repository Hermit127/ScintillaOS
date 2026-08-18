use crate::fs::vfs::{
    read_dentry_symlink_bytes, vfs_clear_tmpfs_mount, vfs_create, vfs_dentry_is_readonly,
    vfs_lookup, vfs_resolve_path, vfs_resolve_with_fallback, vfs_set_readonly_mount,
    vfs_set_tmpfs_mount, vfs_tmpfs_can_resize, vfs_tmpfs_commit_resize, Dentry, VfsSuperBlock,
};
use crate::fs::{
    follow_symlink, link_at, make_pipe, make_unix_socket_pair, open_file_at, rename_at,
    symlink_at, DevBlock, DevNull, DevTty, DevZero, EpollEvent, EpollFile, EventFdFile, File,
    Kstat, OpenFlags, TimerFdFile, TimerFdSpec, UnixSocketKind, Urandom, EPOLL_MAX_INTERESTS,
    S_IFDIR, S_IFREG, TFD_NONBLOCK, TFD_TIMER_ABSTIME, TFD_TIMER_CANCEL_ON_SET,
    TIMERFD_CLOCK_BOOTTIME, TIMERFD_CLOCK_MONOTONIC, TIMERFD_CLOCK_REALTIME,
};
use crate::sync::{DeviceLock, TaskGuard, VfsLock};
use crate::task::{
    current_task, current_user_token, file_group_tasks_locked, has_interrupting_signal,
    pid2task, TaskControlBlock, TaskControlBlockInner, FILE_TABLE_LOCK,
};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicI32, Ordering};
use lazy_static::lazy_static;

const AT_FDCWD: isize = -100;
const O_NOATIME: u32 = 0x40000;
const O_CLOEXEC: u32 = 0x80000;
const O_TMPFILE: u32 = 0x410000;
const FILE_TIME_NOW_GUARD_SEC: i64 = 5;
const PATH_MAX: usize = 4096;
const NAME_MAX: usize = 255;
const EROFS: isize = -30;
const INLINE_IO_STAGING_SIZE: usize = 1024;
pub(crate) const MAX_IO_STAGING_SIZE: usize = 1024 * 1024;
const IO_STAGING_POOL_SIZE: usize = 16;

struct IoStagingPool {
    buffers: [Option<Vec<u8>>; IO_STAGING_POOL_SIZE],
}

impl IoStagingPool {
    const fn new() -> Self {
        Self {
            buffers: [const { None }; IO_STAGING_POOL_SIZE],
        }
    }

    fn take(&mut self, len: usize) -> Option<Vec<u8>> {
        let index = self
            .buffers
            .iter()
            .position(|buffer| buffer.as_ref().is_some_and(|buffer| buffer.capacity() >= len))
            .or_else(|| self.buffers.iter().position(Option::is_some))?;
        self.buffers[index].take()
    }

    fn put(&mut self, buffer: Vec<u8>) -> Result<(), Vec<u8>> {
        let Some(slot) = self.buffers.iter_mut().find(|slot| slot.is_none()) else {
            return Err(buffer);
        };
        *slot = Some(buffer);
        Ok(())
    }
}

static IO_STAGING_POOL: VfsLock<IoStagingPool> =
    VfsLock::new_named(IoStagingPool::new(), "VfsIoStagingPool");

fn recycle_io_staging(mut buffer: Vec<u8>) {
    buffer.clear();
    let retired = IO_STAGING_POOL.exclusive_access().put(buffer).err();
    drop(retired);
}

pub(crate) enum IoStaging {
    Inline {
        bytes: [u8; INLINE_IO_STAGING_SIZE],
        len: usize,
    },
    Heap(Vec<u8>),
}

impl IoStaging {
    pub(crate) fn zeroed(len: usize) -> Result<Self, isize> {
        if len <= INLINE_IO_STAGING_SIZE {
            return Ok(Self::Inline {
                bytes: [0; INLINE_IO_STAGING_SIZE],
                len,
            });
        }
        let mut bytes = IO_STAGING_POOL
            .exclusive_access()
            .take(len)
            .unwrap_or_default();
        if bytes.try_reserve_exact(len).is_err() {
            recycle_io_staging(bytes);
            return Err(-12);
        }
        bytes.resize(len, 0);
        Ok(Self::Heap(bytes))
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::Inline { bytes, len } => &bytes[..*len],
            Self::Heap(bytes) => bytes,
        }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Self::Inline { bytes, len } => &mut bytes[..*len],
            Self::Heap(bytes) => bytes,
        }
    }
}

impl Drop for IoStaging {
    fn drop(&mut self) {
        if let Self::Heap(bytes) = self {
            recycle_io_staging(core::mem::take(bytes));
        }
    }
}

fn map_dev_shm_path(path: &str) -> String {
    if let Some(name) = path.strip_prefix("/dev/shm/") {
        format!("/tmp/.shm_{}", name)
    } else {
        String::from(path)
    }
}

fn valid_user_write_buf(ptr: *mut u8, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if ptr.is_null() {
        return false;
    }
    let Some(task) = current_task() else {
        return false;
    };
    super::process::prepare_task_user_access(&task, ptr as usize, len, true)
}

fn valid_user_read_buf(ptr: *const u8, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if ptr.is_null() {
        return false;
    }
    let Some(task) = current_task() else {
        return false;
    };
    super::process::prepare_task_user_access(&task, ptr as usize, len, false)
}

pub(crate) fn copy_from_user_for_io(
    task: &Arc<crate::task::TaskControlBlock>,
    _token: polyhal::pagetable::PageTable,
    buf: *mut u8,
    len: usize,
) -> Result<IoStaging, isize> {
    if len == 0 {
        return IoStaging::zeroed(0);
    }
    // A userspace read/write count is not a kernel allocation contract.
    // Bound the bounce buffer and report a legal short I/O; callers can retry
    // the remainder just as they do for Linux short reads and writes.
    let staging_len = len.min(MAX_IO_STAGING_SIZE);
    let mut staging = IoStaging::zeroed(staging_len)?;
    if !super::process::copy_from_task_user(task, buf as usize, staging.as_mut_slice()) {
        return Err(-14);
    }
    Ok(staging)
}

pub(crate) fn copy_to_user_after_io(
    task: &Arc<crate::task::TaskControlBlock>,
    _token: polyhal::pagetable::PageTable,
    buf: *mut u8,
    len: usize,
) -> Result<IoStaging, isize> {
    if len == 0 {
        return IoStaging::zeroed(0);
    }
    let staging_len = len.min(MAX_IO_STAGING_SIZE);
    if !super::process::prepare_task_user_access(task, buf as usize, staging_len, true) {
        return Err(-14);
    }
    IoStaging::zeroed(staging_len)
}

pub(crate) fn copy_staged_output_to_user(
    task: &Arc<crate::task::TaskControlBlock>,
    _token: polyhal::pagetable::PageTable,
    buf: *mut u8,
    data: &[u8],
) -> Result<(), isize> {
    if data.is_empty() {
        return Ok(());
    }
    if !super::process::copy_to_task_user(task, buf as usize, data) {
        return Err(-14);
    }
    Ok(())
}

fn fd_path_for_proc(fd: usize) -> Option<String> {
    let task = current_task()?;
    let inner = task.inner_exclusive_access();
    let file = inner.fd_table.get(fd)?.as_ref()?;
    let raw_path = file.path()?;
    let root_path = inner.root.path.clone();
    if root_path != "/" && raw_path.starts_with(&root_path) {
        let suffix = &raw_path[root_path.len()..];
        if suffix.is_empty() {
            Some(String::from("/"))
        } else if suffix.starts_with('/') {
            Some(String::from(suffix))
        } else {
            Some(format!("/{}", suffix))
        }
    } else {
        Some(raw_path)
    }
}

fn proc_fd_readlink(path: &str) -> Option<String> {
    let rest = if let Some(rest) = path.strip_prefix("/proc/self/fd/") {
        rest
    } else if let Some(rest) = path.strip_prefix("/proc/") {
        let (pid, tail) = rest.split_once('/')?;
        if tail.strip_prefix("fd/").is_none() {
            return None;
        }
        let current_pid = current_task().map(|task| task.getpid())?;
        if pid.parse::<usize>().ok()? != current_pid {
            return None;
        }
        tail.strip_prefix("fd/")?
    } else {
        return None;
    };
    let fd = rest.parse::<usize>().ok()?;
    fd_path_for_proc(fd)
}

fn proc_exe_task(path: &str) -> Option<Arc<crate::task::TaskControlBlock>> {
    let path = path.trim_end_matches('/');
    if path == "/proc/self/exe" {
        return current_task();
    }
    let pid = path
        .strip_prefix("/proc/")?
        .strip_suffix("/exe")?
        .parse::<usize>()
        .ok()?;
    pid2task(pid)
}

fn proc_exe_readlink(path: &str) -> Option<String> {
    let task = proc_exe_task(path)?;
    let inner = task.inner_exclusive_access();
    if inner.executable_path.is_empty() {
        None
    } else {
        Some(inner.executable_path.clone())
    }
}

fn open_proc_executable(path: &str) -> Option<Arc<dyn File + Send + Sync>> {
    let task = proc_exe_task(path)?;
    let (root, executable_path) = {
        let inner = task.inner_exclusive_access();
        (inner.root.clone(), inner.executable_path.clone())
    };
    if executable_path.is_empty() {
        return None;
    }
    let relative_path = executable_path.strip_prefix('/').unwrap_or(&executable_path);
    let file = open_file_at(Some(root), relative_path, OpenFlags::RDONLY, 0)?;
    let file: Arc<dyn File + Send + Sync> = file;
    Some(file)
}

fn proc_exe_stat(path: &str, nofollow: bool) -> Option<Kstat> {
    let target = proc_exe_readlink(path)?;
    if nofollow {
        let mut stat = Kstat::new(crate::fs::S_IFLNK | 0o777);
        stat.st_size = target.len() as i64;
        return Some(stat);
    }
    open_proc_executable(path).map(|file| file.stat())
}

fn shared_fd_limit(inners: &[TaskGuard<'_, TaskControlBlockInner>]) -> usize {
    inners
        .iter()
        .map(|inner| inner.rlimit_nofile.min(1024))
        .min()
        .unwrap_or(0)
}

fn first_free_shared_fd(
    inners: &[TaskGuard<'_, TaskControlBlockInner>],
    start: usize,
    limit: usize,
) -> Option<usize> {
    (start..limit).find(|&fd| {
        inners
            .iter()
            .all(|inner| fd >= inner.fd_table.len() || inner.fd_table[fd].is_none())
    })
}

struct LockedFileGroup<'a> {
    inners: Vec<TaskGuard<'a, TaskControlBlockInner>>,
}

impl<'a> LockedFileGroup<'a> {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            inners: Vec::with_capacity(capacity),
        }
    }

    fn push(&mut self, inner: TaskGuard<'a, TaskControlBlockInner>) {
        self.inners.push(inner);
    }
}

impl<'a> Deref for LockedFileGroup<'a> {
    type Target = [TaskGuard<'a, TaskControlBlockInner>];

    fn deref(&self) -> &Self::Target {
        &self.inners
    }
}

impl DerefMut for LockedFileGroup<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inners
    }
}

impl Drop for LockedFileGroup<'_> {
    fn drop(&mut self) {
        while self.inners.pop().is_some() {}
    }
}

fn lock_file_group<'a>(tasks: &'a [Arc<TaskControlBlock>]) -> LockedFileGroup<'a> {
    let mut inners = LockedFileGroup::with_capacity(tasks.len());
    for task in tasks {
        inners.push(task.inner_exclusive_access());
    }
    inners
}

fn prepare_file_group(
    state: &mut crate::task::FileTableState,
    task: &Arc<TaskControlBlock>,
    files_id: usize,
) -> Vec<Arc<TaskControlBlock>> {
    let mut tasks = file_group_tasks_locked(state, task, files_id);
    // TaskLock uses its address as the same-class lock order key. TaskInner is
    // at a fixed offset inside every TCB, so sorting the owning Arc pointers
    // gives the same stable order before the guards are acquired.
    tasks.sort_unstable_by_key(|task| Arc::as_ptr(task) as usize);
    tasks
}

fn ensure_fd_slot(inner: &mut crate::task::TaskControlBlockInner, fd: usize) {
    while inner.fd_table.len() <= fd {
        inner.fd_table.push(None);
        inner.fd_cloexec.push(false);
    }
}

/// Install one descriptor into every task sharing the same Linux files_struct.
/// Each per-task mirror owns one File reference, so all mirrors are updated
/// under FILE_TABLE_LOCK before another thread can reuse the descriptor number.
pub(crate) fn install_fd_shared(
    task: &Arc<TaskControlBlock>,
    file: Arc<dyn File + Send + Sync>,
    cloexec: bool,
) -> Result<usize, isize> {
    let files_id = task.inner_exclusive_access().files_id;
    let mut files = FILE_TABLE_LOCK.lock();
    let tasks = prepare_file_group(&mut files, task, files_id);
    let mut inners = lock_file_group(&tasks);
    let limit = shared_fd_limit(&inners);
    let Some(fd) = first_free_shared_fd(&inners, 0, limit) else {
        return Err(-24);
    };
    for _ in 1..inners.len() {
        file.dup_ref();
    }
    for inner in inners.iter_mut() {
        ensure_fd_slot(inner, fd);
        debug_assert!(inner.fd_table[fd].is_none());
        inner.fd_table[fd] = Some(file.clone());
        inner.fd_cloexec[fd] = cloexec;
    }
    Ok(fd)
}

fn install_fd(
    task: &Arc<crate::task::TaskControlBlock>,
    file: Arc<dyn File + Send + Sync>,
    cloexec: bool,
) -> Result<usize, isize> {
    install_fd_shared(task, file, cloexec)
}

fn close_fd_shared(
    task: &Arc<TaskControlBlock>,
    fd: usize,
) -> Result<bool, isize> {
    let files_id = task.inner_exclusive_access().files_id;
    let mut file_state = FILE_TABLE_LOCK.lock();
    let tasks = prepare_file_group(&mut file_state, task, files_id);
    let mut inners = lock_file_group(&tasks);
    let task_index = tasks
        .iter()
        .position(|candidate| Arc::ptr_eq(candidate, task))
        .expect("current task missing from shared file group");
    let Some(file) = inners[task_index]
        .fd_table
        .get(fd)
        .and_then(|file| file.as_ref())
    else {
        return Err(-9);
    };
    let is_unix_socket = file.as_any().is::<crate::fs::UnixSocketFile>();
    let mut closed_files = Vec::new();
    for inner in inners.iter_mut() {
        if let Some(file) = inner.take_fd_for_close(fd) {
            closed_files.push(file);
        }
    }
    drop(inners);
    drop(file_state);
    for file in closed_files {
        file.close_ref();
    }
    Ok(is_unix_socket)
}

fn duplicate_fd_shared(
    task: &Arc<TaskControlBlock>,
    fd: usize,
    minimum: usize,
    cloexec: bool,
) -> Result<usize, isize> {
    let files_id = task.inner_exclusive_access().files_id;
    let mut files = FILE_TABLE_LOCK.lock();
    let tasks = prepare_file_group(&mut files, task, files_id);
    let mut inners = lock_file_group(&tasks);
    let task_index = tasks
        .iter()
        .position(|candidate| Arc::ptr_eq(candidate, task))
        .expect("current task missing from shared file group");
    let source = inners[task_index]
        .fd_table
        .get(fd)
        .and_then(|file| file.as_ref())
        .cloned()
        .ok_or(-9isize)?;
    let limit = shared_fd_limit(&inners);
    if minimum >= limit {
        return Err(-22);
    }
    let Some(new_fd) = first_free_shared_fd(&inners, minimum, limit) else {
        return Err(-24);
    };
    for _ in 0..inners.len() {
        source.dup_ref();
    }
    for inner in inners.iter_mut() {
        ensure_fd_slot(inner, new_fd);
        debug_assert!(inner.fd_table[new_fd].is_none());
        inner.fd_table[new_fd] = Some(source.clone());
        inner.fd_cloexec[new_fd] = cloexec;
    }
    Ok(new_fd)
}

fn replace_fd_shared(
    task: &Arc<TaskControlBlock>,
    fd: usize,
    new_fd: usize,
    cloexec: bool,
) -> Result<usize, isize> {
    if fd == new_fd {
        return Ok(new_fd);
    }
    let files_id = task.inner_exclusive_access().files_id;
    let mut files = FILE_TABLE_LOCK.lock();
    let tasks = prepare_file_group(&mut files, task, files_id);
    let mut inners = lock_file_group(&tasks);
    let task_index = tasks
        .iter()
        .position(|candidate| Arc::ptr_eq(candidate, task))
        .expect("current task missing from shared file group");
    let source = inners[task_index]
        .fd_table
        .get(fd)
        .and_then(|file| file.as_ref())
        .cloned()
        .ok_or(-9isize)?;
    if new_fd >= shared_fd_limit(&inners) {
        return Err(-9);
    }
    for _ in 0..inners.len() {
        source.dup_ref();
    }
    let mut replaced = Vec::new();
    for inner in inners.iter_mut() {
        ensure_fd_slot(inner, new_fd);
        if let Some(file) = inner.take_fd_for_close(new_fd) {
            replaced.push(file);
        }
        inner.fd_table[new_fd] = Some(source.clone());
        inner.fd_cloexec[new_fd] = cloexec;
    }
    drop(inners);
    drop(files);
    for file in replaced {
        file.close_ref();
    }
    Ok(new_fd)
}

fn set_fd_cloexec_shared(
    task: &Arc<TaskControlBlock>,
    fd: usize,
    cloexec: bool,
) -> Result<(), isize> {
    let files_id = task.inner_exclusive_access().files_id;
    let mut files = FILE_TABLE_LOCK.lock();
    let tasks = prepare_file_group(&mut files, task, files_id);
    let mut inners = lock_file_group(&tasks);
    let task_index = tasks
        .iter()
        .position(|candidate| Arc::ptr_eq(candidate, task))
        .expect("current task missing from shared file group");
    if inners[task_index]
        .fd_table
        .get(fd)
        .and_then(|file| file.as_ref())
        .is_none()
    {
        return Err(-9);
    }
    for inner in inners.iter_mut() {
        if fd < inner.fd_table.len() && inner.fd_table[fd].is_some() {
            inner.fd_cloexec[fd] = cloexec;
        }
    }
    Ok(())
}

// RISC-V and LoongArch use the naturally aligned Linux UAPI layout:
// u32 events, four bytes of padding, then u64 data.
const EPOLL_EVENT_DATA_OFFSET: usize = 8;
const EPOLL_EVENT_SIZE: usize = 16;
const EPOLL_CLOEXEC: usize = 0x80000;
const EPOLL_CTL_ADD: usize = 1;
const EPOLL_CTL_DEL: usize = 2;
const EPOLL_CTL_MOD: usize = 3;

fn read_epoll_event(event: *const u8) -> Result<EpollEvent, isize> {
    if !valid_user_read_buf(event, EPOLL_EVENT_SIZE) {
        return Err(-14);
    }
    let task = current_task().ok_or(-3isize)?;
    let mut bytes = [0u8; EPOLL_EVENT_SIZE];
    if !super::process::copy_from_task_user(&task, event as usize, &mut bytes) {
        return Err(-14);
    }
    Ok(EpollEvent {
        events: u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        data: u64::from_ne_bytes([
            bytes[EPOLL_EVENT_DATA_OFFSET],
            bytes[EPOLL_EVENT_DATA_OFFSET + 1],
            bytes[EPOLL_EVENT_DATA_OFFSET + 2],
            bytes[EPOLL_EVENT_DATA_OFFSET + 3],
            bytes[EPOLL_EVENT_DATA_OFFSET + 4],
            bytes[EPOLL_EVENT_DATA_OFFSET + 5],
            bytes[EPOLL_EVENT_DATA_OFFSET + 6],
            bytes[EPOLL_EVENT_DATA_OFFSET + 7],
        ]),
    })
}

fn encode_epoll_events(events: &[EpollEvent]) -> Result<Vec<u8>, isize> {
    let len = events
        .len()
        .checked_mul(EPOLL_EVENT_SIZE)
        .ok_or(-22isize)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(|_| -12isize)?;
    for event in events {
        bytes.extend_from_slice(&event.events.to_ne_bytes());
        bytes.extend_from_slice(&[0; EPOLL_EVENT_DATA_OFFSET - core::mem::size_of::<u32>()]);
        bytes.extend_from_slice(&event.data.to_ne_bytes());
    }
    Ok(bytes)
}

fn epoll_fd_for_current_task(fd: usize) -> Result<Arc<dyn File + Send + Sync>, isize> {
    let task = current_task().ok_or(-3isize)?;
    let inner = task.inner_exclusive_access();
    inner
        .fd_table
        .get(fd)
        .and_then(|file| file.as_ref())
        .cloned()
        .ok_or(-9)
}

pub fn sys_eventfd2(initval: u32, flags: usize) -> isize {
    const EFD_SEMAPHORE: usize = 0x1;
    const EFD_NONBLOCK: usize = 0x800;
    const EFD_CLOEXEC: usize = 0x80000;
    if flags & !(EFD_SEMAPHORE | EFD_NONBLOCK | EFD_CLOEXEC) != 0 {
        return -22;
    }
    let task = current_task().unwrap();
    let file: Arc<dyn File + Send + Sync> = Arc::new(EventFdFile::new(initval, flags));
    install_fd_shared(&task, file, flags & EFD_CLOEXEC != 0)
        .map(|fd| fd as isize)
        .unwrap_or_else(|errno| errno)
}

const TFD_CLOEXEC: usize = 0x80000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TimerFdTimeSpec {
    tv_sec: isize,
    tv_nsec: isize,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TimerFdITimerSpec {
    it_interval: TimerFdTimeSpec,
    it_value: TimerFdTimeSpec,
}

fn timerfd_timespec_to_ns(value: TimerFdTimeSpec) -> Result<u64, isize> {
    if value.tv_sec < 0 || value.tv_nsec < 0 || value.tv_nsec >= 1_000_000_000 {
        return Err(-22);
    }
    (value.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(value.tv_nsec as u64))
        .ok_or(-22)
}

fn timerfd_timespec_from_ns(value: u64) -> TimerFdTimeSpec {
    TimerFdTimeSpec {
        tv_sec: (value / 1_000_000_000).min(isize::MAX as u64) as isize,
        tv_nsec: (value % 1_000_000_000) as isize,
    }
}

fn timerfd_file_for_current_task(fd: usize) -> Result<Arc<dyn File + Send + Sync>, isize> {
    let file = epoll_fd_for_current_task(fd)?;
    if file.as_any().is::<TimerFdFile>() {
        Ok(file)
    } else {
        Err(-22)
    }
}

pub fn sys_timerfd_create(clock_id: i32, flags: usize) -> isize {
    if !matches!(
        clock_id,
        TIMERFD_CLOCK_REALTIME | TIMERFD_CLOCK_MONOTONIC | TIMERFD_CLOCK_BOOTTIME
    ) || flags & !(TFD_NONBLOCK | TFD_CLOEXEC) != 0
    {
        return -22;
    }
    let task = current_task().unwrap();
    let timerfd = Arc::new(TimerFdFile::new(clock_id, flags));
    TimerFdFile::register(&timerfd);
    let file: Arc<dyn File + Send + Sync> = timerfd;
    install_fd_shared(&task, file, flags & TFD_CLOEXEC != 0)
        .map(|fd| fd as isize)
        .unwrap_or_else(|errno| errno)
}

pub fn sys_timerfd_settime(
    fd: usize,
    flags: usize,
    new_value: *const u8,
    old_value: *mut u8,
) -> isize {
    if flags & !(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) != 0 {
        return -22;
    }
    if flags & TFD_TIMER_CANCEL_ON_SET != 0 && flags & TFD_TIMER_ABSTIME == 0 {
        return -22;
    }
    if new_value.is_null() {
        return -14;
    }
    let task = current_task().unwrap();
    let Some(new_spec) = super::process::copy_user_value(
        &task,
        new_value as *const TimerFdITimerSpec,
    ) else {
        return -14;
    };
    let Ok(value_ns) = timerfd_timespec_to_ns(new_spec.it_value) else {
        return -22;
    };
    let Ok(interval_ns) = timerfd_timespec_to_ns(new_spec.it_interval) else {
        return -22;
    };
    let file = match timerfd_file_for_current_task(fd) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    let timerfd = file
        .as_any()
        .downcast_ref::<TimerFdFile>()
        .expect("timerfd type changed after validation");
    if flags & TFD_TIMER_CANCEL_ON_SET != 0 && timerfd.clock_id() != TIMERFD_CLOCK_REALTIME {
        return -22;
    }
    let old_spec = timerfd.set_time(
        flags,
        TimerFdSpec {
            interval_ns,
            value_ns,
        },
    );
    if !old_value.is_null() {
        let old_user = TimerFdITimerSpec {
            it_interval: timerfd_timespec_from_ns(old_spec.interval_ns),
            it_value: timerfd_timespec_from_ns(old_spec.value_ns),
        };
        if !super::process::write_user_value(
            &task,
            old_value as *mut TimerFdITimerSpec,
            &old_user,
        ) {
            return -14;
        }
    }
    0
}

pub fn sys_timerfd_gettime(fd: usize, current_value: *mut u8) -> isize {
    if current_value.is_null() {
        return -14;
    }
    let task = current_task().unwrap();
    let file = match timerfd_file_for_current_task(fd) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    let timerfd = file
        .as_any()
        .downcast_ref::<TimerFdFile>()
        .expect("timerfd type changed after validation");
    let spec = timerfd.get_time();
    let user_spec = TimerFdITimerSpec {
        it_interval: timerfd_timespec_from_ns(spec.interval_ns),
        it_value: timerfd_timespec_from_ns(spec.value_ns),
    };
    if super::process::write_user_value(
        &task,
        current_value as *mut TimerFdITimerSpec,
        &user_spec,
    ) {
        0
    } else {
        -14
    }
}

pub fn sys_epoll_create1(flags: usize) -> isize {
    if flags & !EPOLL_CLOEXEC != 0 {
        return -22;
    }
    let epoll = match EpollFile::new() {
        Ok(epoll) => epoll,
        Err(errno) => return errno,
    };
    let task = current_task().unwrap();
    let file: Arc<dyn File + Send + Sync> = Arc::new(epoll);
    install_fd_shared(&task, file, flags & EPOLL_CLOEXEC != 0)
        .map(|fd| fd as isize)
        .unwrap_or_else(|errno| errno)
}

pub fn sys_epoll_ctl(epfd: usize, op: usize, fd: usize, event: *const u8) -> isize {
    if !matches!(op, EPOLL_CTL_ADD | EPOLL_CTL_DEL | EPOLL_CTL_MOD) {
        return -22;
    }
    let epoll_file = match epoll_fd_for_current_task(epfd) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    let watched_file = match epoll_fd_for_current_task(fd) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    if Arc::ptr_eq(&epoll_file, &watched_file) {
        return -22;
    }
    let Some(epoll) = epoll_file.as_any().downcast_ref::<EpollFile>() else {
        return -22;
    };

    match op {
        EPOLL_CTL_ADD => match read_epoll_event(event) {
            Ok(event) => epoll.add(fd, watched_file, event).map_or_else(|errno| errno, |_| 0),
            Err(errno) => errno,
        },
        EPOLL_CTL_DEL => epoll
            .remove(fd, &watched_file)
            .map_or_else(|errno| errno, |_| 0),
        EPOLL_CTL_MOD => match read_epoll_event(event) {
            Ok(event) => epoll
                .modify(fd, &watched_file, event)
                .map_or_else(|errno| errno, |_| 0),
            Err(errno) => errno,
        },
        _ => unreachable!(),
    }
}

pub fn sys_epoll_pwait(
    epfd: usize,
    events: *mut u8,
    maxevents: usize,
    timeout_ms: isize,
    sigmask: *const u8,
    sigsetsize: usize,
) -> isize {
    if maxevents == 0 || maxevents > EPOLL_MAX_INTERESTS || timeout_ms < -1 {
        return -22;
    }
    let Some(output_len) = maxevents.checked_mul(EPOLL_EVENT_SIZE) else {
        return -22;
    };
    if !valid_user_write_buf(events, output_len) {
        return -14;
    }
    if !sigmask.is_null() && sigsetsize != core::mem::size_of::<u64>() {
        return -22;
    }

    let task = current_task().unwrap();
    let epoll_file = match epoll_fd_for_current_task(epfd) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    let Some(epoll) = epoll_file.as_any().downcast_ref::<EpollFile>() else {
        return -22;
    };

    let saved_mask = if sigmask.is_null() {
        None
    } else {
        if !valid_user_read_buf(sigmask, core::mem::size_of::<u64>()) {
            return -14;
        }
        let mut raw = [0u8; core::mem::size_of::<u64>()];
        if !super::process::copy_from_task_user(&task, sigmask as usize, &mut raw) {
            return -14;
        }
        let raw = u64::from_ne_bytes(raw);
        let mut mask = super::process::user_sigset_to_flags(raw);
        mask.remove(crate::task::SignalFlags::SIGKILL | crate::task::SignalFlags::SIGSTOP);
        let mut inner = task.inner_exclusive_access();
        let saved = inner.signal_mask;
        inner.signal_mask = mask;
        Some(saved)
    };

    let result = (|| {
        let deadline_ns = if timeout_ms > 0 {
            let timeout_ns = (timeout_ms as usize).saturating_mul(1_000_000);
            Some(
                (polyhal::time::Time::now().to_nsec() as usize).saturating_add(timeout_ns),
            )
        } else {
            None
        };
        loop {
            crate::task::check_itimers();
            crate::net::net_poll();
            if has_interrupting_signal(&task.inner_exclusive_access()) {
                return -4;
            }

            let ready = match epoll.collect_ready(maxevents) {
                Ok(ready) => ready,
                Err(errno) => return errno,
            };
            if !ready.is_empty() {
                let bytes = match encode_epoll_events(&ready) {
                    Ok(bytes) => bytes,
                    Err(errno) => return errno,
                };
                let token = current_user_token();
                return match copy_staged_output_to_user(&task, token, events, &bytes) {
                    Ok(()) => ready.len() as isize,
                    Err(errno) => errno,
                };
            }
            if timeout_ms == 0 {
                return 0;
            }
            if let Some(deadline_ns) = deadline_ns {
                if (polyhal::time::Time::now().to_nsec() as usize) >= deadline_ns {
                    return 0;
                }
            }

            let should_block = || {
                if has_interrupting_signal(&task.inner_exclusive_access()) {
                    return false;
                }
                if let Some(deadline_ns) = deadline_ns {
                    if (polyhal::time::Time::now().to_nsec() as usize) >= deadline_ns {
                        return false;
                    }
                }
                !epoll.has_ready()
            };
            if let Some(deadline_ns) = deadline_ns {
                crate::task::block_current_until_if(
                    crate::task::BlockReason::PollWait,
                    deadline_ns,
                    should_block,
                );
            } else {
                crate::task::block_current_and_run_next_if(
                    crate::task::BlockReason::PollWait,
                    should_block,
                );
            }
        }
    })();

    if let Some(saved) = saved_mask {
        task.inner_exclusive_access().signal_mask = saved;
    }
    result
}

fn translated_path_bounded(path: *const u8) -> Result<String, isize> {
    if path.is_null() {
        return Err(-14);
    }

    let task = current_task().unwrap();
    let mut bytes = Vec::new();
    let mut page_bytes = [0u8; crate::config::PAGE_SIZE];

    while bytes.len() < PATH_MAX {
        let offset = bytes.len();
        let Some(addr) = (path as usize).checked_add(offset) else {
            return Err(-14);
        };
        let page_offset = addr & (crate::config::PAGE_SIZE - 1);
        let page_left = crate::config::PAGE_SIZE - page_offset;
        let check_len = page_left.min(PATH_MAX - offset);

        let chunk = &mut page_bytes[..check_len];
        if !super::process::copy_from_task_user(&task, addr, chunk) {
            return Err(-14);
        }
        if let Some(nul_pos) = chunk.iter().position(|&b| b == 0) {
            bytes.extend_from_slice(&chunk[..nul_pos]);
            let s = unsafe { core::str::from_utf8_unchecked(&bytes) };
            return Ok(String::from(s));
        }
        bytes.extend_from_slice(chunk);
    }

    Err(-36)
}

fn path_has_too_long_component(path: &str) -> bool {
    path.split('/')
        .any(|component| component.as_bytes().len() > NAME_MAX)
}

fn resolve_base_dir(dirfd: isize) -> Option<Arc<Dentry>> {
    let task = current_task()?;
    if dirfd == AT_FDCWD {
        let inner = task.inner_exclusive_access();
        return Some(inner.cwd.clone());
    }
    if dirfd < 0 {
        return None;
    }
    let inner = task.inner_exclusive_access();
    let dirfd = dirfd as usize;
    if dirfd >= inner.fd_table.len() {
        return None;
    }
    let Some(file) = &inner.fd_table[dirfd] else {
        return None;
    };
    if !file.is_dir() {
        return None;
    }
    if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        return Some(inode.get_dentry().clone());
    }
    let path = file.path()?;
    let cwd = inner.cwd.clone();
    drop(inner);
    vfs_resolve_path(&cwd, &path)
}

fn resolve_base_dir_for_path(dirfd: isize, path: &str) -> Result<Arc<Dentry>, isize> {
    if path.starts_with('/') {
        return Ok(current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone()));
    }
    let Some(task) = current_task() else {
        return Err(-9);
    };
    if dirfd == AT_FDCWD {
        return Ok(task.inner_exclusive_access().cwd.clone());
    }
    if dirfd < 0 {
        return Err(-9);
    }
    let dentry = {
        let inner = task.inner_exclusive_access();
        let fd = dirfd as usize;
        if fd >= inner.fd_table.len() {
            return Err(-9);
        }
        let Some(file) = &inner.fd_table[fd] else {
            return Err(-9);
        };
        if !file.is_dir() {
            return Err(-20);
        }
        let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() else {
            return Err(-20);
        };
        inode.get_dentry().clone()
    };
    if dentry_still_exists(&dentry) {
        Ok(dentry)
    } else {
        Err(-2)
    }
}

fn dentry_still_exists(dentry: &Arc<Dentry>) -> bool {
    if dentry.parent_epoch_is_current() {
        return true;
    }
    let validation_epoch = dentry.parent_epoch_for_revalidation();
    let exists = dentry
        .super_block
        .get_inode_info(&dentry.fs_path)
        .map(|info| info.inode_num == dentry.inode_num)
        .unwrap_or(false);
    if exists {
        if let Some(epoch) = validation_epoch {
            dentry.mark_parent_epoch_validated(epoch);
        }
    }
    exists
}

fn resolve_search_dir(current: Arc<Dentry>) -> Result<Arc<Dentry>, isize> {
    if current.is_dir {
        return Ok(current);
    }
    if current.is_link {
        let resolved = follow_symlink(&current, current.clone(), 0).ok_or(-40isize)?;
        if resolved.is_dir {
            return Ok(resolved);
        }
    }
    Err(-20)
}

fn resolve_parent_and_name_checked(
    start: &Arc<Dentry>,
    path: &str,
) -> Result<(Arc<Dentry>, String), isize> {
    let path = path.trim_matches('\0');
    if path.is_empty() {
        return Err(-2);
    }

    let mut current = if path.starts_with('/') {
        current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone())
    } else {
        start.clone()
    };

    let mut components = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".");
    let mut component = components.next().ok_or(-2isize)?;
    for next_component in components {
        current = resolve_search_dir(current)?;
        let next = vfs_lookup(&current, component).ok_or(-2isize)?;
        current = if next.is_link {
            follow_symlink(&current, next, 0).ok_or(-40isize)?
        } else {
            next
        };
        component = next_component;
    }

    current = resolve_search_dir(current)?;

    Ok((current, String::from(component)))
}

fn resolve_path_checked(start: &Arc<Dentry>, path: &str) -> Result<Arc<Dentry>, isize> {
    let path = path.trim_matches('\0');
    if path.is_empty() {
        return Err(-2);
    }

    let mut current = if path.starts_with('/') {
        current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone())
    } else {
        start.clone()
    };

    let mut components = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .peekable();

    while let Some(component) = components.next() {
        current = resolve_search_dir(current)?;
        let next = vfs_lookup(&current, component).ok_or(-2isize)?;
        current = if components.peek().is_some() && next.is_link {
            follow_symlink(&current, next, 0).ok_or(-40isize)?
        } else {
            next
        };
    }

    Ok(current)
}

pub fn sys_write(fd: usize, buf: *mut u8, len: usize) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if file.is_dir() {
            return -21;
        }
        if !file.writable() {
            return -9;
        }
        let file = file.clone();
        if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
            if vfs_dentry_is_readonly(inode.get_dentry()) {
                return EROFS;
            }
        }
        drop(inner);
        let complete_regular_io = file
            .as_any()
            .downcast_ref::<crate::fs::inode::OSInode>()
            .is_some();
        let mut total = 0usize;
        loop {
            let remaining = len - total;
            let chunk_len = remaining.min(MAX_IO_STAGING_SIZE);
            let Some(chunk_addr) = (buf as usize).checked_add(total) else {
                return if total == 0 { -14 } else { total as isize };
            };
            // User memory is copied before entering the file layer so a page
            // fault cannot re-enter TaskLock beneath a filesystem lock.
            let mut staging = match copy_from_user_for_io(
                &task,
                token,
                chunk_addr as *mut u8,
                chunk_len,
            ) {
                Ok(staging) => staging,
                Err(errno) => {
                    #[cfg(any(
                        feature = "buildstorm-diagnostics",
                        feature = "buildstorm-mm-diagnostics"
                    ))]
                    super::process::diagnose_task_user_access(
                        &task,
                        chunk_addr,
                        chunk_len,
                        false,
                        "write-copy-in",
                    );
                    return if total == 0 { errno } else { total as isize };
                }
            };
            let staged_len = staging.as_slice().len();
            let written = file.write(staging.as_mut_slice());
            if written < 0 {
                return if total == 0 { written } else { total as isize };
            }
            let written = written as usize;
            if written > staged_len {
                return if total == 0 { -5 } else { total as isize };
            }
            total += written;
            if written < staged_len || !complete_regular_io || total == len {
                return total as isize;
            }
        }
    } else {
        -1
    }
}

pub fn sys_read(fd: usize, buf: *mut u8, len: usize) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        #[cfg(feature = "smp-diagnostics")]
        crate::println!(
            "[socketpair-diag] read-ebadf tid={} tgid={} fd={} table_len={}",
            task.getpid(),
            inner.tgid,
            fd,
            inner.fd_table.len(),
        );
        return -9;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if file.is_dir() {
            return -21;
        }
        if !file.readable() {
            return -9;
        }
        drop(inner);
        let complete_regular_io = file
            .as_any()
            .downcast_ref::<crate::fs::inode::OSInode>()
            .is_some();
        let mut total = 0usize;
        loop {
            let remaining = len - total;
            let chunk_len = remaining.min(MAX_IO_STAGING_SIZE);
            let Some(chunk_addr) = (buf as usize).checked_add(total) else {
                return if total == 0 { -14 } else { total as isize };
            };
            let mut staging = match copy_to_user_after_io(
                &task,
                token,
                chunk_addr as *mut u8,
                chunk_len,
            ) {
                Ok(staging) => staging,
                Err(errno) => {
                    #[cfg(any(
                        feature = "buildstorm-diagnostics",
                        feature = "buildstorm-mm-diagnostics"
                    ))]
                    super::process::diagnose_task_user_access(
                        &task,
                        chunk_addr,
                        chunk_len,
                        true,
                        "read-prepare-out",
                    );
                    return if total == 0 { errno } else { total as isize };
                }
            };
            let read = file.read(staging.as_mut_slice());
            if read < 0 {
                return if total == 0 { read } else { total as isize };
            }
            let read = read as usize;
            if read > staging.as_slice().len() {
                return if total == 0 { -5 } else { total as isize };
            }
            if let Err(errno) = copy_staged_output_to_user(
                &task,
                token,
                chunk_addr as *mut u8,
                &staging.as_slice()[..read],
            ) {
                #[cfg(any(
                    feature = "buildstorm-diagnostics",
                    feature = "buildstorm-mm-diagnostics"
                ))]
                super::process::diagnose_task_user_access(
                    &task,
                    chunk_addr,
                    read,
                    true,
                    "read-copy-out",
                );
                return if total == 0 { errno } else { total as isize };
            }
            total += read;
            if read < staging.as_slice().len() || !complete_regular_io || total == len {
                return total as isize;
            }
        }
    } else {
        #[cfg(feature = "smp-diagnostics")]
        crate::println!(
            "[socketpair-diag] read-ebadf tid={} tgid={} fd={} empty-slot",
            task.getpid(),
            inner.tgid,
            fd,
        );
        -9
    }
}

pub fn sys_open(path: *const u8, flags: u32) -> isize {
    sys_openat(AT_FDCWD, path, flags, 0)
}

fn open_existing_access(dentry: &Arc<Dentry>, flags: OpenFlags) -> isize {
    let (readable, writable) = flags.read_write();
    let need = ((readable as u32) * 0o4) | (((writable || flags.trunc_requested()) as u32) * 0o2);
    if need == 0 {
        return 0;
    }
    if dentry.is_dir {
        return if writable || flags.trunc_requested() {
            -21
        } else {
            0
        };
    }
    let Some(task) = current_task() else {
        return -13;
    };
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if euid == 0 {
        return 0;
    }
    match dentry.inode_info() {
        Some(info) if inode_perm_allows(info.mode, info.uid, info.gid, euid, egid, need) => 0,
        Some(_) => -13,
        None => -2,
    }
}

fn is_libc_tmpfile_fallback(path: &str, open_flags: OpenFlags) -> bool {
    if !open_flags.create_requested() || !open_flags.excl_requested() {
        return false;
    }
    let (readable, writable) = open_flags.read_write();
    if !readable || !writable {
        return false;
    }
    let path = path.trim_matches('\0');
    path.starts_with("/tmp/tmpfile_") || path.starts_with("/var/tmp/tmpfile_")
}

pub fn sys_openat(dirfd: isize, path: *const u8, flags: u32, mode: u32) -> isize {
    let task = current_task().unwrap();
    let cloexec = (flags & O_CLOEXEC) != 0;
    let raw_path = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&raw_path) {
        return -36;
    }
    let path = map_dev_shm_path(&raw_path);

    // proc filesystem intercept
    if let Some(file) = open_proc(&path) {
        return install_fd(&task, file, cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|e| e);
    }

    if path == "/dev/urandom" || path == "/dev/random" {
        return install_fd(&task, Arc::new(Urandom), cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|e| e);
    }

    if path == "/dev/null" {
        return install_fd(&task, Arc::new(DevNull), cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|e| e);
    }

    if path == "/dev/tty" || path == "/dev/console" {
        return install_fd(&task, Arc::new(DevTty), cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|e| e);
    }

    if path == "/dev/zero" {
        let (readable, writable) = OpenFlags::from_raw(flags).read_write();
        return install_fd(&task, Arc::new(DevZero::new(readable, writable)), cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|e| e);
    }

    if let Some(index) = block_device_index(path.as_str()) {
        return install_fd(&task, Arc::new(DevBlock::new(index)), cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|e| e);
    }

    if flags & O_TMPFILE == O_TMPFILE {
        return sys_open_tmpfile(dirfd, path.as_str(), mode, cloexec);
    }

    let open_flags = OpenFlags::from_raw(flags);
    #[cfg(feature = "buildstorm-diagnostics")]
    if path.ends_with("build-script-build-script-build") {
        crate::println!(
            "[buildstorm-fs] traced-openat dirfd={} flags={:#x} mode={:#o} path={}",
            dirfd,
            flags,
            mode,
            path,
        );
    }
    let base = match resolve_base_dir_for_path(dirfd, path.as_str()) {
        Ok(base) => base,
        Err(errno) => return errno,
    };
    #[cfg(feature = "buildstorm-diagnostics")]
    let diagnose_buildstorm =
        path.contains("/target/debug/") || base.fs_path.contains("/target/debug/");
    if path.is_empty() {
        return -2;
    }
    if open_flags.create_requested() && open_flags.excl_requested() {
        let (parent, _) = match resolve_parent_and_name_checked(&base, path.as_str()) {
            Ok(parts) => parts,
            Err(errno) => return errno,
        };
        // O_CREAT|O_EXCL is already an atomic create operation. If creating
        // in this directory is permitted, avoid resolving the absent target
        // first; a racing/existing target is resolved only after create fails.
        // Read-only or denied parents retain the slower path below so an
        // already-existing target still reports EEXIST with Linux ordering.
        if !vfs_dentry_is_readonly(&parent) && can_modify_directory(&parent) {
            if let Some(inode) =
                open_file_at(Some(base.clone()), path.as_str(), open_flags, mode as u32)
            {
                let file: Arc<dyn File + Send + Sync> =
                    if is_libc_tmpfile_fallback(path.as_str(), open_flags) {
                        Arc::new(crate::fs::TmpFile::new())
                    } else {
                        inode
                    };
                return install_fd_shared(&task, file, cloexec)
                    .map(|fd| fd as isize)
                    .unwrap_or_else(|errno| errno);
            }
            return if vfs_resolve_with_fallback(&base, path.as_str()).is_some() {
                -17
            } else {
                -2
            };
        }
    }
    if open_flags.create_requested()
        && !open_flags.directory_requested()
        && !open_flags.nofollow_requested()
        && !open_flags.path_requested()
        && (!open_flags.trunc_requested() || open_flags.read_write().1)
    {
        let (parent, _) = match resolve_parent_and_name_checked(&base, path.as_str()) {
            Ok(parts) => parts,
            Err(errno) => return errno,
        };
        if !vfs_dentry_is_readonly(&parent) && can_modify_directory(&parent) {
            let mut deferred_flags = open_flags;
            deferred_flags.remove(OpenFlags::TRUNC);
            if let Some((inode, created)) = crate::fs::inode::open_file_at_with_status(
                Some(base.clone()),
                path.as_str(),
                deferred_flags,
                mode as u32,
            ) {
                if !created {
                    let existing = inode.get_dentry();
                    if (open_flags.read_write().1 || open_flags.trunc_requested())
                        && vfs_dentry_is_readonly(existing)
                    {
                        return EROFS;
                    }
                    let access = open_existing_access(existing, open_flags);
                    if access != 0 {
                        return access;
                    }
                    if flags & O_NOATIME != 0 {
                        let euid = {
                            let inner = task.inner_exclusive_access();
                            inner.euid
                        };
                        let owner = existing.inode_info().map(|info| info.uid).unwrap_or(0);
                        if euid != 0 && euid != owner {
                            return -1;
                        }
                    }
                    if open_flags.trunc_requested() && !inode.truncate(0) {
                        return -5;
                    }
                }
                return install_fd_shared(&task, inode, cloexec)
                    .map(|fd| fd as isize)
                    .unwrap_or_else(|errno| errno);
            }
        }
    }
    let existing = vfs_resolve_with_fallback(&base, path.as_str());
    #[cfg(feature = "buildstorm-diagnostics")]
    if path.ends_with("/invoked.timestamp")
        || path == "invoked.timestamp"
        || path.ends_with("build-script-build-script-build")
    {
        if let Some(cached) = existing.as_ref() {
            let physical = cached.super_block.get_inode_info(&cached.fs_path);
            crate::println!(
                "[buildstorm-fs] traced-openat-existing cached=({}, {}, {}) physical={:?} path={}",
                cached.inode_num,
                cached.is_dir,
                cached.is_link,
                physical.map(|info| (info.inode_num, info.is_dir, info.is_link, info.mode)),
                cached.fs_path,
            );
        }
    }
    if let Some(existing) = existing {
        if existing.is_link && open_flags.nofollow_requested() {
            if open_flags.path_requested() {
                let inode = Arc::new(crate::fs::inode::OSInode::new(
                    false, false, false, existing,
                ));
                return install_fd(&task, inode, cloexec)
                    .map(|fd| fd as isize)
                    .unwrap_or_else(|e| e);
            }
            return -40;
        }
        if open_flags.directory_requested() && !existing.is_dir {
            return -20;
        }
        if open_flags.create_requested() && open_flags.excl_requested() {
            return -17;
        }
        if open_flags.trunc_requested() && vfs_dentry_is_readonly(&existing) {
            return EROFS;
        }
        let access = open_existing_access(&existing, open_flags);
        if access != 0 {
            return access;
        }
        if flags & O_NOATIME != 0 {
            let euid = {
                let inner = task.inner_exclusive_access();
                inner.euid
            };
            let owner = existing.inode_info().map(|info| info.uid).unwrap_or(0);
            if euid != 0 && euid != owner {
                return -1;
            }
        }
    } else if !open_flags.create_requested() {
        #[cfg(feature = "buildstorm-diagnostics")]
        crate::println!(
            "[buildstorm-fs] open-lookup-missing dirfd={} flags={:#x} base={} path={}",
            dirfd,
            flags,
            base.fs_path,
            path,
        );
        return -2;
    } else {
        let (parent, _) = match resolve_parent_and_name_checked(&base, path.as_str()) {
            Ok(parts) => parts,
            Err(errno) => {
                #[cfg(feature = "buildstorm-diagnostics")]
                if diagnose_buildstorm {
                    crate::println!(
                        "[buildstorm-fs] open-parent-check-failed dirfd={} errno={} path={}",
                        dirfd,
                        errno,
                        path,
                    );
                }
                return errno;
            }
        };
        if vfs_dentry_is_readonly(&parent) {
            return EROFS;
        }
        if !can_modify_directory(&parent) {
            return -13;
        }
    }
    let use_anon_tmpfile = is_libc_tmpfile_fallback(path.as_str(), open_flags);
    let inode = open_file_at(Some(base.clone()), path.as_str(), open_flags, mode as u32);

    if let Some(inode) = inode {
        let file: Arc<dyn File + Send + Sync> = if use_anon_tmpfile {
            Arc::new(crate::fs::TmpFile::new())
        } else {
            inode.clone()
        };
        install_fd_shared(&task, file, cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|errno| errno)
    } else {
        if open_flags.create_requested()
            && open_flags.excl_requested()
            && vfs_resolve_with_fallback(&base, path.as_str()).is_some()
        {
            return -17;
        }
        #[cfg(feature = "buildstorm-diagnostics")]
        crate::println!(
            "[buildstorm-fs] open-file-at-failed dirfd={} flags={:#x} base={} path={}",
            dirfd,
            flags,
            base.fs_path,
            path,
        );
        -2
    }
}

fn sys_open_tmpfile(dirfd: isize, path: &str, _mode: u32, cloexec: bool) -> isize {
    let base = match resolve_base_dir_for_path(dirfd, path) {
        Ok(base) => base,
        Err(errno) => return errno,
    };
    let dir = match vfs_resolve_with_fallback(&base, path).and_then(|d| {
        if d.is_dir {
            Some(d)
        } else {
            crate::fs::follow_symlink(&base, d, 0).filter(|target| target.is_dir)
        }
    }) {
        Some(dir) => dir,
        None => return -20,
    };

    if vfs_dentry_is_readonly(&dir) {
        return EROFS;
    }

    let task = current_task().unwrap();
    install_fd_shared(&task, Arc::new(crate::fs::TmpFile::new()), cloexec)
        .map(|fd| fd as isize)
        .unwrap_or_else(|e| e)
}

fn open_proc(path: &str) -> Option<Arc<dyn File + Send + Sync>> {
    use crate::fs::proc::{
        DevRtcFile, ProcCpuinfoFile, ProcDirFile, ProcMeminfoFile, ProcMountsFile, ProcPidStatFile,
        ProcPidStatusFile, ProcStaticFile, ProcUptimeFile, PseudoDirFile, SysBlockStatFile,
    };
    let path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };
    if let Some(file) = open_proc_executable(path) {
        return Some(file);
    }
    if path == "/proc" {
        return Some(Arc::new(ProcDirFile::new()));
    }
    if path == "/proc/mounts" {
        return Some(Arc::new(ProcMountsFile::new()));
    }
    if path == "/proc/meminfo" {
        return Some(Arc::new(ProcMeminfoFile::new()));
    }
    if path == "/proc/cpuinfo" {
        return Some(Arc::new(ProcCpuinfoFile::new()));
    }
    if path == "/proc/uptime" {
        return Some(Arc::new(ProcUptimeFile::new()));
    }
    if path == "/proc/sys/kernel/pid_max" {
        return Some(Arc::new(ProcStaticFile::new("4194304\n")));
    }
    if path == "/proc/sys/kernel/tainted" {
        return Some(Arc::new(ProcStaticFile::new("0\n")));
    }
    if path == "/proc/self/maps" {
        return Some(Arc::new(ProcStaticFile::new(
            "10000-20000 r-xp 00000000 00:00 0 /proc/self/exe\n",
        )));
    }
    if path == "/proc/self/stat" {
        let pid = current_task().map(|task| task.getpid())?;
        return Some(Arc::new(ProcPidStatFile::new(pid)));
    }
    if path == "/proc/self/status" {
        let pid = current_task().map(|task| task.getpid())?;
        return Some(Arc::new(ProcPidStatusFile::new(pid)));
    }
    if path == "/dev/rtc" || path == "/dev/misc/rtc" {
        return Some(Arc::new(DevRtcFile));
    }
    match path {
        "/sys" => return Some(Arc::new(PseudoDirFile::sys_root())),
        "/sys/block" => return Some(Arc::new(PseudoDirFile::sys_block())),
        "/sys/block/vda" | "/sys/block/vdb" => {
            return Some(Arc::new(PseudoDirFile::sys_block_device()));
        }
        "/sys/dev" => return Some(Arc::new(PseudoDirFile::sys_dev())),
        "/sys/dev/block" => return Some(Arc::new(PseudoDirFile::sys_dev_block())),
        "/sys/dev/block/254:0" | "/sys/dev/block/254:1" => {
            return Some(Arc::new(PseudoDirFile::sys_dev_block_device()));
        }
        "/sys/block/vda/stat" | "/sys/block/vdb/stat" => {
            return Some(Arc::new(SysBlockStatFile::new()));
        }
        "/sys/block/vda/uevent" | "/sys/dev/block/254:0/uevent" => {
            return Some(Arc::new(ProcStaticFile::new(
                "MAJOR=254\nMINOR=0\nDEVNAME=vda\nDEVTYPE=disk\n",
            )));
        }
        "/sys/block/vdb/uevent" | "/sys/dev/block/254:1/uevent" => {
            return Some(Arc::new(ProcStaticFile::new(
                "MAJOR=254\nMINOR=1\nDEVNAME=vdb\nDEVTYPE=disk\n",
            )));
        }
        _ => {}
    }
    if let Some(pid_str) = path
        .strip_prefix("/proc/")
        .and_then(|s| s.strip_suffix("/stat"))
    {
        if let Ok(pid) = pid_str.parse::<usize>() {
            return Some(Arc::new(ProcPidStatFile::new(pid)));
        }
    }
    if let Some(pid_str) = path
        .strip_prefix("/proc/")
        .and_then(|s| s.strip_suffix("/status"))
    {
        if let Ok(pid) = pid_str.parse::<usize>() {
            return Some(Arc::new(ProcPidStatusFile::new(pid)));
        }
    }
    if let Some(pid_str) = path
        .strip_prefix("/proc/")
        .and_then(|s| s.strip_suffix("/maps"))
    {
        if pid_str.parse::<usize>().is_ok() {
            return Some(Arc::new(ProcStaticFile::new(
                "10000-20000 r-xp 00000000 00:00 0 /proc/self/exe\n",
            )));
        }
    }
    None
}

fn special_device_stat(path: &str) -> Option<Kstat> {
    let path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };
    if path == "/proc/uptime" {
        return Some(crate::fs::proc::ProcUptimeFile::new().stat());
    }
    match path {
        "/sys"
        | "/sys/block"
        | "/sys/block/vda"
        | "/sys/block/vdb"
        | "/sys/dev"
        | "/sys/dev/block"
        | "/sys/dev/block/254:0"
        | "/sys/dev/block/254:1" => return Some(Kstat::new(crate::fs::S_IFDIR | 0o555)),
        "/sys/block/vda/stat"
        | "/sys/block/vdb/stat"
        | "/sys/block/vda/uevent"
        | "/sys/block/vdb/uevent"
        | "/sys/dev/block/254:0/uevent"
        | "/sys/dev/block/254:1/uevent" => {
            return Some(Kstat::new(crate::fs::S_IFREG | 0o444));
        }
        _ => {}
    }
    let mut st = match path {
        "/dev/null" => Kstat::new(crate::fs::S_IFCHR | 0o666),
        "/dev/tty" | "/dev/console" => Kstat::new(crate::fs::S_IFCHR | 0o666),
        "/dev/zero" => Kstat::new(crate::fs::S_IFCHR | 0o666),
        "/dev/random" | "/dev/urandom" => Kstat::new(crate::fs::S_IFCHR | 0o444),
        "/dev/rtc" | "/dev/misc/rtc" => Kstat::new(crate::fs::S_IFCHR | 0o444),
        _ => {
            let index = block_device_index(path)?;
            let mut st = Kstat::new(crate::fs::S_IFBLK | 0o660);
            st.st_rdev = 0xfe00 + index as u64;
            st.st_size = crate::drivers::block::get_block_device(index)
                .and_then(|dev| dev.capacity_bytes())
                .unwrap_or(512 * 1024 * 1024) as i64;
            return Some(st);
        }
    };
    st.st_rdev = match path {
        "/dev/null" => 0x0103,
        "/dev/tty" | "/dev/console" => 0x0500,
        "/dev/zero" => 0x0105,
        "/dev/random" => 0x0108,
        "/dev/urandom" => 0x0109,
        "/dev/rtc" | "/dev/misc/rtc" => 0x0a00,
        _ => 0,
    };
    Some(st)
}

fn block_device_index(path: &str) -> Option<usize> {
    match path {
        "/dev/vda" | "vda" => Some(0),
        "/dev/vdb" | "vdb" => Some(1),
        "/dev/vdc" | "vdc" => Some(2),
        "/dev/vdd" | "vdd" => Some(3),
        _ => None,
    }
}

fn special_device_as_parent(path: &str) -> bool {
    path.starts_with("/dev/null/")
        || path.starts_with("/dev/tty/")
        || path.starts_with("/dev/console/")
        || path.starts_with("/dev/zero/")
        || path.starts_with("/dev/random/")
        || path.starts_with("/dev/urandom/")
        || path.starts_with("/dev/rtc/")
        || path.starts_with("/dev/misc/rtc/")
}

pub fn sys_mkdir(path: *const u8, mode: u32) -> isize {
    sys_mkdirat(AT_FDCWD, path, mode)
}

pub fn sys_mkdirat(dirfd: isize, path: *const u8, mode: u32) -> isize {
    let path = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    #[cfg(feature = "buildstorm-diagnostics")]
    if path.ends_with("/invoked.timestamp")
        || path == "invoked.timestamp"
        || path.contains("build-script-build-script-build")
    {
        crate::println!(
            "[buildstorm-fs] traced-mkdirat dirfd={} mode={:#o} path={}",
            dirfd,
            mode,
            path,
        );
    }
    #[cfg(feature = "smp-diagnostics")]
    if path.starts_with("/mnt") {
        crate::println!("[mkdir-diag] begin path={}", path);
    }
    if path_has_too_long_component(&path) {
        return -36;
    }
    let base_dir = if path.starts_with('/') {
        current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone())
    } else if dirfd == AT_FDCWD {
        current_task().unwrap().inner_exclusive_access().cwd.clone()
    } else if dirfd < 0 {
        return -9; // EBADF
    } else {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        let fd = dirfd as usize;
        if fd >= inner.fd_table.len() {
            return -9; // EBADF
        }
        let Some(file) = &inner.fd_table[fd] else {
            return -9; // EBADF
        };
        if !file.is_dir() {
            return -20; // ENOTDIR
        }
        if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
            inode.get_dentry().clone()
        } else {
            return -20; // ENOTDIR
        }
    };
    if path.is_empty() {
        return -2;
    }
    // Linux reports EEXIST when mkdirat names the already-existing lookup
    // root (for example "/", ".", or redundant slash/dot spellings).
    // There is no final component for resolve_parent_and_name_checked() in
    // these cases, so letting them fall through incorrectly produces ENOENT.
    // BusyBox `mkdir -p /absolute/path` probes "/" first and relies on this
    // EEXIST result before creating the remaining components.
    if path
        .split('/')
        .all(|component| component.is_empty() || component == ".")
    {
        return -17; // EEXIST
    }
    #[cfg(feature = "smp-diagnostics")]
    if path.starts_with("/mnt") {
        crate::println!("[mkdir-diag] base-ready path={}", path);
    }
    if special_device_stat(path.as_str()).is_some() {
        return -17;
    }
    // Check the target before entering the create path.  Besides matching
    // Linux's EEXIST result for files, directories, and dangling symlinks,
    // this avoids relying on a lookup performed after a failed Ext4 create:
    // that failure may have populated a transient negative dentry cache for
    // an object which already exists.  BusyBox mkdir -p probes each existing
    // prefix (for example /tmp) and must see EEXIST rather than ENOENT.
    match resolve_path_checked(&base_dir, path.as_str()) {
        Ok(_) => return -17, // EEXIST
        Err(-2) => {}
        Err(errno) => return errno,
    }
    #[cfg(feature = "smp-diagnostics")]
    if path.starts_with("/mnt") {
        crate::println!("[mkdir-diag] resolving-existing path={}", path);
    }
    #[cfg(feature = "smp-diagnostics")]
    if path.starts_with("/mnt") {
        crate::println!("[mkdir-diag] resolving-parent path={}", path);
    }
    let (parent, name) = match resolve_parent_and_name_checked(&base_dir, path.as_str()) {
        Ok(parts) => parts,
        Err(errno) => {
            #[cfg(feature = "buildstorm-diagnostics")]
            if path.contains("/.fingerprint/") {
                crate::println!(
                    "[buildstorm-fs] mkdir-parent-failed errno={} path={}",
                    errno,
                    path,
                );
            }
            return errno;
        }
    };
    if vfs_dentry_is_readonly(&parent) {
        return EROFS;
    }
    if !can_modify_directory(&parent) {
        return -13;
    }
    #[cfg(feature = "smp-diagnostics")]
    if path.starts_with("/mnt") {
        crate::println!("[mkdir-diag] creating path={}", path);
    }
    let created = vfs_create(&parent, &name, true, mode).is_some();
    #[cfg(feature = "buildstorm-diagnostics")]
    if path.ends_with("/invoked.timestamp")
        || path == "invoked.timestamp"
        || path.contains("build-script-build-script-build")
    {
        crate::println!(
            "[buildstorm-fs] traced-mkdirat-result created={} parent={} name={}",
            created,
            parent.fs_path,
            name,
        );
    }
    if created {
        #[cfg(feature = "smp-diagnostics")]
        if path.starts_with("/mnt") {
            crate::println!("[mkdir-diag] done path={}", path);
        }
        0
    } else {
        #[cfg(feature = "buildstorm-diagnostics")]
        if path.contains("/.fingerprint/") {
            crate::println!("[buildstorm-fs] mkdir-create-failed path={}", path);
        }
        // A racing creator may have installed the same directory after the
        // initial lookup. Linux reports EEXIST in that case, not ENOENT.
        if vfs_resolve_path(&base_dir, path.as_str()).is_some() {
            -17
        } else {
            -2
        }
    }
}

pub fn sys_unlink(path: *const u8) -> isize {
    sys_unlinkat(AT_FDCWD, path, 0)
}

const AT_REMOVEDIR: u32 = 0x200;

pub fn sys_unlinkat(dirfd: isize, path: *const u8, flags: u32) -> isize {
    let is_dir = (flags & AT_REMOVEDIR) != 0;
    if flags & !AT_REMOVEDIR != 0 {
        return -22;
    }
    let raw_path = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&raw_path) {
        return -36;
    }
    let path = map_dev_shm_path(&raw_path);
    let base_dir = match resolve_base_dir_for_path(dirfd, path.as_str()) {
        Ok(base) => base,
        Err(errno) => return errno,
    };
    if path.is_empty() {
        return -2;
    }
    let (parent, name) = match resolve_parent_and_name_checked(&base_dir, path.as_str()) {
        Ok(parts) => parts,
        Err(errno) => return errno,
    };
    let dentry = match vfs_lookup(&parent, &name) {
        Some(dentry) => dentry,
        None => return -2,
    };
    const FS_IMMUTABLE_FL: usize = 0x00000010;
    const FS_APPEND_FL: usize = 0x00000020;
    if crate::fs::inode::path_fs_flags(&dentry.fs_path) & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
        return -1;
    }
    if dentry.is_dir && !is_dir {
        return -21;
    }
    if !dentry.is_dir && is_dir {
        return -20;
    }
    if vfs_dentry_is_readonly(&parent) {
        return EROFS;
    }
    if !can_modify_directory(&parent) {
        return -13;
    }
    match crate::fs::unlink_at_result(Some(base_dir.clone()), path.as_str(), is_dir) {
        Ok(()) => {
            if !is_dir {
                crate::syscall::process::remove_shared_file_mappings(&dentry.fs_path);
            }
            0
        }
        Err(errno) => errno,
    }
}

pub fn sys_chdir(path: *const u8) -> isize {
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path_str) {
        return -36;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let cwd = inner.cwd.clone();
    drop(inner);
    let Some(dir_dentry) = vfs_resolve_path(&cwd, &path_str) else {
        return -2;
    };
    let dir_dentry = if dir_dentry.is_link {
        match follow_symlink(&cwd, dir_dentry, 0) {
            Some(target) => target,
            None => return -40,
        }
    } else {
        dir_dentry
    };
    if !dir_dentry.is_dir {
        return -20;
    }
    if !can_search_directory(&dir_dentry) {
        return -13;
    }
    let mut inner = task.inner_exclusive_access();
    inner.cwd = dir_dentry;
    0
}

pub fn sys_fchdir(fd: usize) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    let dentry = file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .map(|inode| inode.dentry().clone());
    drop(inner);
    let Some(dentry) = dentry else {
        return -20;
    };
    if !dentry.is_dir {
        return -20;
    }
    if !can_search_directory(&dentry) {
        return -13;
    }
    let mut inner = task.inner_exclusive_access();
    inner.cwd = dentry;
    0
}

pub fn sys_chroot(path: *const u8) -> isize {
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path_str) {
        return -36;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let cwd = inner.cwd.clone();
    drop(inner);
    let Some(root_dentry) = vfs_resolve_path(&cwd, &path_str) else {
        return -2;
    };
    if !root_dentry.is_dir {
        return -20;
    }
    let mut inner = task.inner_exclusive_access();
    inner.root = root_dentry.clone();
    inner.cwd = root_dentry;
    0
}

pub fn sys_getcwd(buf: *mut u8, size: usize) -> isize {
    let task = current_task().unwrap();
    let cwd_path = {
        let inner = task.inner_exclusive_access();
        let cwd_path = inner.cwd.path.clone();
        let root_path = inner.root.path.clone();
        if root_path != "/" && cwd_path.starts_with(&root_path) {
            let suffix = &cwd_path[root_path.len()..];
            if suffix.is_empty() {
                String::from("/")
            } else if suffix.starts_with('/') {
                String::from(suffix)
            } else {
                let mut path = String::from("/");
                path.push_str(suffix);
                path
            }
        } else {
            cwd_path
        }
    };
    let need = cwd_path.len() + 1;
    if size > 0 && size < need {
        return -34;
    }
    if size == 0 {
        return -34;
    }
    if buf.is_null() {
        return -14;
    }
    let mut bytes = cwd_path.into_bytes();
    bytes.push(0);
    if !super::process::copy_to_task_user(&task, buf as usize, &bytes) {
        return -14;
    }
    need as isize
}

pub fn sys_utimensat(dirfd: isize, path: *const u8, times: *const u8, _flags: usize) -> isize {
    const UTIME_NOW: i64 = (1i64 << 30) - 1;
    const UTIME_OMIT: i64 = (1i64 << 30) - 2;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TimeSpec {
        tv_sec: i64,
        tv_nsec: i64,
    }

    let task = current_task().unwrap();
    let fd_file = if path.is_null() {
        let fd = if dirfd < 0 { return -9 } else { dirfd as usize };
        let inner = task.inner_exclusive_access();
        if fd >= inner.fd_table.len() {
            return -9;
        }
        let Some(file) = inner.fd_table[fd].as_ref().cloned() else {
            return -9;
        };
        Some(file)
    } else {
        None
    };
    let dentry = if let Some(file) = &fd_file {
        if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
            Some(inode.get_dentry().clone())
        } else if file
            .as_any()
            .downcast_ref::<crate::fs::stdio::TmpFile>()
            .is_some()
        {
            None
        } else {
            return -9;
        }
    } else {
        let path_str = match translated_path_bounded(path) {
            Ok(path) => path,
            Err(errno) => return errno,
        };
        if special_device_as_parent(path_str.as_str()) {
            return -20;
        }
        if special_device_stat(path_str.as_str()).is_some() {
            return 0;
        }
        let base = match resolve_base_dir(dirfd) {
            Some(b) => b,
            None => return -2,
        };
        match vfs_resolve_with_fallback(&base, &path_str) {
            Some(d) => Some(d),
            None => return -2,
        }
    };

    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };

    if euid != 0 && dentry.is_some() {
        let dentry = dentry.as_ref().unwrap();
        let permitted = match dentry.inode_info() {
            Some(info) => {
                if info.uid == euid {
                    true
                } else {
                    let perm = if info.gid == egid {
                        (info.mode >> 3) & 0o7
                    } else {
                        info.mode & 0o7
                    };
                    (perm & 0o2) != 0
                }
            }
            None => false,
        };
        if !permitted {
            return -1; // EPERM
        }
    }

    let now_sec = super::process::user_realtime_sec_ceil().saturating_add(FILE_TIME_NOW_GUARD_SEC);
    let now_sec_u32 = now_sec.clamp(0, u32::MAX as i64) as u32;
    let now = (now_sec, 0i64);

    let (new_atime, new_mtime, changed) = if times.is_null() {
        (Some(now), Some(now), true)
    } else {
        let Some([ts0, ts1]) =
            super::process::copy_user_value(&task, times as *const [TimeSpec; 2])
        else {
            return -14;
        };
        for ts in [ts0, ts1] {
            if ts.tv_nsec != UTIME_NOW
                && ts.tv_nsec != UTIME_OMIT
                && !(0..1_000_000_000).contains(&ts.tv_nsec)
            {
                return -22;
            }
        }
        let mut ch = false;

        let at = if ts0.tv_nsec == UTIME_OMIT {
            None
        } else if ts0.tv_nsec == UTIME_NOW {
            ch = true;
            Some(now)
        } else {
            ch = true;
            Some((ts0.tv_sec, ts0.tv_nsec))
        };

        let mt = if ts1.tv_nsec == UTIME_OMIT {
            None
        } else if ts1.tv_nsec == UTIME_NOW {
            ch = true;
            Some(now)
        } else {
            ch = true;
            Some((ts1.tv_sec, ts1.tv_nsec))
        };

        if !ch {
            return 0; // both UTIME_OMIT → ctime unchanged
        }
        (at, mt, true)
    };

    if let Some(file) = &fd_file {
        if let Some(tmpfile) = file.as_any().downcast_ref::<crate::fs::stdio::TmpFile>() {
            tmpfile.update_times(new_atime, new_mtime, if changed { Some(now) } else { None });
            return 0;
        }
    }

    let dentry = dentry.unwrap();
    let new_atime_sec = new_atime.map(|(sec, _)| sec);
    let new_mtime_sec = new_mtime.map(|(sec, _)| sec);
    let fs_path = &dentry.fs_path;
    if let Some(a) = new_atime_sec {
        dentry
            .super_block
            .set_atime(fs_path, a.clamp(0, u32::MAX as i64) as u32);
    }
    if let Some(m) = new_mtime_sec {
        dentry
            .super_block
            .set_mtime(fs_path, m.clamp(0, u32::MAX as i64) as u32);
    }
    if changed {
        dentry.super_block.set_ctime(fs_path, now_sec_u32);
    }
    crate::fs::inode::update_path_cached_times(
        fs_path,
        new_atime_sec,
        new_mtime_sec,
        if changed { Some(now_sec) } else { None },
    );
    dentry.update_cached_times(
        new_atime_sec.map(|v| v.clamp(0, u32::MAX as i64) as u32),
        new_mtime_sec.map(|v| v.clamp(0, u32::MAX as i64) as u32),
        if changed {
            Some(now_sec.clamp(0, u32::MAX as i64) as u32)
        } else {
            None
        },
    );
    if let Some(file) = &fd_file {
        if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
            inode.update_cached_times(
                new_atime_sec,
                new_mtime_sec,
                if changed { Some(now_sec) } else { None },
            );
        }
    }

    0
}

pub fn sys_faccessat(dirfd: isize, path: *const u8, mode: usize, _flags: usize) -> isize {
    sys_faccessat2(dirfd, path, mode, 0)
}

fn inode_perm_allows(
    mode: u32,
    uid: u32,
    gid: u32,
    check_uid: u32,
    check_gid: u32,
    need: u32,
) -> bool {
    let mode = mode & 0o777;
    let owner_perm = (mode >> 6) & 0o7;
    let group_perm = (mode >> 3) & 0o7;
    let other_perm = mode & 0o7;
    let perm = if uid == check_uid {
        owner_perm
    } else if gid == check_gid || current_has_group(gid) {
        group_perm
    } else {
        other_perm
    };
    (perm & need) == need
}

fn current_has_group(gid: u32) -> bool {
    current_task()
        .map(|task| {
            let inner = task.inner_exclusive_access();
            inner.egid == gid || inner.supplementary_groups.contains(&gid)
        })
        .unwrap_or(false)
}

fn can_modify_directory(dir: &Arc<Dentry>) -> bool {
    let Some(task) = current_task() else {
        return false;
    };
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if euid == 0 {
        return true;
    }
    match dir.inode_info() {
        Some(info) => inode_perm_allows(info.mode, info.uid, info.gid, euid, egid, 0o3),
        None => false,
    }
}

fn can_search_directory(dir: &Arc<Dentry>) -> bool {
    let Some(task) = current_task() else {
        return false;
    };
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if euid == 0 {
        return true;
    }
    match dir.inode_info() {
        Some(info) => inode_perm_allows(info.mode, info.uid, info.gid, euid, egid, 0o1),
        None => false,
    }
}

fn chmod_requested_mode(requested: u32, file_gid: u32) -> u32 {
    let mut perm = requested & 0o7777;
    let Some(task) = current_task() else {
        return perm;
    };
    let euid = task.inner_exclusive_access().euid;
    if euid != 0 && (perm & 0o2000) != 0 && !current_has_group(file_gid) {
        perm &= !0o2000;
    }
    perm
}

fn check_traverse_to_leaf(
    base: &Arc<Dentry>,
    path: &str,
    check_uid: u32,
    check_gid: u32,
) -> Result<(), isize> {
    if check_uid == 0 {
        return Ok(());
    }
    let path = path.trim_matches('\0');
    let mut current = if path.starts_with('/') {
        current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone())
    } else {
        base.clone()
    };
    let mut components = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        current = resolve_search_dir(current)?;
        let Some(next) = vfs_lookup(&current, component) else {
            return Err(-2);
        };
        let next = if next.is_link {
            follow_symlink(&current, next, 0).ok_or(-40isize)?
        } else {
            next
        };
        if !next.is_dir {
            return Err(-20);
        }
        let Some(info) = next.inode_info() else {
            return Err(-2);
        };
        if !inode_perm_allows(info.mode, info.uid, info.gid, check_uid, check_gid, 0o1) {
            return Err(-13);
        }
        current = next;
    }
    Ok(())
}

pub fn sys_faccessat2(dirfd: isize, path: *const u8, mode: usize, flags: usize) -> isize {
    const F_OK: usize = 0;
    const R_OK: usize = 4;
    const W_OK: usize = 2;
    const X_OK: usize = 1;
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;
    const AT_EACCESS: usize = 0x200;
    const AT_EMPTY_PATH: usize = 0x1000;

    if mode & !(R_OK | W_OK | X_OK) != 0 {
        return -22;
    }
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EACCESS | AT_EMPTY_PATH) != 0 {
        return -22;
    }
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path_str) {
        return -36;
    }

    if path_str.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return -2;
    }
    if let Some(st) = special_device_stat(path_str.as_str()) {
        if mode == F_OK {
            return 0;
        }
        if (mode & X_OK) != 0 {
            return -13;
        }
        if (mode & W_OK) != 0 && (st.st_mode & 0o222) == 0 {
            return -13;
        }
        if (mode & R_OK) != 0 && (st.st_mode & 0o444) == 0 {
            return -13;
        }
        return 0;
    }
    let base = match resolve_base_dir_for_path(dirfd, path_str.as_str()) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let task = current_task().unwrap();
    let (uid, gid, euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.uid, inner.gid, inner.euid, inner.egid)
    };
    let (check_uid, check_gid) = if flags & AT_EACCESS != 0 {
        (euid, egid)
    } else {
        (uid, gid)
    };
    if let Err(errno) = check_traverse_to_leaf(&base, &path_str, check_uid, check_gid) {
        return errno;
    }
    let dentry = match resolve_path_checked(&base, &path_str) {
        Ok(d) => d,
        Err(errno) => return errno,
    };
    let dentry = if flags & AT_SYMLINK_NOFOLLOW == 0 {
        match crate::fs::follow_symlink(&base, dentry, 0) {
            Some(d) => d,
            None => return -40,
        }
    } else {
        dentry
    };

    if mode == F_OK {
        return 0;
    }
    if (mode & W_OK) != 0 && vfs_dentry_is_readonly(&dentry) {
        return EROFS;
    }

    let allowed = match dentry.inode_info() {
        Some(info) => {
            if check_uid == 0 {
                return if (mode & X_OK) != 0 && (info.mode & 0o111) == 0 {
                    -13
                } else {
                    0
                };
            }
            let mut need = 0;
            if (mode & R_OK) != 0 {
                need |= 0o4;
            }
            if (mode & W_OK) != 0 {
                need |= 0o2;
            }
            if (mode & X_OK) != 0 {
                need |= 0o1;
            }
            if !inode_perm_allows(info.mode, info.uid, info.gid, check_uid, check_gid, need) {
                false
            } else {
                true
            }
        }
        None => false,
    };

    if allowed {
        0
    } else {
        -13
    }
}

pub fn sys_fchmodat(dirfd: isize, path: *const u8, mode: u32) -> isize {
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_str.is_empty() {
        return -2;
    }
    if path_has_too_long_component(&path_str) {
        return -36;
    }
    let base = match resolve_base_dir_for_path(dirfd, &path_str) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let dentry = match vfs_resolve_with_fallback(&base, &path_str) {
        Some(d) => d,
        None => return -2,
    };
    let dentry = match follow_symlink(&base, dentry, 0) {
        Some(d) => d,
        None => return -40,
    };
    if vfs_dentry_is_readonly(&dentry) {
        return EROFS;
    }

    let task = current_task().unwrap();
    let euid = {
        let inner = task.inner_exclusive_access();
        inner.euid
    };

    let fs_path = &dentry.fs_path;
    if euid != 0 {
        let is_owner = match dentry.inode_info() {
            Some(info) => info.uid == euid,
            None => return -2,
        };
        if !is_owner {
            return -1;
        }
    }

    let info = match dentry.inode_info() {
        Some(info) => info,
        None => return -2,
    };
    let old_mode = info.mode;
    let file_type = if dentry.is_dir {
        crate::fs::S_IFDIR
    } else {
        let old_type = old_mode & 0o170000;
        if old_type == 0 {
            crate::fs::S_IFREG
        } else {
            old_type
        }
    };
    let new_perm = chmod_requested_mode(mode, info.gid);
    let rc = dentry.super_block.set_mode(fs_path, file_type | new_perm);
    if rc != 0 {
        return -1;
    }

    let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as u32;
    dentry.super_block.set_ctime(fs_path, now_sec);
    dentry.update_cached_mode(file_type | new_perm);
    dentry.update_cached_times(None, None, Some(now_sec));
    0
}

pub fn sys_fchmodat2(dirfd: isize, path: *const u8, mode: u32, flags: usize) -> isize {
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;
    const AT_EMPTY_PATH: usize = 0x1000;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return -22;
    }
    if flags != 0 {
        return -95;
    }
    sys_fchmodat(dirfd, path, mode)
}

fn resolve_fd_dentry(fd: usize) -> Option<Arc<Dentry>> {
    let task = current_task()?;
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return None;
    }
    let file = inner.fd_table[fd].as_ref()?;
    let inode = file.as_any().downcast_ref::<crate::fs::inode::OSInode>()?;
    Some(inode.get_dentry().clone())
}

fn chown_dentry(dentry: &Arc<Dentry>, owner: u32, group: u32) -> isize {
    if vfs_dentry_is_readonly(dentry) {
        return EROFS;
    }
    let Some(task) = current_task() else {
        return -1;
    };
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    let fs_path = &dentry.fs_path;
    let info = match dentry.inode_info() {
        Some(info) => info,
        None => return -2,
    };

    let uid_unchanged = owner == u32::MAX;
    let gid_unchanged = group == u32::MAX;
    if euid != 0 {
        if !uid_unchanged || info.uid != euid {
            return -1;
        }
        if !gid_unchanged && group != egid && !current_has_group(group) {
            return -1;
        }
    }

    let new_uid = if uid_unchanged { info.uid } else { owner };
    let new_gid = if gid_unchanged { info.gid } else { group };
    let rc = dentry.super_block.set_owner(fs_path, new_uid, new_gid);
    if rc != 0 {
        return -1;
    }

    let mut new_mode = info.mode;
    if (info.mode & 0o170000) == crate::fs::S_IFREG {
        new_mode &= !0o4000;
        if (info.mode & 0o0010) != 0 {
            new_mode &= !0o2000;
        }
    }
    if new_mode != info.mode {
        let _ = dentry.super_block.set_mode(fs_path, new_mode);
    }

    let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as u32;
    dentry.super_block.set_ctime(fs_path, now_sec);
    dentry.update_cached_owner(new_uid, new_gid, new_mode, now_sec);
    0
}

pub fn sys_fchmod(fd: usize, mode: u32) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    let file = match inner.fd_table[fd].as_ref() {
        Some(file) => file.clone(),
        None => return -9,
    };
    drop(inner);

    let inode = match file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        Some(inode) => inode,
        None => return -9,
    };
    let dentry = inode.get_dentry().clone();
    if vfs_dentry_is_readonly(&dentry) {
        return EROFS;
    }

    let euid = {
        let inner = task.inner_exclusive_access();
        inner.euid
    };

    let fs_path = &dentry.fs_path;
    if euid != 0 {
        let is_owner = match dentry.inode_info() {
            Some(info) => info.uid == euid,
            None => return -9,
        };
        if !is_owner {
            return -1;
        }
    }

    let info = match dentry.inode_info() {
        Some(info) => info,
        None => return -9,
    };
    let old_mode = info.mode;
    let file_type = if dentry.is_dir {
        crate::fs::S_IFDIR
    } else {
        let old_type = old_mode & 0o170000;
        if old_type == 0 {
            crate::fs::S_IFREG
        } else {
            old_type
        }
    };
    let new_mode = file_type | chmod_requested_mode(mode, info.gid);
    let rc = dentry.super_block.set_mode(fs_path, new_mode);
    if rc != 0 {
        return -1;
    }

    let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as u32;
    dentry.super_block.set_ctime(fs_path, now_sec);
    inode.update_cached_mode(new_mode);
    inode.update_cached_times(None, None, Some(now_sec as i64));
    0
}

pub fn sys_fchownat(dirfd: isize, path: *const u8, owner: u32, group: u32, flags: u32) -> isize {
    const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
    const AT_EMPTY_PATH: u32 = 0x1000;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return -22;
    }
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_str.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return -2;
    }
    if path_has_too_long_component(&path_str) {
        return -36;
    }

    let dentry = if path_str.is_empty() {
        if dirfd < 0 {
            return -9;
        }
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        let fd = dirfd as usize;
        if fd >= inner.fd_table.len() {
            return -9;
        }
        let Some(file) = &inner.fd_table[fd] else {
            return -9;
        };
        let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() else {
            return -9;
        };
        inode.get_dentry().clone()
    } else {
        let base = match resolve_base_dir_for_path(dirfd, &path_str) {
            Ok(b) => b,
            Err(errno) => return errno,
        };
        let dentry = match vfs_resolve_with_fallback(&base, &path_str) {
            Some(d) => d,
            None => return -2,
        };
        if flags & AT_SYMLINK_NOFOLLOW == 0 {
            match follow_symlink(&base, dentry, 0) {
                Some(d) => d,
                None => return -40,
            }
        } else {
            dentry
        }
    };

    chown_dentry(&dentry, owner, group)
}

pub fn sys_fchown(fd: usize, owner: u32, group: u32) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    let file = match inner.fd_table[fd].as_ref() {
        Some(file) => file.clone(),
        None => return -9,
    };
    drop(inner);
    let inode = match file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        Some(inode) => inode,
        None => return -9,
    };
    let dentry = inode.get_dentry().clone();

    let ret = chown_dentry(&dentry, owner, group);
    if ret == 0 {
        if let Some(info) = dentry.inode_info() {
            inode.update_cached_owner(info.uid, info.gid, info.mode, info.ctime as i64);
        }
    }
    ret
}

pub fn sys_fcntl(fd: usize, cmd: usize, arg: usize) -> isize {
    const F_DUPFD: usize = 0;
    const F_GETFD: usize = 1;
    const F_SETFD: usize = 2;
    const F_GETFL: usize = 3;
    const F_SETFL: usize = 4;
    const F_DUPFD_CLOEXEC: usize = 1030;
    const FD_CLOEXEC: usize = 1;
    const MAX_FD: usize = 1024;
    const F_RDLCK: i16 = 0;
    const F_WRLCK: i16 = 1;
    const F_UNLCK: i16 = 2;
    const FLOCK_SIZE: usize = 32;

    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9; // EBADF
    }

    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            if arg >= MAX_FD {
                return -22;
            }
            drop(inner);
            return duplicate_fd_shared(&task, fd, arg, cmd == F_DUPFD_CLOEXEC)
                .map(|new_fd| new_fd as isize)
                .unwrap_or_else(|errno| errno);
        }
        F_GETFD => {
            if inner.fd_cloexec[fd] {
                FD_CLOEXEC as isize
            } else {
                0
            }
        }
        F_SETFD => {
            drop(inner);
            return set_fd_cloexec_shared(&task, fd, (arg & FD_CLOEXEC) != 0)
                .map(|()| 0)
                .unwrap_or_else(|errno| errno);
        }
        F_GETFL => {
            if let Some(file) = &inner.fd_table[fd] {
                file.get_flags() as isize
            } else {
                -9
            }
        }
        F_SETFL => {
            if let Some(file) = &inner.fd_table[fd] {
                if file.set_flags(arg) {
                    0
                } else {
                    -22
                }
            } else {
                -9
            }
        }
        // File locking stubs (both 32-bit: 5/6/7 and 64-bit: 12/13/14)
        5 | 12 => {
            let pid = current_task().map(|task| task.getpid() as i32).unwrap_or(0);
            drop(inner);
            let mut buf = [0u8; FLOCK_SIZE];
            if !super::process::copy_from_task_user(&task, arg, &mut buf) {
                return -14;
            }
            let lock_type = F_UNLCK.to_le_bytes();
            buf[0..2].copy_from_slice(&lock_type);
            buf[24..28].copy_from_slice(&pid.to_le_bytes());
            if !super::process::copy_to_task_user(&task, arg, &buf) {
                return -14;
            }
            return 0;
        }
        6 | 7 | 13 | 14 => {
            let file = inner.fd_table[fd].as_ref().unwrap().clone();
            drop(inner);
            let mut buf = [0u8; FLOCK_SIZE];
            if !super::process::copy_from_task_user(&task, arg, &mut buf) {
                return -14;
            }
            let lock_type = i16::from_le_bytes([buf[0], buf[1]]);
            let whence = i16::from_le_bytes([buf[2], buf[3]]);
            if !matches!(lock_type, F_RDLCK | F_WRLCK | F_UNLCK) {
                return -22;
            }
            if !matches!(whence, 0 | 1 | 2) {
                return -22;
            }
            if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
                inode.set_lock_type(lock_type);
            }
            0
        }
        _ => -22, // EINVAL
    }
}

const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TCSETSW: usize = 0x5403;
const TCSETSF: usize = 0x5404;
const TCSBRK: usize = 0x5409;
const TCFLSH: usize = 0x540b;
const TIOCSCTTY: usize = 0x540e;
const TIOCGPGRP: usize = 0x540f;
const TIOCSPGRP: usize = 0x5410;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSWINSZ: usize = 0x5414;
const FIONREAD: usize = 0x541b;
const FIONBIO: usize = 0x5421;
const TIOCNOTTY: usize = 0x5422;
const BLKGETSIZE: usize = 0x1260;
const BLKGETSIZE64: usize = 0x80081272;
const FS_IOC_GETFLAGS: usize = 0x80086601;
const FS_IOC_SETFLAGS: usize = 0x40086602;

#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 19],
}

lazy_static! {
    static ref CONSOLE_TERMIOS: DeviceLock<Termios> = DeviceLock::new_named(
        Termios {
            c_iflag: 0x0500,
            c_oflag: 0x0005,
            c_cflag: 0x04bf,
            c_lflag: 0x8a3b,
            c_line: 0,
            c_cc: [
                0x03, 0x1c, 0x7f, 0x15, 0x04, 0x00, 0x01, 0x00, 0x11, 0x13, 0x1a, 0x00,
                0x12, 0x0f, 0x17, 0x16, 0x00, 0x00, 0x00,
            ],
        },
        "Console.termios",
    );
}

static CONSOLE_FOREGROUND_PGRP: AtomicI32 = AtomicI32::new(0);

#[repr(C)]
#[derive(Clone, Copy)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

lazy_static! {
    static ref CONSOLE_WINSIZE: DeviceLock<Winsize> = DeviceLock::new_named(
        Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
        "Console.winsize",
    );
}

fn is_terminal_fd(fd: usize) -> bool {
    let Some(task) = current_task() else {
        return false;
    };
    let inner = task.inner_exclusive_access();
    let Some(file) = inner.fd_table.get(fd).and_then(|file| file.as_ref()) else {
        return false;
    };
    file.as_any().is::<crate::fs::Stdin>()
        || file.as_any().is::<crate::fs::Stdout>()
        || file.as_any().is::<crate::fs::DevTty>()
}

pub fn sys_ioctl(fd: usize, request: usize, arg: usize) -> isize {
    let request = request & 0xffffffff;

    if request == FIONBIO {
        const O_NONBLOCK: usize = 0x800;
        let task = current_task().unwrap();
        let Some(enabled) = super::process::copy_user_value(&task, arg as *const i32) else {
            return -14;
        };
        let enabled = enabled != 0;
        let file = {
            let inner = task.inner_exclusive_access();
            if fd >= inner.fd_table.len() {
                return -9;
            }
            match &inner.fd_table[fd] {
                Some(file) => file.clone(),
                None => return -9,
            }
        };
        let mut flags = file.get_flags();
        if enabled {
            flags |= O_NONBLOCK;
        } else {
            flags &= !O_NONBLOCK;
        }
        return if file.set_flags(flags) { 0 } else { -25 };
    }

    // RTC ioctl
    const RTC_RD_TIME: usize = 0x80247009;
    if request == RTC_RD_TIME {
        if fd >= 1024 {
            return -9;
        }
        let now_ms = polyhal::time::Time::now().to_msec() as u64;
        let secs = (now_ms / 1000) as i64;
        let days_since_epoch = (secs / 86400) as i32;
        let rem = (secs % 86400) as i32;
        let tm_sec = rem % 60;
        let tm_min = (rem / 60) % 60;
        let tm_hour = (rem / 3600) % 24;
        let mut tm_year = 70i32;
        let mut days_left = days_since_epoch;
        loop {
            let diy = if is_leap(1900 + tm_year) { 366 } else { 365 };
            if days_left < diy {
                break;
            }
            days_left -= diy;
            tm_year += 1;
        }
        let tm_yday = days_left;
        let months: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut tm_mon = 0;
        let mut mdays = days_left;
        for i in 0..12 {
            let md = if i == 1 && is_leap(1900 + tm_year) {
                months[i] + 1
            } else {
                months[i]
            };
            if mdays < md {
                tm_mon = i as i32;
                break;
            }
            mdays -= md;
        }
        let tm_mday = mdays + 1;
        let tm_wday = ((days_since_epoch + 4) % 7) as i32;
        #[repr(C)]
        struct RtcTime {
            tm_sec: i32,
            tm_min: i32,
            tm_hour: i32,
            tm_mday: i32,
            tm_mon: i32,
            tm_year: i32,
            tm_wday: i32,
            tm_yday: i32,
            tm_isdst: i32,
        }
        let rtc = RtcTime {
            tm_sec,
            tm_min,
            tm_hour,
            tm_mday,
            tm_mon,
            tm_year,
            tm_wday,
            tm_yday,
            tm_isdst: -1,
        };
        let task = current_task().unwrap();
        return if super::process::write_user_value(&task, arg as *mut RtcTime, &rtc) {
            0
        } else {
            -14
        };
    }

    if request == BLKGETSIZE || request == BLKGETSIZE64 {
        if arg == 0 {
            return -14;
        }
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        if fd >= inner.fd_table.len() {
            return -9;
        }
        let file = match &inner.fd_table[fd] {
            Some(file) => file.clone(),
            None => return -9,
        };
        drop(inner);
        let Some(dev) = file.as_any().downcast_ref::<DevBlock>() else {
            return -25;
        };
        let size = dev.capacity_bytes();
        let value = if request == BLKGETSIZE64 {
            size
        } else {
            size / 512
        };
        return if super::process::write_user_value(&task, arg as *mut u64, &value) {
            0
        } else {
            -14
        };
    }

    if request == FS_IOC_GETFLAGS || request == FS_IOC_SETFLAGS {
        let word = core::mem::size_of::<usize>();
        if arg == 0 {
            return -14;
        }
        if request == FS_IOC_GETFLAGS && !valid_user_write_buf(arg as *mut u8, word) {
            return -14;
        }
        if request == FS_IOC_SETFLAGS && !valid_user_read_buf(arg as *const u8, word) {
            return -14;
        }
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        if fd >= inner.fd_table.len() {
            return -9;
        }
        let file = match &inner.fd_table[fd] {
            Some(file) => file.clone(),
            None => return -9,
        };
        drop(inner);
        let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() else {
            return -25;
        };
        let fs_path = inode.get_dentry().fs_path.clone();
        if request == FS_IOC_GETFLAGS {
            let flags = crate::fs::inode::path_fs_flags(&fs_path);
            if !super::process::write_user_value(&task, arg as *mut usize, &flags) {
                return -14;
            }
        } else {
            let Some(flags) = super::process::copy_user_value(&task, arg as *const usize) else {
                return -14;
            };
            crate::fs::inode::set_path_fs_flags(&fs_path, flags);
        }
        return 0;
    }

    match request {
        TCGETS => {
            if !is_terminal_fd(fd) {
                return -25; // ENOTTY
            }
            if arg == 0 {
                return -14; // EFAULT
            }
            let term = *CONSOLE_TERMIOS.lock();
            let task = current_task().unwrap();
            if super::process::write_user_value(&task, arg as *mut Termios, &term) {
                0
            } else {
                -14
            }
        }
        TCSETS | TCSETSW | TCSETSF => {
            if !is_terminal_fd(fd) {
                return -25;
            }
            if arg == 0 {
                return -14;
            }
            let task = current_task().unwrap();
            let Some(term) = super::process::copy_user_value(&task, arg as *const Termios) else {
                return -14;
            };
            *CONSOLE_TERMIOS.lock() = term;
            crate::fs::stdio::set_console_termios(term.c_iflag, term.c_lflag, &term.c_cc);
            if request == TCSETSF {
                crate::fs::stdio::flush_console_input();
            }
            0
        }
        TIOCSCTTY => {
            if is_terminal_fd(fd) {
                0
            } else {
                -25
            }
        }
        TIOCGPGRP => {
            if !is_terminal_fd(fd) {
                return -25;
            }
            if arg == 0 {
                return -14;
            }
            let task = current_task().unwrap();
            let current_pgrp = task.inner_exclusive_access().pgid as i32;
            let pgrp = match CONSOLE_FOREGROUND_PGRP.load(Ordering::Acquire) {
                0 => current_pgrp,
                value => value,
            };
            if super::process::write_user_value(&task, arg as *mut i32, &pgrp) {
                0
            } else {
                -14
            }
        }
        TIOCSPGRP => {
            if !is_terminal_fd(fd) {
                return -25;
            }
            if arg == 0 {
                return -14;
            }
            let task = current_task().unwrap();
            let Some(pgrp) = super::process::copy_user_value(&task, arg as *const i32) else {
                return -14;
            };
            if pgrp <= 0 {
                return -22;
            }
            CONSOLE_FOREGROUND_PGRP.store(pgrp, Ordering::Release);
            0
        }
        TCSBRK | TCFLSH | TIOCNOTTY => {
            if is_terminal_fd(fd) {
                0
            } else {
                -25
            }
        }
        FIONREAD => {
            if !is_terminal_fd(fd) {
                return -25;
            }
            if arg == 0 {
                return -14;
            }
            let task = current_task().unwrap();
            let available = crate::fs::stdio::console_input_available(false)
                .min(i32::MAX as usize) as i32;
            if super::process::write_user_value(&task, arg as *mut i32, &available) {
                0
            } else {
                -14
            }
        }
        TIOCGWINSZ => {
            if !is_terminal_fd(fd) {
                return -25; // ENOTTY
            }
            if arg == 0 {
                return -14;
            }
            let ws = *CONSOLE_WINSIZE.lock();
            let task = current_task().unwrap();
            if super::process::write_user_value(&task, arg as *mut Winsize, &ws) {
                0
            } else {
                -14
            }
        }
        TIOCSWINSZ => {
            if !is_terminal_fd(fd) {
                return -25;
            }
            if arg == 0 {
                return -14;
            }
            let task = current_task().unwrap();
            let Some(ws) = super::process::copy_user_value(&task, arg as *const Winsize) else {
                return -14;
            };
            *CONSOLE_WINSIZE.lock() = ws;
            0
        }
        _ => -25, // ENOTTY
    }
}

pub fn sys_close(fd: usize) -> isize {
    let task = current_task().unwrap();
    match close_fd_shared(&task, fd) {
        Ok(is_unix_socket) => {
        #[cfg(feature = "smp-diagnostics")]
        if is_unix_socket {
            let tgid = task.inner_exclusive_access().tgid;
            crate::println!("[socketpair-diag] close tid={} tgid={} fd={}", task.getpid(), tgid, fd);
        }
        0
        }
        Err(errno) => errno,
    }
}

pub fn sys_pipe(pipe: *mut usize) -> isize {
    sys_pipe2(pipe, 0)
}

pub fn sys_pipe2(pipe: *mut usize, flags: usize) -> isize {
    const O_NONBLOCK: usize = 0x800;
    const O_CLOEXEC: usize = 0x80000;
    if pipe.is_null() {
        return -14; // EFAULT
    }
    if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        return -22; // EINVAL
    }
    let task = current_task().unwrap();
    if !super::process::prepare_task_user_access(
        &task,
        pipe as usize,
        2 * core::mem::size_of::<u32>(),
        true,
    ) {
        #[cfg(feature = "buildstorm-diagnostics")]
        super::process::diagnose_task_user_access(
            &task,
            pipe as usize,
            2 * core::mem::size_of::<u32>(),
            true,
            "pipe2-prevalidate",
        );
        return -14;
    }
    let (pipe_read, pipe_write) = make_pipe();
    if flags & O_NONBLOCK != 0 {
        pipe_read.set_flags(O_NONBLOCK);
        pipe_write.set_flags(O_NONBLOCK);
    }
    let cloexec = flags & O_CLOEXEC != 0;
    let read_fd = match install_fd_shared(&task, pipe_read, cloexec) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    let write_fd = match install_fd_shared(&task, pipe_write, cloexec) {
        Ok(fd) => fd,
        Err(errno) => {
            let _ = close_fd_shared(&task, read_fd);
            return errno;
        }
    };
    let fds = [read_fd as u32, write_fd as u32];
    let bytes = unsafe {
        core::slice::from_raw_parts(
            fds.as_ptr() as *const u8,
            core::mem::size_of_val(&fds),
        )
    };
    if !super::process::copy_to_task_user(&task, pipe as usize, bytes) {
        #[cfg(feature = "buildstorm-diagnostics")]
        super::process::diagnose_task_user_access(
            &task,
            pipe as usize,
            bytes.len(),
            true,
            "pipe2-writeback",
        );
        let _ = close_fd_shared(&task, read_fd);
        let _ = close_fd_shared(&task, write_fd);
        return -14;
    }
    0
}

pub fn sys_dup(fd: usize) -> isize {
    let task = current_task().unwrap();
    duplicate_fd_shared(&task, fd, 0, false)
        .map(|new_fd| new_fd as isize)
        .unwrap_or_else(|errno| errno)
}

pub fn sys_dup2(fd: usize, new_fd: usize) -> isize {
    sys_dup3(fd, new_fd, 0)
}

pub fn sys_dup3(fd: usize, new_fd: usize, flags: usize) -> isize {
    const O_CLOEXEC: usize = 0x80000;
    const MAX_FD: usize = 1024;
    if flags != 0 && flags != O_CLOEXEC {
        return -22;
    }
    if fd == new_fd {
        return new_fd as isize;
    }
    if new_fd >= MAX_FD {
        return -9;
    }
    let task = current_task().unwrap();
    replace_fd_shared(&task, fd, new_fd, (flags & O_CLOEXEC) != 0)
        .map(|new_fd| new_fd as isize)
        .unwrap_or_else(|errno| errno)
}

pub fn sys_lseek(fd: usize, offset: i64, whence: i32) -> isize {
    const SEEK_SET: i32 = 0;
    const SEEK_CUR: i32 = 1;
    const SEEK_END: i32 = 2;

    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9; // EBADF
    }
    let Some(file) = &inner.fd_table[fd] else {
        return -9; // EBADF
    };
    let file = file.clone();
    drop(inner);
    let mode_type = file.stat().st_mode & 0o170000;
    let is_regular = mode_type == 0o100000;
    let is_directory = mode_type == 0o040000;
    if !is_regular && !is_directory {
        return -29; // ESPIPE
    }

    let new_off: i64 = match whence {
        SEEK_SET => offset,
        SEEK_CUR => {
            let cur = file.get_offset() as i64;
            cur + offset
        }
        SEEK_END => {
            if is_directory {
                return -22;
            }
            let size = file.stat().st_size;
            size + offset
        }
        _ => return -22, // EINVAL
    };

    if new_off < 0 {
        return -22; // EINVAL
    }

    file.seek(new_off as usize);
    new_off as isize
}

pub fn sys_renameat2(
    olddirfd: isize,
    oldpath: *const u8,
    newdirfd: isize,
    newpath: *const u8,
    flags: usize,
) -> isize {
    const RENAME_NOREPLACE: usize = 1;

    if flags & !RENAME_NOREPLACE != 0 {
        return -22;
    }
    let old_str = match translated_path_bounded(oldpath) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let new_str = match translated_path_bounded(newpath) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if old_str.is_empty() || new_str.is_empty() {
        return -2;
    }
    if path_has_too_long_component(&old_str) || path_has_too_long_component(&new_str) {
        return -36;
    }
    let old_base = match resolve_base_dir_for_path(olddirfd, &old_str) {
        Ok(base) => base,
        Err(errno) => return errno,
    };
    let new_base = match resolve_base_dir_for_path(newdirfd, &new_str) {
        Ok(base) => base,
        Err(errno) => return errno,
    };
    let (euid, egid) = current_task()
        .map(|task| {
            let inner = task.inner_exclusive_access();
            (inner.euid, inner.egid)
        })
        .unwrap_or((0, 0));
    if let Err(errno) = check_traverse_to_leaf(&old_base, &old_str, euid, egid) {
        return errno;
    }
    if let Err(errno) = check_traverse_to_leaf(&new_base, &new_str, euid, egid) {
        return errno;
    }
    let (old_parent, _) = match resolve_parent_and_name_checked(&old_base, &old_str) {
        Ok(parts) => parts,
        Err(errno) => return errno,
    };
    let (new_parent, _) = match resolve_parent_and_name_checked(&new_base, &new_str) {
        Ok(parts) => parts,
        Err(errno) => return errno,
    };
    if vfs_dentry_is_readonly(&old_parent) || vfs_dentry_is_readonly(&new_parent) {
        return EROFS;
    }
    if !can_modify_directory(&old_parent) || !can_modify_directory(&new_parent) {
        return -13;
    }

    match rename_at(
        Some(old_base),
        &old_str,
        Some(new_base),
        &new_str,
        flags as u32,
    ) {
        Ok(()) => 0,
        Err(errno) => errno,
    }
}

pub fn sys_symlinkat(target: *const u8, newdirfd: isize, linkpath: *const u8) -> isize {
    let target_str = match translated_path_bounded(target) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let linkpath_str = match translated_path_bounded(linkpath) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if target_str.is_empty() || linkpath_str.is_empty() {
        return -2; // ENOENT
    }
    if path_has_too_long_component(&linkpath_str) {
        return -36;
    }
    let base = match resolve_base_dir_for_path(newdirfd, &linkpath_str) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    if vfs_resolve_path(&base, &linkpath_str).is_some() {
        return -17;
    }
    let (parent, _) = match resolve_parent_and_name_checked(&base, &linkpath_str) {
        Ok(parts) => parts,
        Err(errno) => return errno,
    };
    if vfs_dentry_is_readonly(&parent) {
        return EROFS;
    }
    if !can_modify_directory(&parent) {
        return -13;
    }
    if symlink_at(Some(base), &target_str, &linkpath_str) {
        0
    } else {
        -1
    }
}

pub fn sys_linkat(
    olddirfd: isize,
    oldpath: *const u8,
    newdirfd: isize,
    newpath: *const u8,
    flags: u32,
) -> isize {
    const AT_SYMLINK_FOLLOW: u32 = 0x400;
    const EXDEV: isize = -18;
    if flags & !AT_SYMLINK_FOLLOW != 0 {
        return -22;
    }
    let old_str = match translated_path_bounded(oldpath) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let new_str = match translated_path_bounded(newpath) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&old_str) || path_has_too_long_component(&new_str) {
        return -36;
    }
    if old_str.is_empty() || new_str.is_empty() {
        return -2;
    }
    let old_lookup = if flags & AT_SYMLINK_FOLLOW != 0 {
        proc_fd_readlink(&old_str).unwrap_or_else(|| old_str.clone())
    } else {
        old_str.clone()
    };
    if old_lookup.starts_with("/proc/") {
        return EXDEV;
    }
    let old_base = match resolve_base_dir_for_path(olddirfd, &old_lookup) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let new_base = match resolve_base_dir_for_path(newdirfd, &new_str) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let task = current_task().unwrap();
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if let Err(errno) = check_traverse_to_leaf(&old_base, &old_lookup, euid, egid) {
        return errno;
    }
    let old_dentry = match resolve_path_checked(&old_base, &old_lookup) {
        Ok(dentry) => {
            if flags & AT_SYMLINK_FOLLOW != 0 && dentry.is_link {
                match follow_symlink(&old_base, dentry, 0) {
                    Some(target) => target,
                    None => return -40,
                }
            } else {
                dentry
            }
        }
        Err(errno) => return errno,
    };
    if old_dentry.is_dir {
        return -1;
    }
    if let Err(errno) = check_traverse_to_leaf(&new_base, &new_str, euid, egid) {
        return errno;
    }
    let (new_parent, new_name) = match resolve_parent_and_name_checked(&new_base, &new_str) {
        Ok(parts) => parts,
        Err(errno) => return errno,
    };
    if vfs_lookup(&new_parent, &new_name).is_some() {
        return -17;
    }
    if vfs_dentry_is_readonly(&new_parent) {
        return EROFS;
    }
    if !can_modify_directory(&new_parent) {
        return -13;
    }
    if link_at(Some(old_base), &old_lookup, Some(new_base), &new_str) {
        0
    } else {
        -1
    }
}

fn read_symlink_target(dentry: &Arc<Dentry>, max_len: usize) -> Result<Vec<u8>, isize> {
    let mut link_data = read_dentry_symlink_bytes(dentry).ok_or(-22isize)?;
    link_data.truncate(max_len.min(4096));
    Ok(link_data)
}

pub fn sys_readlinkat(dirfd: isize, path: *const u8, buf: *mut u8, bufsiz: usize) -> isize {
    if path.is_null() || buf.is_null() {
        return -14;
    }
    if bufsiz == 0 {
        return -22;
    }
    if !valid_user_write_buf(buf, bufsiz) {
        return -14;
    }

    let task = current_task().unwrap();
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path_str) {
        return -36;
    }
    if path_str.is_empty() {
        if dirfd < 0 {
            return -2;
        }
        let fd = dirfd as usize;
        let dentry = {
            let inner = task.inner_exclusive_access();
            if fd >= inner.fd_table.len() {
                return -9;
            }
            let Some(file) = &inner.fd_table[fd] else {
                return -9;
            };
            let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() else {
                return -22;
            };
            inode.get_dentry().clone()
        };
        let link_target = match read_symlink_target(&dentry, bufsiz) {
            Ok(target) => target,
            Err(errno) => return errno,
        };
        let len = link_target.len().min(bufsiz);
        if !super::process::copy_to_task_user(&task, buf as usize, &link_target[..len]) {
            return -14;
        }
        return len as isize;
    }
    if let Some(link_target) = proc_exe_readlink(path_str.as_str())
        .or_else(|| proc_fd_readlink(path_str.as_str()))
    {
        let bytes = link_target.as_bytes();
        let len = bytes.len().min(bufsiz);
        if !super::process::copy_to_task_user(&task, buf as usize, &bytes[..len]) {
            return -14;
        }
        return len as isize;
    }

    let base = match resolve_base_dir_for_path(dirfd, &path_str) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if let Err(errno) = check_traverse_to_leaf(&base, &path_str, euid, egid) {
        return errno;
    }
    let dentry = match resolve_path_checked(&base, &path_str) {
        Ok(d) => d,
        Err(errno) => return errno,
    };

    let link_target = match read_symlink_target(&dentry, bufsiz) {
        Ok(target) => target,
        Err(errno) => return errno,
    };

    let len = link_target.len().min(bufsiz);
    if !super::process::copy_to_task_user(&task, buf as usize, &link_target[..len]) {
        return -14;
    }
    len as isize
}

fn validate_xattr_path(path: *const u8, follow_symlink: bool) -> Result<(), isize> {
    let path_str = translated_path_bounded(path)?;
    if path_has_too_long_component(&path_str) {
        return Err(-36);
    }
    if path_str.is_empty() {
        return Err(-2);
    }
    if proc_exe_stat(path_str.as_str(), !follow_symlink).is_some()
        || special_device_stat(path_str.as_str()).is_some()
    {
        return Ok(());
    }
    let task = current_task().ok_or(-9isize)?;
    let base = resolve_base_dir_for_path(AT_FDCWD, &path_str)?;
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    check_traverse_to_leaf(&base, &path_str, euid, egid)?;
    let dentry = resolve_path_checked(&base, &path_str)?;
    if follow_symlink && dentry.is_link {
        crate::fs::follow_symlink(&base, dentry, 0).ok_or(-40isize)?;
    }
    Ok(())
}

fn validate_xattr_fd(fd: usize) -> Result<(), isize> {
    let task = current_task().ok_or(-9isize)?;
    let inner = task.inner_exclusive_access();
    if inner.fd_table.get(fd).and_then(|file| file.as_ref()).is_some() {
        Ok(())
    } else {
        Err(-9)
    }
}

fn validate_xattr_name(name: *const u8) -> Result<(), isize> {
    let name = translated_path_bounded(name)?;
    if name.is_empty() {
        return Err(-34);
    }
    if name.len() > 255 {
        return Err(-34);
    }
    Ok(())
}

pub fn sys_listxattr(
    path: *const u8,
    list: *mut u8,
    size: usize,
    follow_symlink: bool,
) -> isize {
    if size > 0 && !valid_user_write_buf(list, size) {
        return -14;
    }
    if let Err(errno) = validate_xattr_path(path, follow_symlink) {
        return errno;
    }
    0
}

pub fn sys_flistxattr(fd: usize, list: *mut u8, size: usize) -> isize {
    if size > 0 && !valid_user_write_buf(list, size) {
        return -14;
    }
    if let Err(errno) = validate_xattr_fd(fd) {
        return errno;
    }
    0
}

pub fn sys_getxattr(
    path: *const u8,
    name: *const u8,
    _value: *mut u8,
    _size: usize,
    follow_symlink: bool,
) -> isize {
    if let Err(errno) = validate_xattr_path(path, follow_symlink) {
        return errno;
    }
    if let Err(errno) = validate_xattr_name(name) {
        return errno;
    }
    -61
}

pub fn sys_fgetxattr(fd: usize, name: *const u8, _value: *mut u8, _size: usize) -> isize {
    if let Err(errno) = validate_xattr_fd(fd) {
        return errno;
    }
    if let Err(errno) = validate_xattr_name(name) {
        return errno;
    }
    -61
}

pub fn sys_setxattr(
    path: *const u8,
    name: *const u8,
    value: *const u8,
    size: usize,
    flags: usize,
    follow_symlink: bool,
) -> isize {
    if flags & !3 != 0 || flags == 3 {
        return -22;
    }
    if size > 0 && !valid_user_read_buf(value, size) {
        return -14;
    }
    if let Err(errno) = validate_xattr_path(path, follow_symlink) {
        return errno;
    }
    if let Err(errno) = validate_xattr_name(name) {
        return errno;
    }
    -95
}

pub fn sys_fsetxattr(
    fd: usize,
    name: *const u8,
    value: *const u8,
    size: usize,
    flags: usize,
) -> isize {
    if flags & !3 != 0 || flags == 3 {
        return -22;
    }
    if size > 0 && !valid_user_read_buf(value, size) {
        return -14;
    }
    if let Err(errno) = validate_xattr_fd(fd) {
        return errno;
    }
    if let Err(errno) = validate_xattr_name(name) {
        return errno;
    }
    -95
}

pub fn sys_removexattr(path: *const u8, name: *const u8, follow_symlink: bool) -> isize {
    if let Err(errno) = validate_xattr_path(path, follow_symlink) {
        return errno;
    }
    if let Err(errno) = validate_xattr_name(name) {
        return errno;
    }
    -61
}

pub fn sys_fremovexattr(fd: usize, name: *const u8) -> isize {
    if let Err(errno) = validate_xattr_fd(fd) {
        return errno;
    }
    if let Err(errno) = validate_xattr_name(name) {
        return errno;
    }
    -61
}

pub fn sys_fstatat(dirfd: isize, path: *const u8, stat_buf: *mut u8, flags: usize) -> isize {
    const AT_EMPTY_PATH: usize = 0x1000;
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;
    const AT_NO_AUTOMOUNT: usize = 0x800;

    if !valid_user_write_buf(stat_buf, core::mem::size_of::<Kstat>()) {
        return -14; // EFAULT
    }
    if flags & !(AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT) != 0 {
        return -22;
    }

    let task = current_task().unwrap();
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path_str) {
        return -36;
    }
    if path_str.is_empty() {
        return if flags & AT_EMPTY_PATH != 0 {
            sys_fstat(dirfd as usize, stat_buf)
        } else {
            -2
        };
    }
    if let Some(st) = proc_exe_stat(path_str.as_str(), flags & AT_SYMLINK_NOFOLLOW != 0) {
        return if super::process::write_user_value(&task, stat_buf as *mut Kstat, &st) {
            0
        } else {
            -14
        };
    }
    if let Some(st) = special_device_stat(path_str.as_str()) {
        return if super::process::write_user_value(&task, stat_buf as *mut Kstat, &st) {
            0
        } else {
            -14
        };
    }

    let base = match resolve_base_dir_for_path(dirfd, &path_str) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if let Err(errno) = check_traverse_to_leaf(&base, &path_str, euid, egid) {
        return errno;
    }
    let dentry = match resolve_path_checked(&base, &path_str) {
        Ok(d) => d,
        Err(errno) => return errno,
    };
    let dentry = if flags & AT_SYMLINK_NOFOLLOW == 0 {
        match crate::fs::follow_symlink(&base, dentry, 0) {
            Some(d) => d,
            None => return -40,
        }
    } else {
        dentry
    };

    let st = crate::fs::inode::stat_dentry(&dentry);
    if super::process::write_user_value(&task, stat_buf as *mut Kstat, &st) {
        0
    } else {
        -14
    }
}

pub fn sys_statx(
    dirfd: isize,
    path: *const u8,
    flags: u32,
    mask: u32,
    statx_buf: *mut u8,
) -> isize {
    const AT_EMPTY_PATH: u32 = 0x1000;
    const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
    const AT_NO_AUTOMOUNT: u32 = 0x800;
    const AT_STATX_FORCE_SYNC: u32 = 0x2000;
    const AT_STATX_DONT_SYNC: u32 = 0x4000;
    const STATX_KNOWN_MASK: u32 = 0x3fff;

    if !valid_user_write_buf(statx_buf, core::mem::size_of::<Statx>()) {
        return -14;
    }
    if flags
        & !(AT_EMPTY_PATH
            | AT_SYMLINK_NOFOLLOW
            | AT_NO_AUTOMOUNT
            | AT_STATX_FORCE_SYNC
            | AT_STATX_DONT_SYNC)
        != 0
    {
        return -22;
    }
    if mask & !STATX_KNOWN_MASK != 0 {
        return -22;
    }
    let task = current_task().unwrap();

    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path_str) {
        return -36;
    }

    if path_str.is_empty() && flags & AT_EMPTY_PATH != 0 {
        let fd = dirfd as usize;
        let inner = task.inner_exclusive_access();
        if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
            return -9;
        }
        let file = inner.fd_table[fd].as_ref().unwrap().clone();
        drop(inner);
        let st = file.stat();
        return if write_statx(&task, statx_buf, &st, mask) {
            0
        } else {
            -14
        };
    }

    if path_str.is_empty() {
        return -2;
    }
    if let Some(st) = proc_exe_stat(path_str.as_str(), flags & AT_SYMLINK_NOFOLLOW != 0) {
        return if write_statx(&task, statx_buf, &st, mask) {
            0
        } else {
            -14
        };
    }
    if let Some(st) = special_device_stat(path_str.as_str()) {
        return if write_statx(&task, statx_buf, &st, mask) {
            0
        } else {
            -14
        };
    }

    let base = match resolve_base_dir_for_path(dirfd, &path_str) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if let Err(errno) = check_traverse_to_leaf(&base, &path_str, euid, egid) {
        return errno;
    }
    let dentry = match resolve_path_checked(&base, &path_str) {
        Ok(d) => d,
        Err(errno) => return errno,
    };
    let dentry = if flags & AT_SYMLINK_NOFOLLOW == 0 {
        match crate::fs::follow_symlink(&base, dentry, 0) {
            Some(d) => d,
            None => return -40,
        }
    } else {
        dentry
    };

    let st = crate::fs::inode::stat_dentry(&dentry);
    if write_statx(&task, statx_buf, &st, mask) {
        0
    } else {
        -14
    }
}

fn write_statx(
    task: &Arc<TaskControlBlock>,
    buf: *mut u8,
    st: &Kstat,
    mask: u32,
) -> bool {
    let statx = Statx {
        stx_mask: mask & 0x7ff,
        stx_blksize: st.st_blksize,
        stx_attributes: 0,
        stx_nlink: st.st_nlink,
        stx_uid: st.st_uid,
        stx_gid: st.st_gid,
        stx_mode: st.st_mode as u16,
        pad1: 0,
        stx_ino: st.st_ino,
        stx_size: st.st_size as u64,
        stx_blocks: st.st_blocks as u64,
        stx_attributes_mask: 0,
        stx_atime: StatxTime {
            tv_sec: st.st_atime_sec,
            tv_nsec: st.st_atime_nsec as u32,
            pad: 0,
        },
        stx_btime: StatxTime {
            tv_sec: 0,
            tv_nsec: 0,
            pad: 0,
        },
        stx_ctime: StatxTime {
            tv_sec: st.st_ctime_sec,
            tv_nsec: st.st_ctime_nsec as u32,
            pad: 0,
        },
        stx_mtime: StatxTime {
            tv_sec: st.st_mtime_sec,
            tv_nsec: st.st_mtime_nsec as u32,
            pad: 0,
        },
        stx_rdev_major: 0,
        stx_rdev_minor: 0,
        stx_dev_major: 0,
        stx_dev_minor: 0,
    };
    super::process::write_user_value(task, buf as *mut Statx, &statx)
}

#[repr(C)]
struct StatxTime {
    tv_sec: i64,
    tv_nsec: u32,
    pad: i32,
}

#[repr(C)]
struct Statx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    pad1: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: StatxTime,
    stx_btime: StatxTime,
    stx_ctime: StatxTime,
    stx_mtime: StatxTime,
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
}

pub fn sys_fstat(fd: usize, stat_buf: *mut u8) -> isize {
    if !valid_user_write_buf(stat_buf, core::mem::size_of::<Kstat>()) {
        return -14; // EFAULT
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9; // EBADF
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        drop(inner);
        let st = file.stat();
        if super::process::write_user_value(&task, stat_buf as *mut Kstat, &st) {
            0
        } else {
            -14
        }
    } else {
        -9 // EBADF
    }
}

pub fn sys_getdents64(fd: usize, dirp: *mut u8, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    if dirp.is_null() {
        return -1;
    }
    let task = current_task().unwrap();
    let token = current_user_token();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        drop(inner);
        let mut staging = match copy_to_user_after_io(&task, token, dirp, len) {
            Ok(staging) => staging,
            Err(errno) => return errno,
        };
        let count = file.getdents64(staging.as_mut_slice());
        if count <= 0 {
            return count;
        }
        let count = count as usize;
        if count > len {
            return -5;
        }
        if let Err(errno) =
            copy_staged_output_to_user(&task, token, dirp, &staging.as_slice()[..count])
        {
            return errno;
        }
        count as isize
    } else {
        -1
    }
}

// ==================== mount / umount ====================

use crate::fs::vfs::{vfs_mount, vfs_umount};

fn parse_tmpfs_size(data: &str) -> Option<u64> {
    for option in data.split(',') {
        let Some(value) = option.strip_prefix("size=") else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let mut digits_end = 0usize;
        for byte in value.as_bytes() {
            if byte.is_ascii_digit() {
                digits_end += 1;
            } else {
                break;
            }
        }
        if digits_end == 0 {
            return None;
        }
        let number = value[..digits_end].parse::<u64>().ok()?;
        let suffix = value[digits_end..].trim();
        let multiplier = match suffix
            .as_bytes()
            .first()
            .copied()
            .map(|b| b.to_ascii_lowercase())
        {
            Some(b'k') => 1024,
            Some(b'm') => 1024 * 1024,
            Some(b'g') => 1024 * 1024 * 1024,
            Some(b'%') => return None,
            Some(_) => 1,
            None => 1,
        };
        return number.checked_mul(multiplier);
    }
    None
}

pub fn sys_mount(
    source: *const u8,
    target: *const u8,
    fstype: *const u8,
    mountflags: usize,
    data: *const u8,
) -> isize {
    const MS_RDONLY: usize = 1;
    const MS_REMOUNT: usize = 32;

    if target.is_null() {
        return -14;
    }
    if fstype.is_null() {
        return -22;
    }
    let target_path = match translated_path_bounded(target) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let fstype_str = match translated_path_bounded(fstype) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let source_str = if source.is_null() {
        String::new()
    } else {
        match translated_path_bounded(source) {
            Ok(path) => path,
            Err(errno) => return errno,
        }
    };

    let fstype_name = fstype_str.trim_end_matches('\0');
    let source_name = source_str.trim_end_matches('\0');
    #[cfg(feature = "mount-trace")]
    crate::println!(
        "[mount-trace] syscall-enter source={} target={} fs={}",
        source_name,
        target_path.trim_end_matches('\0'),
        fstype_str.trim_end_matches('\0')
    );
    #[cfg(feature = "smp-diagnostics")]
    crate::println!(
        "[mount-diag] syscall tid={} source={} target={} fs={}",
        current_task().map(|task| task.getpid()).unwrap_or(0),
        source_name,
        target_path.trim_end_matches('\0'),
        fstype_name
    );

    let task = current_task().unwrap();
    let cwd = {
        let inner = task.inner_exclusive_access();
        inner.cwd.clone()
    };

    #[cfg(feature = "mount-watchdog")]
    crate::fs::vfs::mount_watchdog_begin();
    #[cfg(feature = "mount-watchdog")]
    crate::fs::vfs::mount_watchdog_stage(2);
    let mnt_point = match vfs_resolve_path(&cwd, target_path.as_str()) {
        Some(d) => d,
        None => {
            #[cfg(feature = "mount-watchdog")]
            crate::fs::vfs::mount_watchdog_finish();
            return -1;
        }
    };
    #[cfg(feature = "mount-watchdog")]
    crate::fs::vfs::mount_watchdog_stage(3);
    #[cfg(feature = "mount-trace")]
    crate::println!("[mount-trace] path-resolved point={}", mnt_point.path);

    if !mnt_point.is_dir {
        #[cfg(feature = "mount-watchdog")]
        crate::fs::vfs::mount_watchdog_finish();
        return -20;
    }

    if fstype_name == "tmpfs" {
        let data_str = if data.is_null() {
            String::new()
        } else {
            match translated_path_bounded(data) {
                Ok(data) => data,
                Err(errno) => return errno,
            }
        };
        let data_name = data_str.trim_end_matches('\0');
        vfs_set_tmpfs_mount(&mnt_point.path, parse_tmpfs_size(data_name));
        vfs_set_readonly_mount(&mnt_point.path, mountflags & MS_RDONLY != 0);
        #[cfg(feature = "mount-watchdog")]
        crate::fs::vfs::mount_watchdog_finish();
        return 0;
    }

    if mountflags & MS_REMOUNT != 0 {
        vfs_set_readonly_mount(&mnt_point.path, mountflags & MS_RDONLY != 0);
        #[cfg(feature = "mount-watchdog")]
        crate::fs::vfs::mount_watchdog_finish();
        return 0;
    }

    if fstype_name != "ext4" && fstype_name != "vfat" {
        #[cfg(feature = "mount-watchdog")]
        crate::fs::vfs::mount_watchdog_finish();
        return -19;
    }

    if fstype_name == "vfat" && matches!(source_name, "/dev/vda2" | "vda2") {
        #[cfg(feature = "mount-watchdog")]
        crate::fs::vfs::mount_watchdog_finish();
        return 0;
    }

    // 如果已经挂载，幂等成功
    use crate::fs::vfs::MOUNT_TABLE;
    {
        let table = MOUNT_TABLE.exclusive_access();
        let found = table.iter().any(|m| m.mnt_point.path == mnt_point.path);
        if found {
            #[cfg(feature = "mount-watchdog")]
            crate::fs::vfs::mount_watchdog_finish();
            return 0;
        }
    }
    #[cfg(feature = "mount-watchdog")]
    crate::fs::vfs::mount_watchdog_stage(4);

    let blk_dev = match source_name {
        "" => crate::drivers::block::get_block_device(0),
        name => match block_device_index(name) {
            Some(index) => crate::drivers::block::get_block_device(index),
            None if name.starts_with("/dev/") => return -15,
            None => return -2,
        },
    };
    let Some(blk_dev) = blk_dev else {
        #[cfg(feature = "mount-watchdog")]
        crate::fs::vfs::mount_watchdog_finish();
        return -19;
    };
    #[cfg(feature = "mount-watchdog")]
    crate::fs::vfs::mount_watchdog_stage(5);
    #[cfg(feature = "mount-trace")]
    crate::println!("[mount-trace] block-device-selected source={}", source_name);
    let mut lwext4_mount_path =
        String::from(target_path.trim_end_matches('\0').trim_end_matches('/'));
    if lwext4_mount_path.is_empty() {
        lwext4_mount_path.push('/');
    } else if lwext4_mount_path != "/" {
        lwext4_mount_path.push('/');
    }

    #[cfg(feature = "mount-trace")]
    crate::println!("[mount-trace] create-super-begin point={}", lwext4_mount_path);
    #[cfg(feature = "smp-diagnostics")]
    crate::println!("[mount-diag] create-super point={}", lwext4_mount_path);
    let sb = match VfsSuperBlock::new_ext4_at(blk_dev, "ext4", lwext4_mount_path.as_str()) {
        Some(sb) => sb,
        None => {
            #[cfg(feature = "mount-watchdog")]
            crate::fs::vfs::mount_watchdog_finish();
            return -17;
        }
    };

    #[cfg(feature = "mount-trace")]
    crate::println!("[mount-trace] create-super-done point={}", mnt_point.path);
    #[cfg(feature = "smp-diagnostics")]
    crate::println!("[mount-diag] publish point={}", mnt_point.path);
    let mounted = vfs_mount(mnt_point.clone(), sb, lwext4_mount_path.as_str());
    #[cfg(feature = "mount-watchdog")]
    crate::fs::vfs::mount_watchdog_finish();
    if mounted {
        vfs_set_readonly_mount(&mnt_point.path, mountflags & MS_RDONLY != 0);
        0
    } else {
        -16
    }
}

pub fn sys_umount2(target: *const u8, _flags: usize) -> isize {
    let target_path = match translated_path_bounded(target) {
        Ok(path) => path,
        Err(errno) => return errno,
    };

    let task = current_task().unwrap();
    let cwd = {
        let inner = task.inner_exclusive_access();
        inner.cwd.clone()
    };

    use crate::fs::vfs::{vfs_resolve_path, MOUNT_TABLE};

    let dentry = match vfs_resolve_path(&cwd, target_path.as_str()) {
        Some(d) => d,
        None => return -1,
    };

    let path = &dentry.path;
    let mut table = MOUNT_TABLE.exclusive_access();
    if let Some(idx) = table.iter().position(|m| &m.mnt_point.path == path) {
        let mount = table.remove(idx);
        mount.mnt_root.super_block.force_umount();
    }
    drop(table);
    vfs_clear_tmpfs_mount(path);
    vfs_set_readonly_mount(path, false);
    0
}

#[repr(C)]
struct StatFs {
    f_type: i64,
    f_bsize: i64,
    f_blocks: i64,
    f_bfree: i64,
    f_bavail: i64,
    f_files: i64,
    f_ffree: i64,
    f_fsid: [i32; 2],
    f_namelen: i64,
    f_frsize: i64,
    f_flags: i64,
    f_spare: [i64; 4],
}

const EXT4_SUPER_MAGIC: i64 = 0xEF53;
const PIPEFS_MAGIC: i64 = 0x50495045;
const PROC_SUPER_MAGIC: i64 = 0x9fa0;
const SOCKFS_MAGIC: i64 = 0x534f434b;
const SYSFS_MAGIC: i64 = 0x62656572;
const TMPFS_MAGIC: i64 = 0x01021994;

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn statfs_from_dentry(dentry: &Arc<Dentry>) -> StatFs {
    let stats = dentry.super_block.get_fs_stats();
    let block_size = stats.block_size as i64;
    let blocks = stats.blocks_count as i64;
    let free_blocks = stats.free_blocks_count as i64;
    let inodes = stats.inodes_count as i64;
    let free_inodes = stats.free_inodes_count as i64;
    StatFs {
        f_type: EXT4_SUPER_MAGIC,
        f_bsize: block_size,
        f_blocks: blocks,
        f_bfree: free_blocks,
        f_bavail: free_blocks,
        f_files: inodes,
        f_ffree: free_inodes,
        f_fsid: [0, 0],
        f_namelen: 255,
        f_frsize: block_size,
        f_flags: 0,
        f_spare: [0; 4],
    }
}

fn pseudo_statfs(f_type: i64) -> StatFs {
    StatFs {
        f_type,
        f_bsize: 4096,
        f_blocks: 0,
        f_bfree: 0,
        f_bavail: 0,
        f_files: 0,
        f_ffree: 0,
        f_fsid: [0, 0],
        f_namelen: 255,
        f_frsize: 4096,
        f_flags: 0,
        f_spare: [0; 4],
    }
}

fn pseudo_statfs_magic_for_path(path: &str) -> Option<i64> {
    if path == "/proc" || path.starts_with("/proc/") {
        return Some(PROC_SUPER_MAGIC);
    }
    if path == "/sys" || path.starts_with("/sys/") {
        return Some(SYSFS_MAGIC);
    }
    if path == "/dev" || path.starts_with("/dev/") {
        return Some(TMPFS_MAGIC);
    }
    None
}

fn pseudo_statfs_magic_for_mode(mode: u32) -> i64 {
    match mode & 0o170000 {
        crate::fs::S_IFIFO => PIPEFS_MAGIC,
        crate::fs::S_IFSOCK => SOCKFS_MAGIC,
        crate::fs::S_IFCHR | crate::fs::S_IFBLK => TMPFS_MAGIC,
        _ => TMPFS_MAGIC,
    }
}

pub fn sys_statfs(path: *const u8, buf: *mut u8) -> isize {
    if !valid_user_write_buf(buf, core::mem::size_of::<StatFs>()) {
        return -14;
    }
    let path = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path) {
        return -36;
    }
    if let Some(magic) = pseudo_statfs_magic_for_path(&path) {
        let task = current_task().unwrap();
        let st = pseudo_statfs(magic);
        return if super::process::write_user_value(&task, buf as *mut StatFs, &st) {
            0
        } else {
            -14
        };
    }
    let task = current_task().unwrap();
    let (cwd, euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.cwd.clone(), inner.euid, inner.egid)
    };
    if let Err(errno) = check_traverse_to_leaf(&cwd, path.as_str(), euid, egid) {
        return errno;
    }
    let dentry = match resolve_path_checked(&cwd, path.as_str()) {
        Ok(d) => d,
        Err(errno) => return errno,
    };
    let dentry = if dentry.is_link {
        match follow_symlink(&cwd, dentry, 0) {
            Some(target) => target,
            None => return -40,
        }
    } else {
        dentry
    };
    let st = statfs_from_dentry(&dentry);
    if super::process::write_user_value(&task, buf as *mut StatFs, &st) {
        0
    } else {
        -14
    }
}

pub fn sys_pread64(fd: usize, buf: *mut u8, len: usize, offset: usize) -> isize {
    if offset > isize::MAX as usize {
        return -22;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    let inode = file.as_any().downcast_ref::<crate::fs::inode::OSInode>();
    if inode.is_none() {
        return -29;
    }
    if file.is_dir() {
        return -21;
    }
    if !file.readable() {
        return -9;
    }
    drop(inner);
    let mut total = 0usize;
    loop {
        let chunk_len = (len - total).min(MAX_IO_STAGING_SIZE);
        let Some(chunk_addr) = (buf as usize).checked_add(total) else {
            return if total == 0 { -14 } else { total as isize };
        };
        let Some(chunk_offset) = offset.checked_add(total) else {
            return if total == 0 { -22 } else { total as isize };
        };
        let mut staging = match copy_to_user_after_io(
            &task,
            token,
            chunk_addr as *mut u8,
            chunk_len,
        ) {
            Ok(staging) => staging,
            Err(errno) => return if total == 0 { errno } else { total as isize },
        };
        let read = file.read_at(chunk_offset, staging.as_mut_slice());
        if read < 0 {
            return if total == 0 { read } else { total as isize };
        }
        let read = read as usize;
        if read > staging.as_slice().len() {
            return if total == 0 { -5 } else { total as isize };
        }
        if let Err(errno) = copy_staged_output_to_user(
            &task,
            token,
            chunk_addr as *mut u8,
            &staging.as_slice()[..read],
        ) {
            return if total == 0 { errno } else { total as isize };
        }
        total += read;
        if read < staging.as_slice().len() || total == len {
            return total as isize;
        }
    }
}

pub fn sys_pwrite64(fd: usize, buf: *mut u8, len: usize, offset: usize) -> isize {
    if offset > isize::MAX as usize {
        return -22;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    let inode = file.as_any().downcast_ref::<crate::fs::inode::OSInode>();
    if inode.is_none() {
        return -29;
    }
    if file.is_dir() {
        return -21;
    }
    if !file.writable() {
        return -9;
    }
    drop(inner);
    let mut total = 0usize;
    loop {
        let chunk_len = (len - total).min(MAX_IO_STAGING_SIZE);
        let Some(chunk_addr) = (buf as usize).checked_add(total) else {
            return if total == 0 { -14 } else { total as isize };
        };
        let Some(chunk_offset) = offset.checked_add(total) else {
            return if total == 0 { -22 } else { total as isize };
        };
        let mut staging = match copy_from_user_for_io(
            &task,
            token,
            chunk_addr as *mut u8,
            chunk_len,
        ) {
            Ok(staging) => staging,
            Err(errno) => return if total == 0 { errno } else { total as isize },
        };
        let staged_len = staging.as_slice().len();
        let written = file.write_at(chunk_offset, staging.as_mut_slice());
        if written < 0 {
            return if total == 0 { written } else { total as isize };
        }
        let written = written as usize;
        if written > staged_len {
            return if total == 0 { -5 } else { total as isize };
        }
        total += written;
        if written < staged_len || total == len {
            return total as isize;
        }
    }
}

pub fn sys_preadv(fd: usize, iov: *const u8, iovcnt: usize, offset: usize) -> isize {
    const IOV_MAX: usize = 1024;
    if offset > isize::MAX as usize {
        return -22;
    }
    if iovcnt > IOV_MAX {
        return -22;
    }
    if iovcnt == 0 {
        return 0;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(
            iov,
            iovcnt * core::mem::size_of::<super::process::UserIoVec>(),
            false,
        )
    {
        return -14;
    }
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    let inode = file.as_any().downcast_ref::<crate::fs::inode::OSInode>();
    if inode.is_none() {
        return -29;
    }
    if file.is_dir() {
        return -21;
    }
    if !file.readable() {
        return -9;
    }
    drop(inner);
    let (entries, requested) = match super::process::load_user_iovecs(&task, iov, iovcnt) {
        Ok(entries) => entries,
        Err(errno) => return errno,
    };
    if requested == 0 {
        return 0;
    }
    let mut total = 0usize;
    let mut index = 0usize;
    let mut iov_offset = 0usize;
    while total < requested {
        let staging_len = (requested - total).min(MAX_IO_STAGING_SIZE);
        let prepared = super::process::prepare_iovec_output_prefix(
            &task,
            &entries,
            index,
            iov_offset,
            staging_len,
        );
        if prepared == 0 {
            return if total == 0 { -14 } else { total as isize };
        }
        let mut staging = match super::process::vectored_staging(prepared) {
            Ok(staging) => staging,
            Err(errno) => return if total == 0 { errno } else { total as isize },
        };
        let Some(file_offset) = offset.checked_add(total) else {
            return if total == 0 { -22 } else { total as isize };
        };
        let n = file.read_at(file_offset, staging.as_mut_slice());
        if n < 0 {
            return if total == 0 { n } else { total as isize };
        }
        let n = n as usize;
        if n > prepared {
            return if total == 0 { -5 } else { total as isize };
        }
        if !super::process::copy_to_iovecs(
            &task,
            &entries,
            index,
            iov_offset,
            &staging.as_slice()[..n],
        ) {
            return if total == 0 { -14 } else { total as isize };
        }
        super::process::advance_iovec_cursor(
            &entries,
            &mut index,
            &mut iov_offset,
            n,
        );
        total += n;
        if n < prepared || prepared < staging_len {
            return total as isize;
        }
    }
    total as isize
}

pub fn sys_pwritev(fd: usize, iov: *const u8, iovcnt: usize, offset: usize) -> isize {
    const IOV_MAX: usize = 1024;
    if offset > isize::MAX as usize {
        return -22;
    }
    if iovcnt > IOV_MAX {
        return -22;
    }
    if iovcnt == 0 {
        return 0;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(
            iov,
            iovcnt * core::mem::size_of::<super::process::UserIoVec>(),
            false,
        )
    {
        return -14;
    }
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    let inode = file.as_any().downcast_ref::<crate::fs::inode::OSInode>();
    if inode.is_none() {
        return -29;
    }
    if file.is_dir() {
        return -21;
    }
    if !file.writable() {
        return -9;
    }
    drop(inner);
    let (entries, requested) = match super::process::load_user_iovecs(&task, iov, iovcnt) {
        Ok(entries) => entries,
        Err(errno) => return errno,
    };
    if requested == 0 {
        return 0;
    }
    let mut total = 0usize;
    let mut index = 0usize;
    let mut iov_offset = 0usize;
    while total < requested {
        let staging_len = (requested - total).min(MAX_IO_STAGING_SIZE);
        let mut staging = match super::process::vectored_staging(staging_len) {
            Ok(staging) => staging,
            Err(errno) => return if total == 0 { errno } else { total as isize },
        };
        let staged = super::process::copy_from_iovecs_prefix(
            &task,
            &entries,
            index,
            iov_offset,
            staging.as_mut_slice(),
        );
        if staged == 0 {
            return if total == 0 { -14 } else { total as isize };
        }
        let Some(file_offset) = offset.checked_add(total) else {
            return if total == 0 { -22 } else { total as isize };
        };
        let n = file.write_at(file_offset, &mut staging.as_mut_slice()[..staged]);
        if n < 0 {
            return if total == 0 { n } else { total as isize };
        }
        let n = n as usize;
        if n > staged {
            return if total == 0 { -5 } else { total as isize };
        }
        super::process::advance_iovec_cursor(
            &entries,
            &mut index,
            &mut iov_offset,
            n,
        );
        total += n;
        if n < staged || staged < staging_len {
            return total as isize;
        }
    }
    total as isize
}

pub fn sys_preadv2(fd: usize, iov: *const u8, iovcnt: usize, offset: usize, flags: usize) -> isize {
    if flags != 0 {
        return -95;
    }
    if offset == usize::MAX {
        return super::process::sys_readv(fd, iov, iovcnt);
    }
    sys_preadv(fd, iov, iovcnt, offset)
}

pub fn sys_pwritev2(
    fd: usize,
    iov: *const u8,
    iovcnt: usize,
    offset: usize,
    flags: usize,
) -> isize {
    if flags != 0 {
        return -95;
    }
    if offset == usize::MAX {
        return super::process::sys_writev(fd, iov, iovcnt);
    }
    sys_pwritev(fd, iov, iovcnt, offset)
}

fn sync_fd(fd: usize) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    let inode = match file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        Some(inode) => {
            let mode_type = inode.stat().st_mode & 0o170000;
            if mode_type != S_IFREG && mode_type != S_IFDIR {
                return -22;
            }
            inode
        }
        None => return -22,
    };
    let dentry = inode.dentry().clone();
    drop(inner);
    if !inode.flush_pending_data() {
        return -5;
    }
    let rc = dentry.super_block.fsync(&dentry.fs_path);
    if rc == 0 {
        0
    } else {
        -5
    }
}

pub fn sys_fsync(fd: usize) -> isize {
    sync_fd(fd)
}

pub fn sys_fdatasync(fd: usize) -> isize {
    sync_fd(fd)
}

pub fn sys_sync() -> isize {
    let rc = crate::fs::vfs::sync_filesystems();
    if rc == 0 {
        0
    } else {
        -5
    }
}

pub fn sys_syncfs(fd: usize) -> isize {
    sync_fd(fd)
}

pub fn sys_fallocate(fd: usize, mode: usize, offset: usize, len: usize) -> isize {
    const FALLOC_FL_KEEP_SIZE: usize = 0x01;
    const EFBIG: isize = -27;
    if len == 0 {
        return -22;
    }
    if offset > isize::MAX as usize || len > isize::MAX as usize {
        return -22;
    }
    let Some(end) = offset.checked_add(len) else {
        return EFBIG;
    };
    if end > isize::MAX as usize {
        return EFBIG;
    }
    if mode & !FALLOC_FL_KEEP_SIZE != 0 {
        return -95;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    if !file.writable() {
        return -9;
    }
    if file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .is_none()
    {
        return -29;
    }
    if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        if vfs_dentry_is_readonly(inode.get_dentry()) {
            return EROFS;
        }
    }
    drop(inner);
    if mode & FALLOC_FL_KEEP_SIZE != 0 {
        return 0;
    }
    crate::syscall::fs::sys_ftruncate(fd, end as u64)
}

pub fn sys_fadvise64(fd: usize, _offset: usize, _len: usize, advice: usize) -> isize {
    const POSIX_FADV_NOREUSE: usize = 5;
    if advice > POSIX_FADV_NOREUSE {
        return -22;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    if file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .is_none()
    {
        return -29;
    }
    0
}

pub fn sys_fstatfs(fd: usize, buf: *mut u8) -> isize {
    if !valid_user_write_buf(buf, core::mem::size_of::<StatFs>()) {
        return -14;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    let file = match &inner.fd_table[fd] {
        Some(f) => f.clone(),
        None => return -9,
    };
    let dentry = file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .map(|inode| inode.dentry());
    drop(inner);
    let st = match dentry {
        Some(d) => statfs_from_dentry(d),
        None => pseudo_statfs(pseudo_statfs_magic_for_mode(file.stat().st_mode)),
    };
    if super::process::write_user_value(&task, buf as *mut StatFs, &st) {
        0
    } else {
        -14
    }
}

pub fn sys_ftruncate(fd: usize, length: u64) -> isize {
    if length > isize::MAX as u64 {
        return -22;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    let file = match &inner.fd_table[fd] {
        Some(f) => f.clone(),
        None => return -9,
    };
    if !file.writable() {
        return -22;
    }
    if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        if vfs_dentry_is_readonly(inode.get_dentry()) {
            return EROFS;
        }
    }
    drop(inner);
    let dentry = file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .map(|inode| inode.dentry());
    match dentry {
        Some(d) => {
            let old_size = file.stat().st_size.max(0) as u64;
            if !vfs_tmpfs_can_resize(&d.path, &d.fs_path, old_size, length) {
                return -28;
            }
            let truncated = file
                .as_any()
                .downcast_ref::<crate::fs::inode::OSInode>()
                .map(|inode| inode.truncate(length))
                .unwrap_or(false);
            if truncated {
                crate::syscall::process::remove_shared_file_mappings(&d.fs_path);
                vfs_tmpfs_commit_resize(&d.path, &d.fs_path, old_size, length);
                0
            } else {
                -1
            }
        }
        None => -22,
    }
}

/// Linux `truncate(2)` follows the final symlink and requires write access to
/// the target.  Reuse the normal open/ftruncate path so path traversal,
/// permissions, read-only mounts, tmpfs accounting, mapping invalidation and
/// Ext4 cache refresh all retain the same semantics as `ftruncate(2)`.
pub fn sys_truncate(path: *const u8, length: u64) -> isize {
    const O_WRONLY: u32 = 1;

    if length > isize::MAX as u64 {
        return -22;
    }
    let fd = sys_openat(AT_FDCWD, path, O_WRONLY, 0);
    if fd < 0 {
        return fd;
    }
    let result = sys_ftruncate(fd as usize, length);
    let _ = sys_close(fd as usize);
    result
}

// ---- flock (32) ----

pub fn sys_flock(fd: usize, op: usize) -> isize {
    const LOCK_SH: usize = 1;
    const LOCK_EX: usize = 2;
    const LOCK_NB: usize = 4;
    const LOCK_UN: usize = 8;
    let op_kind = op & !LOCK_NB;
    if op_kind != LOCK_SH && op_kind != LOCK_EX && op_kind != LOCK_UN {
        return -22;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    0
}

// ---- mknodat (33) ----

pub fn sys_mknodat(dirfd: isize, path: *const u8, mode: u32, _dev: usize) -> isize {
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_str.is_empty() {
        return -2;
    }
    if path_has_too_long_component(&path_str) {
        return -36;
    }
    let base = match resolve_base_dir_for_path(dirfd, &path_str) {
        Ok(base) => base,
        Err(errno) => return errno,
    };
    if vfs_resolve_path(&base, &path_str).is_some() {
        return -17;
    }
    let (parent, name) = match resolve_parent_and_name_checked(&base, &path_str) {
        Ok(parts) => parts,
        Err(errno) => return errno,
    };
    if vfs_dentry_is_readonly(&parent) {
        return EROFS;
    }
    if !can_modify_directory(&parent) {
        return -13;
    }
    let create_mode = if mode == 0 { 0o666 } else { mode };
    let mode_type = create_mode & 0o170000;
    match mode_type {
        0 | crate::fs::S_IFREG | crate::fs::S_IFIFO | crate::fs::S_IFSOCK => {}
        crate::fs::S_IFCHR | crate::fs::S_IFBLK => {
            let euid = current_task()
                .map(|task| task.inner_exclusive_access().euid)
                .unwrap_or(1);
            if euid != 0 {
                return -1;
            }
        }
        _ => return -22,
    }
    if let Some(dentry) = crate::fs::vfs::vfs_create(&parent, &name, false, create_mode) {
        crate::fs::inode::record_special_path_mode(&dentry.fs_path, mode_type);
        0
    } else {
        -17
    }
}

// ---- readahead (213) ----

pub fn sys_readahead(fd: usize, _offset: usize, _count: usize) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    if !file.readable() {
        return -9;
    }
    if file.is_dir() {
        return -22;
    }
    if file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .is_none()
    {
        return -22;
    }
    0
}

// ---- copy_file_range (285) ----

pub fn sys_copy_file_range(
    fd_in: usize,
    off_in: *mut usize,
    fd_out: usize,
    off_out: *mut usize,
    len: usize,
    flags: usize,
) -> isize {
    const EBADF: isize = -9;
    const EFAULT: isize = -14;
    const EISDIR: isize = -21;
    const EINVAL: isize = -22;
    const EFBIG: isize = -27;
    const EOVERFLOW: isize = -75;
    const S_IFMT: u32 = 0o170000;

    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd_in >= inner.fd_table.len() || inner.fd_table[fd_in].is_none() {
        return EBADF;
    }
    if fd_out >= inner.fd_table.len() || inner.fd_table[fd_out].is_none() {
        return EBADF;
    }
    let word = core::mem::size_of::<usize>();
    if !off_in.is_null()
        && !inner
            .memory_set
            .valid_user_buf(off_in as *const u8, word, true)
    {
        return EFAULT;
    }
    if !off_out.is_null()
        && !inner
            .memory_set
            .valid_user_buf(off_out as *const u8, word, true)
    {
        return EFAULT;
    }
    if flags != 0 {
        return EINVAL;
    }
    let f_in = inner.fd_table[fd_in].as_ref().unwrap().clone();
    let f_out = inner.fd_table[fd_out].as_ref().unwrap().clone();
    drop(inner);
    let in_offset_ptr = if off_in.is_null() {
        None::<usize>
    } else {
        let Some(offset) = super::process::copy_user_value(&task, off_in as *const usize) else {
            return EFAULT;
        };
        Some(offset)
    };
    let out_offset_ptr = if off_out.is_null() {
        None::<usize>
    } else {
        let Some(offset) = super::process::copy_user_value(&task, off_out as *const usize) else {
            return EFAULT;
        };
        Some(offset)
    };

    let mode_in = f_in.stat().st_mode & S_IFMT;
    let mode_out = f_out.stat().st_mode & S_IFMT;
    if mode_in == crate::fs::S_IFDIR || mode_out == crate::fs::S_IFDIR {
        return EISDIR;
    }
    if mode_in != crate::fs::S_IFREG || mode_out != crate::fs::S_IFREG {
        return EINVAL;
    }
    if !f_in.readable() || !f_out.writable() {
        return EBADF;
    }
    if f_out.get_flags() & OpenFlags::APPEND.bits() as usize != 0 {
        return EBADF;
    }

    let in_start = in_offset_ptr.unwrap_or_else(|| f_in.get_offset());
    let out_start = out_offset_ptr.unwrap_or_else(|| f_out.get_offset());
    if in_start > isize::MAX as usize || out_start > isize::MAX as usize {
        return EINVAL;
    }
    if len > isize::MAX as usize || out_start.checked_add(len).is_none() {
        return EOVERFLOW;
    }
    let Some(out_end) = out_start.checked_add(len) else {
        return EOVERFLOW;
    };
    if out_end > isize::MAX as usize {
        return EFBIG;
    }
    if len == 0 {
        return 0;
    }

    if let (Some(in_path), Some(out_path)) = (f_in.path(), f_out.path()) {
        if in_path == out_path {
            let in_end = in_start.saturating_add(len);
            let out_end = out_start.saturating_add(len);
            if in_start < out_end && out_start < in_end {
                return EINVAL;
            }
        }
    }

    let mut buf = [0u8; 4096];
    let mut total: isize = 0;
    while (total as usize) < len {
        let chunk = (len - total as usize).min(buf.len());
        let in_pos = in_start + total as usize;
        let out_pos = out_start + total as usize;
        let n = f_in.read_at(in_pos, &mut buf[..chunk]);
        if n == 0 {
            break;
        }
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        let w = f_out.write_at(out_pos, &mut buf[..n as usize]);
        if w < 0 {
            return if total > 0 { total } else { w };
        }
        if w == 0 {
            break;
        }
        total += w;
        if w < n {
            break;
        }
    }
    let copied = total.max(0) as usize;
    if let Some(off) = in_offset_ptr {
        let new_offset = off + copied;
        if !super::process::write_user_value(&task, off_in, &new_offset) {
            return EFAULT;
        }
    } else {
        f_in.seek(in_start + copied);
    }
    if let Some(off) = out_offset_ptr {
        let new_offset = off + copied;
        if !super::process::write_user_value(&task, off_out, &new_offset) {
            return EFAULT;
        }
    } else {
        f_out.seek(out_start + copied);
    }
    total
}

// ---- memfd_create (279) ----

pub fn sys_memfd_create(_name: *const u8, _flags: usize) -> isize {
    let task = current_task().unwrap();
    install_fd_shared(&task, alloc::sync::Arc::new(crate::fs::DevNull), false)
        .map(|fd| fd as isize)
        .unwrap_or_else(|errno| errno)
}

// ---- close_range (436) ----

pub fn sys_close_range(first: usize, last: usize, flags: usize) -> isize {
    const CLOSE_RANGE_UNSHARE: usize = 1 << 1;
    const CLOSE_RANGE_CLOEXEC: usize = 1 << 2;
    if first > last {
        return -22;
    }
    if flags & !(CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC) != 0 {
        return -22;
    }
    let task = current_task().unwrap();
    let last = {
        let inner = task.inner_exclusive_access();
        last.min(inner.fd_table.len().saturating_sub(1))
    };
    if first > last {
        return 0;
    }
    if flags & CLOSE_RANGE_CLOEXEC != 0 {
        for fd in first..=last {
            let _ = set_fd_cloexec_shared(&task, fd, true);
        }
    } else {
        for fd in first..=last {
            let _ = close_fd_shared(&task, fd);
        }
    }
    0
}

// ---- socketpair (199) ----

pub fn sys_socketpair(domain: usize, typ: usize, protocol: usize, sv: *mut usize) -> isize {
    const AF_UNIX: usize = 1;
    const SOCK_STREAM: usize = 1;
    const SOCK_SEQPACKET: usize = 5;
    const SOCK_TYPE_MASK: usize = 0x0f;
    const SOCK_NONBLOCK: usize = 0x800;
    const SOCK_CLOEXEC: usize = 0x80000;

    if sv.is_null() || !valid_user_write_buf(sv as *mut u8, 2 * core::mem::size_of::<u32>()) {
        return -14;
    }
    if domain != AF_UNIX {
        return -97;
    }
    if protocol != 0 {
        return -93;
    }
    if typ & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return -22;
    }
    let kind = match typ & SOCK_TYPE_MASK {
        SOCK_STREAM => UnixSocketKind::Stream,
        SOCK_SEQPACKET => UnixSocketKind::SeqPacket,
        _ => return -94,
    };

    let nonblock = typ & SOCK_NONBLOCK != 0;
    let cloexec = typ & SOCK_CLOEXEC != 0;
    let (first, second) = make_unix_socket_pair(kind, nonblock);
    let first: Arc<dyn File + Send + Sync> = first;
    let second: Arc<dyn File + Send + Sync> = second;
    let task = current_task().unwrap();
    let first_fd = match install_fd_shared(&task, first, cloexec) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    let second_fd = match install_fd_shared(&task, second, cloexec) {
        Ok(fd) => fd,
        Err(errno) => {
            let _ = close_fd_shared(&task, first_fd);
            return errno;
        }
    };
    #[cfg(feature = "smp-diagnostics")]
    {
        let tgid = task.inner_exclusive_access().tgid;
        crate::println!(
            "[socketpair-diag] create tid={} tgid={} first_fd={} second_fd={} cloexec={} nonblock={}",
            task.getpid(),
            tgid,
            first_fd,
            second_fd,
            cloexec,
            nonblock,
        );
    }

    let mut result = [0u8; 8];
    result[..4].copy_from_slice(&(first_fd as u32).to_ne_bytes());
    result[4..].copy_from_slice(&(second_fd as u32).to_ne_bytes());
    if !super::process::copy_to_task_user(&task, sv as usize, &result) {
        let _ = close_fd_shared(&task, first_fd);
        let _ = close_fd_shared(&task, second_fd);
        return -14;
    }
    0
}
