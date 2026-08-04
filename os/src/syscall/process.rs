use crate::fs::stdio::stdout_release_if_owner;
use crate::fs::{open_file, open_file_at, File, OpenFlags};
use crate::mm::{
    frames_alloc, get_elf_interp_file, translated_byte_buffer, translated_ref, translated_refmut,
    translated_str, FrameTracker, MapArea, MapPermission, PageFaultAccess, PageFaultResult,
};
use crate::sync::{FutexLock, VmLock};
use crate::task::{
    add_priority_task, add_task, blank_kcontext, block_current_and_run_next, block_current_until,
    current_task, current_user_token, exit_current_and_run_next, flush_remote_tlb, force_exit_task,
    force_exit_task_group, has_interrupting_signal, online_cpu_mask, pid2task, pid_alloc, schedule,
    suspend_current_and_run_next, task_snapshot, terminate_other_threads_for_exec, wake_blocked_task, wake_signal_wait_task,
    BlockReason, KernelStack, PendingSignalInfo, SignalAction, SignalFlags, TaskControlBlock,
    TaskControlBlockInner, TaskStatus, MAX_SIG,
};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use lazy_static::lazy_static;
use log::info;
use polyhal::irq::IRQ;
use polyhal::pagetable::MappingFlags;
use polyhal::time::Time;
use polyhal_trap::trapframe::TrapFrameArgs;

struct ShmSegment {
    key: usize,
    size: usize,
    frames: Vec<FrameTracker>,
}

struct SharedFileMapping {
    path: String,
    offset: usize,
    len: usize,
    frames: Vec<FrameTracker>,
}

const SHARED_FILE_MAPPING_CACHE_MAX_PAGES: usize = 4096;
/// Keep the vector capacity fixed so publishing or evicting a cache entry
/// never allocates while holding `Vm.shared_file_mappings`.
const SHARED_FILE_MAPPING_CACHE_MAX_ENTRIES: usize = 64;

lazy_static! {
    /// Metadata only: frame allocation and page-table changes are completed
    /// outside this lock before/after publishing a segment.
    static ref SHM_SEGMENTS: VmLock<Vec<Option<ShmSegment>>> =
        VmLock::new_named(Vec::new(), "Vm.shared_memory_segments");
    /// Metadata only.  Frame allocation and file I/O are deliberately done
    /// before acquiring this lock; it is held only to inspect, publish or
    /// evict complete immutable frame lists.
    static ref SHARED_FILE_MAPPINGS: VmLock<Vec<SharedFileMapping>> =
        VmLock::new_named(
            Vec::with_capacity(SHARED_FILE_MAPPING_CACHE_MAX_ENTRIES),
            "Vm.shared_file_mappings",
        );
    /// Futex key registration is serialized separately from the scheduler.
    /// A waiter holds this lock before taking SchedulerLock, so a FUTEX_WAKE
    /// cannot pass between publishing the key and publishing `Blocking`.
    static ref FUTEX_WAITERS: FutexLock<BTreeMap<FutexKey, Vec<Arc<TaskControlBlock>>>> =
        FutexLock::new_named(BTreeMap::new(), "Futex.waiters");
}

struct IrqRestoreGuard {
    enabled: bool,
}

impl IrqRestoreGuard {
    fn disable() -> Self {
        let enabled = IRQ::int_enabled();
        IRQ::int_disable();
        Self { enabled }
    }
}

impl Drop for IrqRestoreGuard {
    fn drop(&mut self) {
        if self.enabled {
            IRQ::int_enable();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FutexKey {
    namespace: usize,
    addr: usize,
}

static LAST_USER_TIME_US: AtomicU64 = AtomicU64::new(0);
static LAST_USER_TIMES_TICK: AtomicU64 = AtomicU64::new(0);
const USER_CLOCK_TICK_US: u64 = 10_000;
const USER_CLOCK_TICK_NS: usize = USER_CLOCK_TICK_US as usize * 1_000;
const NSEC_PER_SEC: usize = 1_000_000_000;
const NSEC_PER_USEC: usize = 1_000;

#[cfg(feature = "smp-diagnostics")]
const FUTEX_EVENT_CAPACITY: usize = 128;
#[cfg(feature = "smp-diagnostics")]
static FUTEX_EVENT_NEXT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp-diagnostics")]
static FUTEX_EVENT_TID: [AtomicUsize; FUTEX_EVENT_CAPACITY] =
    [const { AtomicUsize::new(0) }; FUTEX_EVENT_CAPACITY];
#[cfg(feature = "smp-diagnostics")]
static FUTEX_EVENT_OP: [AtomicUsize; FUTEX_EVENT_CAPACITY] =
    [const { AtomicUsize::new(0) }; FUTEX_EVENT_CAPACITY];
#[cfg(feature = "smp-diagnostics")]
static FUTEX_EVENT_NAMESPACE: [AtomicUsize; FUTEX_EVENT_CAPACITY] =
    [const { AtomicUsize::new(0) }; FUTEX_EVENT_CAPACITY];
#[cfg(feature = "smp-diagnostics")]
static FUTEX_EVENT_ADDR: [AtomicUsize; FUTEX_EVENT_CAPACITY] =
    [const { AtomicUsize::new(0) }; FUTEX_EVENT_CAPACITY];
#[cfg(feature = "smp-diagnostics")]
static FUTEX_EVENT_VAL: [AtomicUsize; FUTEX_EVENT_CAPACITY] =
    [const { AtomicUsize::new(0) }; FUTEX_EVENT_CAPACITY];
#[cfg(feature = "smp-diagnostics")]
static FUTEX_EVENT_RET: [AtomicUsize; FUTEX_EVENT_CAPACITY] =
    [const { AtomicUsize::new(0) }; FUTEX_EVENT_CAPACITY];

#[cfg(feature = "smp-diagnostics")]
fn record_futex_event(op: usize, key: FutexKey, val: usize, ret: isize) {
    let sequence = FUTEX_EVENT_NEXT.fetch_add(1, Ordering::AcqRel);
    let slot = sequence % FUTEX_EVENT_CAPACITY;
    FUTEX_EVENT_TID[slot].store(
        current_task().map(|task| task.getpid()).unwrap_or(0),
        Ordering::Relaxed,
    );
    FUTEX_EVENT_OP[slot].store(op, Ordering::Relaxed);
    FUTEX_EVENT_NAMESPACE[slot].store(key.namespace, Ordering::Relaxed);
    FUTEX_EVENT_ADDR[slot].store(key.addr, Ordering::Relaxed);
    FUTEX_EVENT_VAL[slot].store(val, Ordering::Relaxed);
    FUTEX_EVENT_RET[slot].store(ret as usize, Ordering::Relaxed);
}

fn page_align_up(len: usize) -> Option<usize> {
    len.checked_add(crate::config::PAGE_SIZE - 1)
        .map(|n| n & !(crate::config::PAGE_SIZE - 1))
}

fn fill_shared_file_frames(
    file: &dyn File,
    offset: usize,
    frames: &[FrameTracker],
    base_page: usize,
) {
    for (idx, frame) in frames.iter().enumerate() {
        let page = frame.paddr.slice_mut_with_len(crate::config::PAGE_SIZE);
        let file_off = offset + (base_page + idx) * crate::config::PAGE_SIZE;
        let _ = file.read_at(file_off, page);
    }
}

fn shared_file_mapping_cache_pages(mappings: &[SharedFileMapping]) -> usize {
    mappings.iter().map(|mapping| mapping.frames.len()).sum()
}

fn trim_shared_file_mapping_cache(
    mappings: &mut Vec<SharedFileMapping>,
    reserve_pages: usize,
    retired: &mut Vec<SharedFileMapping>,
) {
    while !mappings.is_empty()
        && shared_file_mapping_cache_pages(mappings).saturating_add(reserve_pages)
            > SHARED_FILE_MAPPING_CACHE_MAX_PAGES
    {
        // `retired` has fixed capacity prepared by the caller, so dropping
        // frame trackers (which can take the frame allocator lock) happens
        // after the cache lock is released.
        retired.push(mappings.remove(0));
    }
}

fn load_shared_file_frames_uncached(
    file: &dyn File,
    offset: usize,
    pages: usize,
) -> Option<Vec<FrameTracker>> {
    let frames = frames_alloc(pages)?;
    fill_shared_file_frames(file, offset, &frames, 0);
    Some(frames)
}

/// Clone a complete cached mapping while holding only the VM-cache lock.
/// The result vector is allocated before taking that lock.  No file operation
/// or physical-frame allocation is allowed in this critical section.
fn clone_cached_shared_file_frames(
    path: &str,
    offset: usize,
    pages: usize,
) -> Option<Vec<FrameTracker>> {
    let mut result = Vec::with_capacity(pages);
    let mappings = SHARED_FILE_MAPPINGS.exclusive_access();
    let mapping = mappings.iter().find(|mapping| {
        mapping.path == path && mapping.offset == offset && mapping.frames.len() >= pages
    })?;
    for frame in mapping.frames.iter().take(pages) {
        result.push(frame.shared_clone());
    }
    Some(result)
}

/// Publish a fully populated mapping.  `frames` was allocated and filled
/// before this call.  If another hart published an equivalent mapping first,
/// the caller simply receives its independently loaded frames; duplicate work
/// is preferable to performing I/O under a spin lock.
fn publish_shared_file_mapping(
    path: String,
    offset: usize,
    len: usize,
    frames: Vec<FrameTracker>,
) -> Vec<FrameTracker> {
    let pages = frames.len();
    let mut result = Vec::with_capacity(pages);
    for frame in &frames {
        result.push(frame.shared_clone());
    }

    // The cache can contain at most this many entries, so all evicted entries
    // fit without allocation and are dropped after leaving the critical
    // section.
    let mut retired = Vec::with_capacity(SHARED_FILE_MAPPING_CACHE_MAX_ENTRIES);
    let mut frames = Some(frames);
    {
        let mut mappings = SHARED_FILE_MAPPINGS.exclusive_access();
        if mappings
            .iter()
            .any(|mapping| mapping.path == path && mapping.offset == offset && mapping.len >= len)
        {
            // A concurrent publisher won.  Return our independently loaded
            // frame list and let the unused shared clones drop outside the
            // cache lock.
            drop(mappings);
            drop(result);
            return frames.take().unwrap();
        }

        if let Some(index) = mappings
            .iter()
            .position(|mapping| mapping.path == path && mapping.offset == offset)
        {
            retired.push(mappings.remove(index));
        }
        trim_shared_file_mapping_cache(&mut mappings, pages, &mut retired);
        if mappings.len() == SHARED_FILE_MAPPING_CACHE_MAX_ENTRIES {
            retired.push(mappings.remove(0));
        }
        mappings.push(SharedFileMapping {
            path,
            offset,
            len,
            frames: frames.take().unwrap(),
        });
    }
    drop(retired);
    result
}

fn shared_file_mapping_frames(
    file: Arc<dyn File>,
    offset: usize,
    len: usize,
) -> Option<Vec<FrameTracker>> {
    let path = file.path()?;
    let len = page_align_up(len)?;
    let pages = len / crate::config::PAGE_SIZE;
    if pages == 0 {
        return None;
    }
    if pages > SHARED_FILE_MAPPING_CACHE_MAX_PAGES {
        return load_shared_file_frames_uncached(file.as_ref(), offset, pages);
    }

    if let Some(frames) = clone_cached_shared_file_frames(&path, offset, pages) {
        return Some(frames);
    }

    // Slow path: both allocation and file reads occur outside VmLock.  A
    // racing mapper may do the same work, but cannot observe an incomplete
    // cache entry or block all other VM metadata on storage I/O.
    let frames = load_shared_file_frames_uncached(file.as_ref(), offset, pages)?;
    Some(publish_shared_file_mapping(path, offset, len, frames))
}

pub(crate) fn remove_shared_file_mappings(path: &str) {
    let mut retired = Vec::with_capacity(SHARED_FILE_MAPPING_CACHE_MAX_ENTRIES);
    {
        let mut mappings = SHARED_FILE_MAPPINGS.exclusive_access();
        let mut index = 0;
        while index < mappings.len() {
            if mappings[index].path == path {
                retired.push(mappings.remove(index));
            } else {
                index += 1;
            }
        }
    }
    // FrameTracker drops can acquire frame-accounting locks; keep them out of
    // the VM-cache critical section.
    drop(retired);
}

pub(crate) fn sync_thread_group_vm_metadata(
    owner_pid: usize,
    tgid: usize,
    program_brk: usize,
    areas: &[MapArea],
) {
    let Some(owner) = pid2task(owner_pid) else {
        return;
    };
    let owner_address_space = {
        let inner = owner.inner_exclusive_access();
        inner.memory_set.address_space_id()
    };
    let tasks = task_snapshot();
    for task in tasks {
        if task.getpid() == owner_pid {
            continue;
        }
        let Some(mut inner) = task.try_inner_exclusive_access() else {
            continue;
        };
        if inner.tgid != tgid
            || !inner.memory_set.is_shared()
            || inner.memory_set.address_space_id() != owner_address_space
        {
            continue;
        }
        inner.program_brk = program_brk;
        inner.memory_set.replace_shared_metadata(areas);
    }
}

pub(crate) fn sync_thread_group_vm_frame(
    owner_pid: usize,
    tgid: usize,
    vpn: polyhal::VirtAddr,
    frame: &FrameTracker,
) {
    let Some(owner) = pid2task(owner_pid) else {
        return;
    };
    let owner_address_space = {
        let inner = owner.inner_exclusive_access();
        inner.memory_set.address_space_id()
    };
    let tasks = task_snapshot();
    for task in tasks {
        if task.getpid() == owner_pid {
            continue;
        }
        let Some(mut inner) = task.try_inner_exclusive_access() else {
            continue;
        };
        if inner.tgid != tgid
            || !inner.memory_set.is_shared()
            || inner.memory_set.address_space_id() != owner_address_space
        {
            continue;
        }
        inner.memory_set.replace_frame_metadata(vpn, frame);
    }
}

fn sync_thread_group_vm_frames(
    owner_pid: usize,
    tgid: usize,
    frames: &[(polyhal::VirtAddr, FrameTracker)],
) {
    if frames.is_empty() {
        return;
    }
    let Some(owner) = pid2task(owner_pid) else {
        return;
    };
    let owner_address_space = {
        let inner = owner.inner_exclusive_access();
        inner.memory_set.address_space_id()
    };
    let tasks = task_snapshot();
    for task in tasks {
        if task.getpid() == owner_pid {
            continue;
        }
        let Some(mut inner) = task.try_inner_exclusive_access() else {
            continue;
        };
        if inner.tgid != tgid
            || !inner.memory_set.is_shared()
            || inner.memory_set.address_space_id() != owner_address_space
        {
            continue;
        }
        for (vpn, frame) in frames.iter() {
            inner.memory_set.replace_frame_metadata(*vpn, frame);
        }
    }
}

fn sync_thread_group_vm_area_metadata(owner_pid: usize, tgid: usize, areas: &[MapArea]) {
    if areas.is_empty() {
        return;
    }
    let Some(owner) = pid2task(owner_pid) else {
        return;
    };
    let owner_address_space = {
        let inner = owner.inner_exclusive_access();
        inner.memory_set.address_space_id()
    };
    let tasks = task_snapshot();
    for task in tasks {
        if task.getpid() == owner_pid {
            continue;
        }
        let Some(mut inner) = task.try_inner_exclusive_access() else {
            continue;
        };
        if inner.tgid != tgid
            || !inner.memory_set.is_shared()
            || inner.memory_set.address_space_id() != owner_address_space
        {
            continue;
        }
        inner.memory_set.insert_shared_metadata_areas(areas);
    }
}

fn sync_thread_group_vm_unmap_metadata(
    owner_pid: usize,
    tgid: usize,
    start: polyhal::VirtAddr,
    end: polyhal::VirtAddr,
) {
    let Some(owner) = pid2task(owner_pid) else {
        return;
    };
    let owner_address_space = {
        let inner = owner.inner_exclusive_access();
        inner.memory_set.address_space_id()
    };
    let tasks = task_snapshot();
    for task in tasks {
        if task.getpid() == owner_pid {
            continue;
        }
        let Some(mut inner) = task.try_inner_exclusive_access() else {
            continue;
        };
        if inner.tgid != tgid
            || !inner.memory_set.is_shared()
            || inner.memory_set.address_space_id() != owner_address_space
        {
            continue;
        }
        inner.memory_set.munmap_metadata_only(start, end);
    }
}

fn thread_group_has_peer(owner_pid: usize, tgid: usize) -> bool {
    task_snapshot().into_iter().any(|task| {
        if task.getpid() == owner_pid {
            return false;
        }
        // Membership must not depend on a transient TaskLock contention: a
        // caller deciding whether it may replace shared VM state needs a
        // stable answer, not a best-effort snapshot.
        let inner = task.inner_exclusive_access();
        inner.tgid == tgid && inner.task_status != TaskStatus::Zombie
    })
}

fn sync_shared_address_space_brk(owner_pid: usize, address_space_id: usize, program_brk: usize) {
    for task in task_snapshot() {
        if task.getpid() == owner_pid {
            continue;
        }
        let Some(mut inner) = task.try_inner_exclusive_access() else {
            continue;
        };
        if inner.memory_set.is_shared() && inner.memory_set.address_space_id() == address_space_id {
            inner.program_brk = program_brk;
        }
    }
}

fn shared_vm_sync_snapshot(inner: &TaskControlBlockInner) -> Option<(usize, usize, Vec<MapArea>)> {
    if inner.memory_set.is_shared() {
        Some((
            inner.tgid,
            inner.program_brk,
            inner.memory_set.shared_metadata_snapshot(),
        ))
    } else {
        None
    }
}

fn sync_thread_group_vm_snapshot(owner_pid: usize, snapshot: Option<(usize, usize, Vec<MapArea>)>) {
    if let Some((tgid, program_brk, areas)) = snapshot {
        if thread_group_has_peer(owner_pid, tgid) {
            sync_thread_group_vm_metadata(owner_pid, tgid, program_brk, &areas);
        }
    }
}

fn sync_thread_group_vm_snapshot_and_frames(
    owner_pid: usize,
    snapshot: Option<(usize, usize, Vec<MapArea>)>,
    frames: &[(polyhal::VirtAddr, FrameTracker)],
) {
    let tgid = snapshot.as_ref().map(|(tgid, _, _)| *tgid);
    sync_thread_group_vm_snapshot(owner_pid, snapshot);
    if let Some(tgid) = tgid {
        sync_thread_group_vm_frames(owner_pid, tgid, frames);
    }
}

fn monotonic_user_time_us() -> u64 {
    let hw_us = (Time::now().to_nsec() / 1000) as u64;
    loop {
        let last = LAST_USER_TIME_US.load(Ordering::Relaxed);
        if hw_us <= last {
            return last;
        }
        if LAST_USER_TIME_US
            .compare_exchange(last, hw_us, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return hw_us;
        }
    }
}

fn monotonic_user_times_tick() -> u64 {
    let hw_tick = monotonic_user_time_us() / USER_CLOCK_TICK_US;
    loop {
        let last = LAST_USER_TIMES_TICK.load(Ordering::Relaxed);
        if hw_tick <= last {
            return last;
        }
        if LAST_USER_TIMES_TICK
            .compare_exchange(last, hw_tick, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return hw_tick;
        }
    }
}

const EXEC_PREFIX_LEN: usize = 512;

fn read_exec_prefix(inode: &Arc<crate::fs::inode::OSInode>) -> Vec<u8> {
    let mut buf = alloc::vec![0u8; EXEC_PREFIX_LEN];
    let n = inode.read_at(0, &mut buf);
    if n <= 0 {
        Vec::new()
    } else {
        buf.truncate(n as usize);
        buf
    }
}

pub fn user_realtime_sec() -> i64 {
    (monotonic_user_time_us() / 1_000_000) as i64
}

pub fn user_realtime_sec_ceil() -> i64 {
    ((monotonic_user_time_us() + 999_999) / 1_000_000) as i64
}

fn nsec_to_timeval(ns: usize) -> TimeVal {
    let usec = if ns == 0 {
        0
    } else {
        (ns + NSEC_PER_USEC - 1) / NSEC_PER_USEC
    };
    TimeVal {
        sec: (usec / 1_000_000) as isize,
        usec: (usec % 1_000_000) as isize,
    }
}

fn nsec_to_clock_ticks(ns: usize) -> isize {
    if ns == 0 {
        0
    } else {
        ((ns + USER_CLOCK_TICK_NS - 1) / USER_CLOCK_TICK_NS) as isize
    }
}

fn looks_like_text_script(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    let sample_len = data.len().min(256);
    let mut has_text = false;
    for &b in data[..sample_len].iter() {
        if b == 0 {
            return false;
        }
        if b == b'\n' || b == b'\r' || b == b'\t' || (b >= 0x20 && b <= 0x7e) {
            if b > b' ' {
                has_text = true;
            }
        } else {
            return false;
        }
    }
    has_text
}

const PROT_NONE: usize = 0;
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const PROT_EXEC: usize = 4;
const VALID_MMAP_PROT: usize = PROT_NONE | PROT_READ | PROT_WRITE | PROT_EXEC;

const MAP_SHARED: usize = 0x01;
const MAP_PRIVATE: usize = 0x02;
const MAP_SHARED_VALIDATE: usize = 0x03;
const MAP_TYPE_MASK: usize = 0x0f;
const MAP_FIXED: usize = 0x10;
const MAP_ANONYMOUS: usize = 0x20;
const MAP_FIXED_NOREPLACE: usize = 0x100000;

const UTSNAME_LEN: usize = 65;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Tms {
    pub tms_utime: isize,
    pub tms_stime: isize,
    pub tms_cutime: isize,
    pub tms_cstime: isize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UtsName {
    pub sysname: [u8; UTSNAME_LEN],
    pub nodename: [u8; UTSNAME_LEN],
    pub release: [u8; UTSNAME_LEN],
    pub version: [u8; UTSNAME_LEN],
    pub machine: [u8; UTSNAME_LEN],
    pub domainname: [u8; UTSNAME_LEN],
}

impl UtsName {
    fn set_str(field: &mut [u8; UTSNAME_LEN], s: &str) {
        let bytes = s.as_bytes();
        let len = bytes.len().min(UTSNAME_LEN - 1);
        field[..len].copy_from_slice(&bytes[..len]);
        field[len] = 0;
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct TimeVal {
    pub sec: isize,
    pub usec: isize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct TimeZone {
    minuteswest: i32,
    dsttime: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RUsage {
    ru_utime: TimeVal,
    ru_stime: TimeVal,
    ru_maxrss: isize,
    ru_ixrss: isize,
    ru_idrss: isize,
    ru_isrss: isize,
    ru_minflt: isize,
    ru_majflt: isize,
    ru_nswap: isize,
    ru_inblock: isize,
    ru_oublock: isize,
    ru_msgsnd: isize,
    ru_msgrcv: isize,
    ru_nsignals: isize,
    ru_nvcsw: isize,
    ru_nivcsw: isize,
}

const ROBUST_LIST_LEN: usize = 3 * core::mem::size_of::<usize>();
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
const FUTEX_WAITERS_BIT: u32 = 0x8000_0000;
const ROBUST_LIST_LIMIT: usize = 2048;
const MUSL_KILLLOCK_OFFSET_FROM_ROBUST_HEAD: usize = 0x28;
const MUSL_LOCKED_NO_WAITERS: u32 = 0x8000_0001;

pub(crate) fn prepare_task_user_access(
    task: &Arc<TaskControlBlock>,
    addr: usize,
    len: usize,
    need_write: bool,
) -> bool {
    if len == 0 {
        return true;
    }
    let Some(last) = addr.checked_add(len.saturating_sub(1)) else {
        return false;
    };
    let start = addr & !(crate::config::PAGE_SIZE - 1);
    let Some(end) = (last & !(crate::config::PAGE_SIZE - 1)).checked_add(crate::config::PAGE_SIZE)
    else {
        return false;
    };
    let mut changed_frames = Vec::new();
    let mut shared_activity = None;
    let mut shared_flush_start = usize::MAX;
    let mut shared_flush_end = 0usize;
    {
        let mut inner = task.inner_exclusive_access();
        if !inner
            .memory_set
            .valid_user_buf(addr as *const u8, len, need_write)
        {
            return false;
        }
        let mut page = start;
        while page < end {
            let vaddr = polyhal::VirtAddr::from(page);
            let needs_fault = match inner.memory_set.translate(vaddr) {
                Some((_, flags)) if need_write => {
                    !flags.contains(MappingFlags::P | MappingFlags::W)
                }
                Some((_, flags)) => !flags.contains(MappingFlags::P),
                None => true,
            };
            if needs_fault {
                let access = if need_write {
                    PageFaultAccess::Write
                } else {
                    PageFaultAccess::Read
                };
                let result = inner.memory_set.handle_page_fault(vaddr, access);
                match result {
                    Some(PageFaultResult::SharedMetadataChanged) => {
                        if let Some(frame) = inner.memory_set.clone_frame_metadata(vaddr.floor()) {
                            changed_frames.push((inner.tgid, vaddr.floor(), frame));
                        }
                    }
                    Some(PageFaultResult::Handled) => {}
                    None => return false,
                }
                if result.is_some() && inner.memory_set.is_shared() {
                    shared_activity = Some(inner.memory_set.activity_handle());
                    shared_flush_start = shared_flush_start.min(page);
                    shared_flush_end = shared_flush_end.max(page + crate::config::PAGE_SIZE);
                }
            }
            page += crate::config::PAGE_SIZE;
        }
    }
    for (tgid, vpn, frame) in changed_frames {
        sync_thread_group_vm_frame(task.getpid(), tgid, vpn, &frame);
    }
    if let Some(activity) = shared_activity.as_ref() {
        let _ = flush_remote_tlb(
            activity,
            shared_flush_start,
            shared_flush_end - shared_flush_start,
        );
    }
    true
}

pub(crate) fn copy_from_task_user(
    task: &Arc<TaskControlBlock>,
    addr: usize,
    dst: &mut [u8],
) -> bool {
    if !prepare_task_user_access(task, addr, dst.len(), false) {
        return false;
    }
    let inner = task.inner_exclusive_access();
    inner.memory_set.copy_from_user(addr, dst)
}

pub(crate) fn copy_to_task_user(task: &Arc<TaskControlBlock>, addr: usize, src: &[u8]) -> bool {
    if !prepare_task_user_access(task, addr, src.len(), true) {
        return false;
    }
    let inner = task.inner_exclusive_access();
    inner.memory_set.copy_to_user(addr, src)
}

fn user_usize(task: &Arc<TaskControlBlock>, addr: usize) -> Option<usize> {
    if addr == 0 {
        return None;
    }
    let mut bytes = [0u8; core::mem::size_of::<usize>()];
    if !copy_from_task_user(task, addr, &mut bytes) {
        return None;
    }
    Some(usize::from_ne_bytes(bytes))
}

fn user_u32(task: &Arc<TaskControlBlock>, addr: usize) -> Option<u32> {
    if addr == 0 || addr & (core::mem::align_of::<u32>() - 1) != 0 {
        return None;
    }
    let mut bytes = [0u8; core::mem::size_of::<u32>()];
    if !copy_from_task_user(task, addr, &mut bytes) {
        return None;
    }
    Some(u32::from_ne_bytes(bytes))
}

fn set_user_u32(task: &Arc<TaskControlBlock>, addr: usize, value: u32) -> Option<Vec<FutexKey>> {
    if addr == 0 || addr & (core::mem::align_of::<u32>() - 1) != 0 {
        return None;
    }
    let keys = {
        let inner = task.inner_exclusive_access();
        futex_all_keys_for_addr(&inner, addr)
    };
    if !copy_to_task_user(task, addr, &value.to_ne_bytes()) {
        return None;
    }
    Some(keys)
}

fn futex_private_key(inner: &TaskControlBlockInner, addr: usize) -> FutexKey {
    FutexKey {
        namespace: inner.memory_set.token().root().raw(),
        addr,
    }
}

fn futex_shared_key(inner: &TaskControlBlockInner, addr: usize) -> FutexKey {
    if let Some((paddr, _)) = inner.memory_set.translate(polyhal::VirtAddr::from(addr)) {
        FutexKey {
            namespace: 0,
            addr: paddr.raw(),
        }
    } else {
        FutexKey { namespace: 0, addr }
    }
}

fn futex_keys_for_addr(inner: &TaskControlBlockInner, addr: usize, shared: bool) -> Vec<FutexKey> {
    let mut keys = Vec::new();
    if shared {
        keys.push(futex_shared_key(inner, addr));
    } else {
        keys.push(futex_private_key(inner, addr));
    }
    keys
}

fn futex_all_keys_for_addr(inner: &TaskControlBlockInner, addr: usize) -> Vec<FutexKey> {
    let mut keys = Vec::new();
    let private = futex_private_key(inner, addr);
    let shared = futex_shared_key(inner, addr);
    keys.push(private);
    if private != shared {
        keys.push(shared);
    }
    keys
}

fn wake_futex_keys(keys: &[FutexKey]) {
    for &key in keys {
        let _ = futex_wake_key(key, 1);
    }
}

fn clear_stale_musl_killlock(task: &Arc<TaskControlBlock>, robust_head: usize) {
    // musl keeps killlock next to robust_list; clear its stale fast-path state before exit cleanup.
    let addr = robust_head + MUSL_KILLLOCK_OFFSET_FROM_ROBUST_HEAD;
    if user_u32(task, addr) == Some(MUSL_LOCKED_NO_WAITERS) {
        let _ = set_user_u32(task, addr, 0);
    }
}

fn process_robust_node(
    task: &Arc<TaskControlBlock>,
    node: usize,
    futex_offset: isize,
    tid: u32,
    wake_keys: &mut Vec<FutexKey>,
) {
    let futex_addr = node.wrapping_add_signed(futex_offset);
    let Some(old) = user_u32(task, futex_addr) else {
        return;
    };
    if old & FUTEX_TID_MASK != tid {
        return;
    }
    let new = (old & FUTEX_WAITERS_BIT) | FUTEX_OWNER_DIED;
    if let Some(keys) = set_user_u32(task, futex_addr, new) {
        wake_keys.extend(keys);
    }
}

fn robust_list_entry(raw: usize) -> usize {
    raw & !1
}

fn process_task_robust_list(task: &Arc<TaskControlBlock>, clear_after: bool) {
    let tid = task.getpid() as u32;
    let mut wake_keys = Vec::new();
    let head = {
        let inner = task.inner_exclusive_access();
        let head = inner.robust_list_head;
        if head == 0 || inner.robust_list_len != ROBUST_LIST_LEN {
            return;
        }
        head
    };
    let Some(offset_raw) = user_usize(&task, head + core::mem::size_of::<usize>()) else {
        if clear_after {
            let mut inner = task.inner_exclusive_access();
            inner.robust_list_head = 0;
            inner.robust_list_len = 0;
        }
        return;
    };
    let futex_offset = offset_raw as isize;
    let pending =
        robust_list_entry(user_usize(task, head + 2 * core::mem::size_of::<usize>()).unwrap_or(0));
    let mut node = robust_list_entry(user_usize(task, head).unwrap_or(0));
    let mut seen = 0usize;
    while node != 0 && node != head && seen < ROBUST_LIST_LIMIT {
        let next = robust_list_entry(user_usize(task, node).unwrap_or(0));
        process_robust_node(task, node, futex_offset, tid, &mut wake_keys);
        node = next;
        seen += 1;
    }
    if pending != 0 && pending != head {
        process_robust_node(task, pending, futex_offset, tid, &mut wake_keys);
    }
    if clear_after {
        let mut inner = task.inner_exclusive_access();
        inner.robust_list_head = 0;
        inner.robust_list_len = 0;
    }
    wake_futex_keys(&wake_keys);
}

fn process_current_robust_list(clear_after: bool) {
    let Some(task) = current_task() else {
        return;
    };
    process_task_robust_list(&task, clear_after);
}

fn clear_task_tid_ptr(task: &Arc<TaskControlBlock>) {
    let tid = {
        let inner = task.inner_exclusive_access();
        let tid = inner.tid_ptr;
        if tid == 0 || tid & (core::mem::size_of::<i32>() - 1) != 0 {
            return;
        }
        tid
    };
    let (wake_keys, _diagnostic_key) = {
        let inner = task.inner_exclusive_access();
        (
            futex_all_keys_for_addr(&inner, tid),
            futex_private_key(&inner, tid),
        )
    };
    #[cfg(feature = "smp-diagnostics")]
    record_futex_event(
        0x201,
        _diagnostic_key,
        user_u32(task, tid).unwrap_or(0) as usize,
        tid as isize,
    );
    if !copy_to_task_user(task, tid, &0u32.to_ne_bytes()) {
        return;
    }
    #[cfg(feature = "smp-diagnostics")]
    record_futex_event(0x202, _diagnostic_key, 0, tid as isize);
    {
        let mut inner = task.inner_exclusive_access();
        if inner.tid_ptr == tid {
            inner.tid_ptr = 0;
        }
    }
    wake_futex_keys(&wake_keys);
}

pub(crate) fn cleanup_task_exit_futexes(task: &Arc<TaskControlBlock>) {
    process_task_robust_list(task, false);
    clear_task_tid_ptr(task);
}

pub fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code)
}

pub fn sys_yield() -> isize {
    stdout_release_if_owner();
    suspend_current_and_run_next();
    0
}

pub fn sys_get_time(ts_ptr: usize, tz_ptr: usize) -> isize {
    let now_us = monotonic_user_time_us();
    if ts_ptr == 0 && tz_ptr == 0 {
        return (now_us / 1000) as isize;
    }

    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if ts_ptr != 0
        && !inner.memory_set.valid_user_buf(
            ts_ptr as *const u8,
            core::mem::size_of::<TimeVal>(),
            true,
        )
    {
        return -14;
    }
    if tz_ptr != 0
        && !inner.memory_set.valid_user_buf(
            tz_ptr as *const u8,
            core::mem::size_of::<TimeZone>(),
            true,
        )
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);

    if ts_ptr != 0 {
        let ts = translated_refmut(token, ts_ptr as *mut TimeVal);
        ts.sec = (now_us / 1_000_000) as isize;
        ts.usec = (now_us % 1_000_000) as isize;
    }
    if tz_ptr != 0 {
        *translated_refmut(token, tz_ptr as *mut TimeZone) = TimeZone {
            minuteswest: 0,
            dsttime: 0,
        };
    }
    0
}

pub fn sys_getpid() -> isize {
    current_task()
        .map(|t| t.inner_exclusive_access().tgid as isize)
        .unwrap_or(-1)
}

pub fn sys_getppid() -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    match &inner.parent {
        Some(parent_weak) => parent_weak
            .upgrade()
            .map(|p| p.getpid() as isize)
            .unwrap_or(1),
        None => 1,
    }
}

const CLONE_VM: usize = 0x100;
const CLONE_VFORK: usize = 0x4000;
const CLONE_FS: usize = 0x200;
const CLONE_FILES: usize = 0x400;
const CLONE_SIGHAND: usize = 0x800;
const CLONE_THREAD: usize = 0x10000;
const CLONE_SETTLS: usize = 0x80000;
const CLONE_PARENT_SETTID: usize = 0x100000;
const CLONE_CHILD_CLEARTID: usize = 0x200000;
const CLONE_CHILD_SETTID: usize = 0x01000000;

#[cfg(feature = "smp-diagnostics")]
static CLONE_DIAG_COUNT: AtomicUsize = AtomicUsize::new(0);

// The legacy clone syscall has an architecture-specific register layout.
// RISC-V passes (flags, stack, parent_tid, tls, child_tid), while LoongArch
// passes (flags, stack, parent_tid, child_tid, tls). Normalize it before the
// common Linux clone implementation consumes TLS and clear-tid addresses.
pub fn sys_clone(
    flags: usize,
    stack: usize,
    ptid: usize,
    fourth: usize,
    fifth: usize,
) -> isize {
    #[cfg(target_arch = "riscv64")]
    let (ctid, tls) = (fifth, fourth);
    #[cfg(not(target_arch = "riscv64"))]
    let (ctid, tls) = (fourth, fifth);
    #[cfg(feature = "smp-diagnostics")]
    let clone_diag = flags & CLONE_THREAD != 0
        && CLONE_DIAG_COUNT.fetch_add(1, Ordering::Relaxed) < 128;
    #[cfg(feature = "smp-diagnostics")]
    if clone_diag {
        crate::println!(
            "[clone-diag] enter tid={} flags={:#x} stack={:#x} ptid={:#x} ctid={:#x} tls={:#x}",
            current_task().map(|task| task.getpid()).unwrap_or(0),
            flags,
            stack,
            ptid,
            ctid,
            tls,
        );
    }
    let current_task = current_task().unwrap();
    if flags & CLONE_PARENT_SETTID != 0
        && ptid != 0
        && (ptid & (core::mem::align_of::<u32>() - 1) != 0
            || !prepare_task_user_access(&current_task, ptid, core::mem::size_of::<i32>(), true))
    {
        #[cfg(feature = "smp-diagnostics")]
        if clone_diag {
            crate::println!("[clone-diag] reject invalid parent_tid");
        }
        return if ptid & (core::mem::align_of::<u32>() - 1) != 0 {
            -22
        } else {
            -14
        };
    }
    if flags & (CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID) != 0
        && ctid != 0
        && (ctid & (core::mem::align_of::<u32>() - 1) != 0
            || !prepare_task_user_access(&current_task, ctid, core::mem::size_of::<i32>(), true))
    {
        #[cfg(feature = "smp-diagnostics")]
        if clone_diag {
            crate::println!("[clone-diag] reject invalid child_tid");
        }
        return if ctid & (core::mem::align_of::<u32>() - 1) != 0 {
            -22
        } else {
            -14
        };
    }
    let parent_tgid = {
        let inner = current_task.inner_exclusive_access();
        inner.tgid
    };
    let is_thread = flags & CLONE_THREAD != 0;
    let is_vfork = flags & CLONE_VFORK != 0;
    let new_task = if flags & CLONE_VM != 0 {
        let Some(new_task) = current_task.clone_shared_vm(
            flags & CLONE_SIGHAND != 0,
            flags & CLONE_FILES != 0,
            !is_vfork,
        ) else {
            #[cfg(feature = "smp-diagnostics")]
            if clone_diag {
                crate::println!("[clone-diag] reject shared-vm allocation");
            }
            return -12;
        };
        new_task
    } else {
        let Some(new_task) = current_task.fork() else {
            return -12;
        };
        new_task
    };
    let new_pid = new_task.pid.0;

    {
        let mut inner = new_task.inner_exclusive_access();
        inner.trap_cx[TrapFrameArgs::RET] = 0;
        if stack != 0 {
            inner.trap_cx[TrapFrameArgs::SP] = stack;
        }
        if is_thread {
            inner.tgid = parent_tgid;
        }
        if flags & CLONE_SETTLS != 0 && tls != 0 {
            inner.trap_cx[TrapFrameArgs::TLS] = tls;
        }
        if flags & CLONE_CHILD_CLEARTID != 0 && ctid != 0 {
            inner.tid_ptr = ctid;
        }
    }

    if flags & CLONE_CHILD_SETTID != 0 && ctid != 0 {
        // A non-CLONE_VM child owns a COW copy, so write through the child's
        // page table instead of dereferencing its virtual address under the
        // parent's active address space.
        if !copy_to_task_user(&new_task, ctid, &(new_pid as u32).to_ne_bytes()) {
            #[cfg(feature = "smp-diagnostics")]
            if clone_diag {
                crate::println!("[clone-diag] reject child_tid write");
            }
            return -14;
        }
    }
    if flags & CLONE_PARENT_SETTID != 0 && ptid != 0 {
        if !copy_to_task_user(&current_task, ptid, &(new_pid as u32).to_ne_bytes()) {
            #[cfg(feature = "smp-diagnostics")]
            if clone_diag {
                crate::println!("[clone-diag] reject parent_tid write");
            }
            return -14;
        }
    }

    if is_thread {
        current_task
            .inner_exclusive_access()
            .children
            .retain(|child| child.getpid() != new_pid);
    }
    if is_vfork {
        new_task.set_vfork_parent(current_task.getpid());
    }
    add_task(new_task.clone());
    if is_vfork {
        // CLONE_VM|CLONE_VFORK children run on the parent's address space and
        // often reuse its user stack.  Letting both execute concurrently
        // corrupts the parent's return PC (BusyBox ash reproducibly returned
        // to address zero).  Linux suspends the parent until child exec/exit.
        let _ = crate::task::block_current_and_run_next_if(BlockReason::Vfork(new_pid), || {
            new_task.vfork_pending()
        });
    }
    #[cfg(feature = "smp-diagnostics")]
    if clone_diag {
        crate::println!(
            "[clone-diag] success parent={} child={} tgid={} tls={:#x} clear_tid={:#x}",
            current_task.getpid(),
            new_pid,
            parent_tgid,
            tls,
            ctid,
        );
    }
    new_pid as isize
}

pub fn sys_fork() -> isize {
    let current_task = current_task().unwrap();
    let Some(new_task) = current_task.fork() else {
        return -12;
    };
    // initproc creates exactly one interactive shell.  QEMU's UART input is
    // serviced by the bootstrap hart, so keep that shell on CPU0 while leaving
    // normal shell-command children with the default migratable mask.
    if current_task.getpid() == 1 {
        new_task.set_allowed_cpus(1);
    }
    let new_pid = new_task.pid.0;
    {
        let mut inner = new_task.inner_exclusive_access();
        inner.trap_cx[TrapFrameArgs::RET] = 0;
    }
    add_task(new_task);
    new_pid as isize
}

fn executable_path_visible_from_root(root_path: &str, file: &dyn File, fallback: &str) -> String {
    let raw_path = file.path().unwrap_or_else(|| String::from(fallback));
    if root_path != "/" {
        if raw_path == root_path {
            return String::from("/");
        }
        if let Some(suffix) = raw_path.strip_prefix(root_path) {
            if suffix.starts_with('/') {
                return String::from(suffix);
            }
        }
    }
    if raw_path.starts_with('/') {
        raw_path
    } else {
        format!("/{}", raw_path)
    }
}

pub fn sys_exec(path: *const u8, mut args: *const usize, mut envp: *const usize) -> isize {
    const ENOENT: isize = -2;
    const ENOEXEC: isize = -8;

    let exec_task = current_task().unwrap();
    let (shared_vm, tgid) = {
        let inner = exec_task.inner_exclusive_access();
        (inner.memory_set.is_shared(), inner.tgid)
    };
    // Linux makes execve single-threaded before replacing the address space.
    // Do the same so no sibling can still execute against the old shared VM.
    if shared_vm && thread_group_has_peer(exec_task.getpid(), tgid) {
        terminate_other_threads_for_exec(&exec_task);
        while thread_group_has_peer(exec_task.getpid(), tgid) {
            suspend_current_and_run_next();
        }
    }
    let token = current_user_token();
    let path = translated_str(token, path);
    let mut args_vec: Vec<String> = Vec::new();
    if !args.is_null() {
        loop {
            let arg_str_ptr = *translated_ref(token, args);
            if arg_str_ptr == 0 {
                break;
            }
            args_vec.push(translated_str(token, arg_str_ptr as *const u8));
            unsafe {
                args = args.add(1);
            }
        }
    }
    let mut env_vec: Vec<String> = Vec::new();
    if !envp.is_null() {
        loop {
            let env_str_ptr = *translated_ref(token, envp);
            if env_str_ptr == 0 {
                break;
            }
            env_vec.push(translated_str(token, env_str_ptr as *const u8));
            unsafe {
                envp = envp.add(1);
            }
        }
    }
    let (cwd, root) = {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        (inner.cwd.clone(), inner.root.clone())
    };
    #[cfg(feature = "smp-diagnostics")]
    crate::println!(
        "[exec-diag] request tid={} path='{}' argc={} envc={}",
        exec_task.getpid(),
        path,
        args_vec.len(),
        env_vec.len(),
    );
    let open_exec = |path: &str| {
        if let Some(stripped) = path.strip_prefix('/') {
            open_file_at(Some(root.clone()), stripped, OpenFlags::RDONLY, 0)
        } else {
            open_file_at(Some(cwd.clone()), path, OpenFlags::RDONLY, 0).or_else(|| {
                let bin_path = format!("bin/{}", path);
                open_file_at(Some(root.clone()), bin_path.as_str(), OpenFlags::RDONLY, 0)
            })
        }
    };
    let app_inode = open_exec(path.as_str());
    if let Some(app_inode) = app_inode {
        let prefix = read_exec_prefix(&app_inode);
        if prefix.len() >= 2 && &prefix[..2] == b"#!" {
            let end = prefix
                .iter()
                .position(|&b| b == b'\n')
                .unwrap_or(prefix.len());
            let line = core::str::from_utf8(&prefix[2..end]).unwrap_or("");
            let parts: alloc::vec::Vec<&str> = line.split_whitespace().collect();
            if parts.is_empty() {
                return ENOEXEC;
            }
            let ipath = if parts[0].starts_with('/') {
                alloc::string::String::from(parts[0])
            } else {
                alloc::format!("/bin/{}", parts[0])
            };
            let mut new_args = alloc::vec![ipath.clone()];
            if parts.len() > 1 {
                new_args.push(alloc::string::String::from(parts[1]));
            }
            new_args.push(path);
            for a in args_vec.iter().skip(1) {
                new_args.push(a.clone());
            }
            drop(app_inode);
            let Some(i2) = open_exec(&ipath) else {
                return ENOENT;
            };
            let executable_path =
                executable_path_visible_from_root(root.path.as_str(), i2.as_ref(), &ipath);
            let interp_path = get_elf_interp_file(i2.as_ref());
            let interp_inode = if let Some(interp_path) = interp_path.as_ref() {
                open_exec(interp_path).or_else(|| {
                    let usr_interp = alloc::format!("/usr{}", interp_path);
                    open_exec(&usr_interp)
                })
            } else {
                None
            };
            if interp_path.is_some() && interp_inode.is_none() {
                return ENOENT;
            }
            let interp_file = interp_inode
                .as_ref()
                .map(|inode| inode.as_ref() as &dyn File);
            if current_task().unwrap().exec_file_with_interp(
                i2.as_ref(),
                interp_file,
                new_args,
                env_vec,
            ) {
                exec_task.set_executable_path(executable_path);
                crate::task::complete_vfork(&exec_task);
                return 0;
            }
            return ENOEXEC;
        }
        let task = current_task().unwrap();
        if prefix.len() < 4 || &prefix[..4] != b"\x7fELF" {
            if looks_like_text_script(prefix.as_slice()) {
                let mut sh_args = alloc::vec![alloc::string::String::from("/bin/sh"), path.clone()];
                for a in args_vec.iter().skip(1) {
                    sh_args.push(a.clone());
                }
                drop(app_inode);
                if let Some(sh_inode) = open_exec("/bin/sh").or_else(|| open_exec("/busybox")) {
                    let sh_prefix = read_exec_prefix(&sh_inode);
                    if sh_prefix.len() >= 4 && &sh_prefix[..4] == b"\x7fELF" {
                        let executable_path = executable_path_visible_from_root(
                            root.path.as_str(),
                            sh_inode.as_ref(),
                            "/bin/sh",
                        );
                        let interp_path = get_elf_interp_file(sh_inode.as_ref());
                        let interp_inode = if let Some(interp_path) = interp_path.as_ref() {
                            open_exec(interp_path).or_else(|| {
                                let usr_interp = alloc::format!("/usr{}", interp_path);
                                open_exec(&usr_interp)
                            })
                        } else {
                            None
                        };
                        if interp_path.is_some() && interp_inode.is_none() {
                            return ENOENT;
                        }
                        let interp_file = interp_inode
                            .as_ref()
                            .map(|inode| inode.as_ref() as &dyn File);
                        if current_task().unwrap().exec_file_with_interp(
                            sh_inode.as_ref(),
                            interp_file,
                            sh_args,
                            env_vec,
                        ) {
                            exec_task.set_executable_path(executable_path);
                            crate::task::complete_vfork(&exec_task);
                            return 0;
                        }
                    }
                }
            }
            return -8;
        }
        let executable_path =
            executable_path_visible_from_root(root.path.as_str(), app_inode.as_ref(), &path);
        // Check for dynamically linked ELF (has PT_INTERP)
        if let Some(interp_path) = get_elf_interp_file(app_inode.as_ref()) {
            let interp_inode = open_exec(&interp_path).or_else(|| {
                let usr_interp = alloc::format!("/usr{}", interp_path);
                open_exec(&usr_interp)
            });
            if let Some(interp_inode) = interp_inode {
                if task.exec_file_with_interp(
                    app_inode.as_ref(),
                    Some(interp_inode.as_ref() as &dyn File),
                    args_vec,
                    env_vec,
                ) {
                    exec_task.set_executable_path(executable_path);
                    crate::task::complete_vfork(&exec_task);
                    return 0;
                }
                return ENOEXEC;
            }
            return ENOENT;
        }
        if task.exec_file_with_interp(app_inode.as_ref(), None, args_vec, env_vec) {
            exec_task.set_executable_path(executable_path);
            crate::task::complete_vfork(&exec_task);
            0
        } else {
            ENOEXEC
        }
    } else {
        ENOENT
    }
}

fn wait_child_matches(pid: isize, child_pid: usize) -> bool {
    pid <= 0 || pid as usize == child_pid
}

fn wait_child_event_pending(task: &Arc<TaskControlBlock>, pid: isize) -> bool {
    let inner = task.inner_exclusive_access();
    let mut has_child = false;
    for child in inner.children.iter() {
        if !wait_child_matches(pid, child.getpid()) {
            continue;
        }
        has_child = true;
        if child.is_waitable_zombie() {
            return true;
        }
    }
    !has_child
}

fn wait_cancel_block(task: &Arc<TaskControlBlock>) {
    crate::task::cancel_block_before_switch(task);
    task.record_time_resume();
}

fn wait_block_current_or_retry(task: Arc<TaskControlBlock>, pid: isize) -> bool {
    stdout_release_if_owner();
    task.record_time_suspend();
    let task_cx_ptr = {
        let _irq_guard = IrqRestoreGuard::disable();
        let task_cx_ptr =
            crate::task::begin_block(&task, None, crate::task::BlockQueue::WaitPid, None);
        // A SIGALRM may arrive after sys_waitpid's first signal check but
        // before this task is registered as blocked.  Recheck it after the
        // registration so a timeout waiter cannot miss its only wakeup.
        if wait_child_event_pending(&task, pid)
            || has_interrupting_signal(&task.inner_exclusive_access())
        {
            wait_cancel_block(&task);
            return false;
        }
        #[cfg(any(feature = "smp-diagnostics", feature = "timeout-trace"))]
        if task.getpid() >= 3 {
            crate::println!(
                "[waitpid-diag] block parent={} child={}",
                task.getpid(),
                pid
            );
        }
        task_cx_ptr
    };
    schedule(task_cx_ptr);
    task.record_time_resume();
    let _ = crate::task::take_waitpid(task.getpid());
    #[cfg(any(feature = "smp-diagnostics", feature = "timeout-trace"))]
    if task.getpid() >= 3 {
        crate::println!("[waitpid-diag] resumed parent={} child={}", task.getpid(), pid);
    }
    true
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32, options: usize) -> isize {
    const WNOHANG: usize = 1;
    const WUNTRACED: usize = 2;
    const WCONTINUED: usize = 8;
    const __WNOTHREAD: usize = 0x2000_0000;
    const __WALL: usize = 0x4000_0000;
    const __WCLONE: usize = 0x8000_0000;
    const VALID_WAITPID_OPTIONS: usize =
        WNOHANG | WUNTRACED | WCONTINUED | __WNOTHREAD | __WALL | __WCLONE;
    if options & !VALID_WAITPID_OPTIONS != 0 {
        return -22;
    }
    if pid == i32::MIN as isize {
        return -3;
    }
    loop {
        let task = current_task().unwrap();
        let zombie = {
            let mut inner = task.inner_exclusive_access();
            let mut has_child = false;
            let mut zombie_idx = None;
            for (idx, child) in inner.children.iter().enumerate() {
                if !wait_child_matches(pid, child.getpid()) {
                    continue;
                }
                has_child = true;
                if child.is_waitable_zombie() {
                    zombie_idx = Some(idx);
                    break;
                }
            }
            if !has_child {
                // The in-tree Rust and C user libraries expose the historic
                // rCore waitpid ABI: no matching child is reported as -1.
                // Keep that compatibility boundary here; `waitid` retains
                // its Linux-style ECHILD result independently.
                return -1;
            }
            zombie_idx.map(|idx| inner.children.remove(idx))
        };

        if let Some(child) = zombie {
            let found_pid = child.getpid();
            let (exit_code, exit_signal, child_utime, child_cutime, child_stime, child_cstime) = {
                let child_inner = child.inner_exclusive_access();
                (
                    child_inner.exit_code,
                    child_inner.exit_signal,
                    child_inner.utime,
                    child_inner.cutime,
                    child_inner.stime,
                    child_inner.cstime,
                )
            };
            let token = {
                let mut inner = task.inner_exclusive_access();
                inner.cutime = inner
                    .cutime
                    .saturating_add(child_utime)
                    .saturating_add(child_cutime);
                inner.cstime = inner
                    .cstime
                    .saturating_add(child_stime)
                    .saturating_add(child_cstime);
                inner.memory_set.token()
            };
            crate::task::reap_exited_task(found_pid);
            #[cfg(any(feature = "smp-diagnostics", feature = "timeout-trace"))]
            if task.getpid() >= 3 {
                crate::println!(
                    "[waitpid-diag] collect parent={} child={} code={} signal={}",
                    task.getpid(),
                    found_pid,
                    exit_code,
                    exit_signal,
                );
            }
            if !exit_code_ptr.is_null() {
                *translated_refmut(token, exit_code_ptr) =
                    wait_status_from_exit(exit_code, exit_signal);
            }
            return found_pid as isize;
        }
        // No zombie: non-blocking return 0, or sleep
        if options & WNOHANG != 0 {
            return 0;
        }
        // A timed wait must return to user mode when a deliverable signal
        // arrives.  In particular, GNU timeout waits for its child here and
        // relies on SIGALRM to interrupt that wait before it can terminate the
        // child process.
        if has_interrupting_signal(&task.inner_exclusive_access()) {
            return -4;
        }
        let _ = wait_block_current_or_retry(task, pid);
        // Loop back to check again
    }
}

fn wait_status_from_exit(exit_code: i32, exit_signal: i32) -> i32 {
    if let Some(signal) = wait_signal_from_exit(exit_code, exit_signal) {
        signal
    } else {
        ((exit_code as u8) as i32) << 8
    }
}

fn wait_signal_from_exit(exit_code: i32, exit_signal: i32) -> Option<i32> {
    if exit_signal < 0 {
        Some(-exit_signal)
    } else if (-(MAX_SIG as i32)..0).contains(&exit_code) {
        Some(-exit_code)
    } else {
        None
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct WaitSigInfo {
    si_signo: i32,
    si_errno: i32,
    si_code: i32,
    _pad0: i32,
    si_pid: i32,
    si_uid: u32,
    si_status: i32,
    _pad1: i32,
    si_utime: isize,
    si_stime: isize,
    _pad: [u8; 128 - 40],
}

fn write_wait_siginfo(info_ptr: *mut u8, pid: i32, exit_code: i32, exit_signal: i32) {
    if info_ptr.is_null() {
        return;
    }
    const SIGCHLD: i32 = 17;
    const CLD_EXITED: i32 = 1;
    const CLD_KILLED: i32 = 2;
    let (code, status) = if let Some(signal) = wait_signal_from_exit(exit_code, exit_signal) {
        (CLD_KILLED, signal)
    } else {
        (CLD_EXITED, (exit_code as u8) as i32)
    };
    let token = current_user_token();
    *translated_refmut(token, info_ptr as *mut WaitSigInfo) = WaitSigInfo {
        si_signo: if pid == 0 { 0 } else { SIGCHLD },
        si_errno: 0,
        si_code: if pid == 0 { 0 } else { code },
        _pad0: 0,
        si_pid: pid,
        si_uid: 0,
        si_status: status,
        _pad1: 0,
        si_utime: 0,
        si_stime: 0,
        _pad: [0; 128 - 40],
    };
}

pub fn sys_waitid(
    which: usize,
    id: usize,
    info_ptr: *mut u8,
    options: usize,
    _rusage: *mut u8,
) -> isize {
    const P_ALL: usize = 0;
    const P_PID: usize = 1;
    const WNOHANG: usize = 1;
    const WNOWAIT: usize = 0x01000000;

    let pid = match which {
        P_ALL => -1,
        P_PID => id as isize,
        _ => return -22,
    };

    loop {
        let task = current_task().unwrap();
        let zombie = {
            let mut inner = task.inner_exclusive_access();
            let mut has_child = false;
            let mut zombie_idx = None;
            for (idx, child) in inner.children.iter().enumerate() {
                if !wait_child_matches(pid, child.getpid()) {
                    continue;
                }
                has_child = true;
                if child.is_waitable_zombie() {
                    zombie_idx = Some(idx);
                    break;
                }
            }
            if !has_child {
                return -10;
            }
            zombie_idx.map(|idx| {
                if options & WNOWAIT != 0 {
                    inner.children[idx].clone()
                } else {
                    inner.children.remove(idx)
                }
            })
        };

        if let Some(child) = zombie {
            let found_pid = child.getpid();
            let (exit_code, exit_signal, child_utime, child_cutime, child_stime, child_cstime) = {
                let child_inner = child.inner_exclusive_access();
                (
                    child_inner.exit_code,
                    child_inner.exit_signal,
                    child_inner.utime,
                    child_inner.cutime,
                    child_inner.stime,
                    child_inner.cstime,
                )
            };
            if options & WNOWAIT == 0 {
                let mut parent_inner = task.inner_exclusive_access();
                parent_inner.cutime = parent_inner
                    .cutime
                    .saturating_add(child_utime)
                    .saturating_add(child_cutime);
                parent_inner.cstime = parent_inner
                    .cstime
                    .saturating_add(child_stime)
                    .saturating_add(child_cstime);
            }
            write_wait_siginfo(info_ptr, found_pid as i32, exit_code, exit_signal);
            if options & WNOWAIT == 0 {
                crate::task::reap_exited_task(found_pid);
            }
            return 0;
        }

        if options & WNOHANG != 0 {
            write_wait_siginfo(info_ptr, 0, 0, 0);
            return 0;
        }
        let _ = wait_block_current_or_retry(task, pid);
    }
}

fn signal_task(task: &Arc<TaskControlBlock>, signum: i32, signal_info: PendingSignalInfo) {
    let target = task.getpid();
    if signum == 9 && current_task().map(|t| t.getpid() != target).unwrap_or(true) {
        force_exit_task_group(task, -9);
        force_exit_task(task, -9);
        return;
    }
    if signum < 0 || signum as usize > MAX_SIG {
        return;
    }
    let flag = SignalFlags::from_bits(1u64 << signum as usize).unwrap();
    let target_ready;
    let is_current = current_task()
        .map(|current| current.getpid() == target)
        .unwrap_or(false);
    {
        let mut task_ref = task.inner_exclusive_access();
        #[cfg(any(feature = "smp-diagnostics", feature = "timeout-trace"))]
        if signum == 14 || signum == 15 || signum == 32 || signum == 33 {
            println!(
                "[signal-diag] queue tid={} tgid={} sig={} status={:?} mask={:#x} handling={} handler={:#x}",
                target,
                task_ref.tgid,
                signum,
                task_ref.task_status,
                task_ref.signal_mask.bits(),
                task_ref.handling_sig,
                task_ref.signal_actions.lock().table[signum as usize].handler,
            );
        }
        if task_ref.signals.contains(flag) {
            return;
        }
        task_ref.pending_signal_info[signum as usize] = signal_info;
        task_ref.signals.insert(flag);
        target_ready = task_ref.task_status == TaskStatus::Ready;
        // SIGTERM is a normal catchable/ignorable signal.  GNU timeout, for
        // example, temporarily ignores it before signalling its own process
        // group.  Only SIGKILL bypasses the installed disposition here.
        if flag == SignalFlags::SIGKILL {
            task_ref.killed = true;
            task_ref.exit_signal = -(signum as i32);
        } else if task_ref.signal_actions.lock().table[signum as usize].handler == 0 {
            let fatal = flag.intersects(
                SignalFlags::SIGSEGV
                    | SignalFlags::SIGABRT
                    | SignalFlags::SIGILL
                    | SignalFlags::SIGFPE
                    | SignalFlags::SIGBUS
                    | SignalFlags::SIGSYS
                    | SignalFlags::SIGTRAP
                    | SignalFlags::SIGALRM
                    | SignalFlags::SIGVTALRM
                    | SignalFlags::SIGPROF
                    | SignalFlags::SIGTERM
                    | SignalFlags::SIGINT
                    | SignalFlags::SIGQUIT
                    | SignalFlags::SIGHUP
                    | SignalFlags::SIGPIPE
                    | SignalFlags::SIGUSR1
                    | SignalFlags::SIGUSR2
                    | SignalFlags::SIGXCPU
                    | SignalFlags::SIGXFSZ
                    | SignalFlags::SIGIO
                    | SignalFlags::SIGPWR,
            );
            if fatal {
                task_ref.killed = true;
                task_ref.exit_signal = -(signum as i32);
            }
        }
    };
    let woke_signal_waiter = wake_signal_wait_task(task, flag);
    let should_interrupt = task
        .try_inner_borrow()
        .map(|inner| has_interrupting_signal(&inner))
        .unwrap_or(false);
    if target_ready && !is_current {
        add_priority_task(task.clone());
    } else if should_interrupt && !woke_signal_waiter {
        wake_blocked_task(task);
    }
}

fn current_signal_sender(code: i32) -> PendingSignalInfo {
    current_task()
        .map(|task| {
            let inner = task.inner_exclusive_access();
            PendingSignalInfo {
                code,
                sender_pid: inner.tgid as i32,
                sender_uid: inner.euid,
            }
        })
        .unwrap_or(PendingSignalInfo {
            code,
            sender_pid: 0,
            sender_uid: 0,
        })
}

pub fn sys_kill(pid: isize, signum: i32) -> isize {
    const MAX_USER_SIG: usize = 64;
    if signum < 0 || signum as usize > MAX_USER_SIG {
        return -22; // EINVAL
    }
    let targets: Vec<Arc<TaskControlBlock>> = if pid > 0 {
        pid2task(pid as usize).into_iter().collect()
    } else {
        let pgid = if pid == 0 {
            current_task()
                .map(|task| task.inner_exclusive_access().pgid)
                .unwrap_or(0)
        } else if pid == -1 {
            0
        } else {
            (-pid) as usize
        };
        task_snapshot()
            .into_iter()
            .filter(|task| {
                if task.getpid() == 0 {
                    return false;
                }
                if pid == -1 {
                    return true;
                }
                task.try_inner_borrow()
                    .map(|inner| inner.pgid == pgid)
                    .unwrap_or(false)
            })
            .collect()
    };
    if targets.is_empty() {
        return if pid == -1 { 0 } else { -3 };
    }
    if signum == 0 {
        return 0;
    }
    if signum as usize > MAX_SIG {
        return 0;
    }
    let signal_info = current_signal_sender(0); // SI_USER
    for task in targets {
        signal_task(&task, signum, signal_info);
    }
    0
}

pub fn sys_tkill(tid: usize, signum: i32) -> isize {
    const MAX_USER_SIG: usize = 64;
    if signum < 0 || signum as usize > MAX_USER_SIG {
        return -22;
    }
    let Some(task) = pid2task(tid) else {
        return -3;
    };
    if signum == 0 || signum as usize > MAX_SIG {
        return 0;
    }
    signal_task(&task, signum, current_signal_sender(-6)); // SI_TKILL
    0
}

pub fn sys_tgkill(tgid: usize, tid: usize, signum: i32) -> isize {
    const MAX_USER_SIG: usize = 64;
    if signum < 0 || signum as usize > MAX_USER_SIG {
        return -22;
    }
    let Some(task) = pid2task(tid) else {
        return -3;
    };
    let task_tgid = task.inner_exclusive_access().tgid;
    if tgid != 0 && task_tgid != tgid {
        return -3;
    }
    if signum == 0 || signum as usize > MAX_SIG {
        return 0;
    }
    signal_task(&task, signum, current_signal_sender(-6)); // SI_TKILL
    0
}

pub fn sys_setpgid(pid: usize, pgid: usize) -> isize {
    const PID_MAX: usize = 4_194_304;
    if pgid == usize::MAX {
        return -22;
    }
    let Some(current) = current_task() else {
        return -3;
    };
    let current_pid = current.getpid();
    let target_pid = if pid == 0 { current_pid } else { pid };
    let new_pgid = if pgid == 0 { target_pid } else { pgid };
    let Some(task) = pid2task(target_pid) else {
        return -3;
    };
    if target_pid != current_pid {
        let parent_pid = task
            .inner_exclusive_access()
            .parent
            .as_ref()
            .and_then(|parent| parent.upgrade())
            .map(|parent| parent.getpid());
        if parent_pid != Some(current_pid) {
            return -3;
        }
    }
    if new_pgid >= PID_MAX {
        return -1;
    }
    let target_sid = task.inner_exclusive_access().sid;
    if new_pgid != target_pid {
        let group_exists = task_snapshot().iter().any(|candidate| {
            candidate
                .try_inner_borrow()
                .map(|inner| inner.sid == target_sid && inner.pgid == new_pgid)
                .unwrap_or(false)
        });
        if !group_exists {
            return -1;
        }
    }
    task.inner_exclusive_access().pgid = new_pgid;
    0
}

pub fn sys_getpgid(pid: usize) -> isize {
    let target_pid = if pid == 0 {
        current_task().map(|task| task.getpid()).unwrap_or(0)
    } else {
        pid
    };
    pid2task(target_pid)
        .map(|task| task.inner_exclusive_access().pgid as isize)
        .unwrap_or(-3)
}

pub fn sys_getsid(pid: usize) -> isize {
    let target_pid = if pid == 0 {
        current_task().map(|task| task.getpid()).unwrap_or(0)
    } else {
        pid
    };
    pid2task(target_pid)
        .map(|task| task.inner_exclusive_access().sid as isize)
        .unwrap_or(-3)
}

pub fn sys_setsid() -> isize {
    let Some(task) = current_task() else {
        return -3;
    };
    let pid = task.getpid();
    let pid_is_process_group = task_snapshot().iter().any(|candidate| {
        candidate
            .try_inner_borrow()
            .map(|inner| inner.pgid == pid)
            .unwrap_or(false)
    });
    if pid_is_process_group {
        return -1;
    }
    let mut inner = task.inner_exclusive_access();
    inner.sid = pid;
    inner.pgid = pid;
    pid as isize
}

pub(crate) fn user_sigset_to_flags(raw: u64) -> SignalFlags {
    let mut flags = SignalFlags::empty();
    for sig in 1..=MAX_SIG {
        if (raw & (1u64 << (sig - 1))) != 0 {
            if let Some(flag) = SignalFlags::from_bits(1u64 << sig) {
                flags.insert(flag);
            }
        }
    }
    flags
}

fn flags_to_user_sigset(flags: SignalFlags) -> u64 {
    let mut raw = 0u64;
    for sig in 1..=MAX_SIG {
        if let Some(flag) = SignalFlags::from_bits(1u64 << sig) {
            if flags.contains(flag) {
                raw |= 1u64 << (sig - 1);
            }
        }
    }
    raw
}

pub fn sys_sigprocmask(how: i32, set: *const u8, old_set: *mut u8, _sigsetsize: usize) -> isize {
    const SIG_BLOCK: i32 = 0;
    const SIG_UNBLOCK: i32 = 1;
    const SIG_SETMASK: i32 = 2;
    if let Some(task) = current_task() {
        let token = current_user_token();
        let new_mask_raw = if set.is_null() {
            None
        } else {
            Some(*translated_ref(token, set as *const u64))
        };
        let old_mask = {
            let inner = task.inner_exclusive_access();
            inner.signal_mask
        };
        if !old_set.is_null() {
            *translated_refmut(token, old_set as *mut u64) = flags_to_user_sigset(old_mask);
        }
        let Some(raw_mask) = new_mask_raw else {
            return 0;
        };
        let mut new_mask = user_sigset_to_flags(raw_mask);
        new_mask.remove(SignalFlags::SIGKILL | SignalFlags::SIGSTOP);
        let mut inner = task.inner_exclusive_access();
        inner.signal_mask = match how {
            SIG_BLOCK => old_mask | new_mask,
            SIG_UNBLOCK => old_mask & !new_mask,
            SIG_SETMASK => new_mask,
            _ => return -22,
        };
        #[cfg(feature = "smp-diagnostics")]
        if (raw_mask & ((1u64 << 31) | (1u64 << 32))) != 0
            || old_mask.contains(SignalFlags::SIGRTMIN)
            || inner.signal_mask.contains(SignalFlags::SIGRTMIN)
        {
            println!(
                "[signal-diag] mask tid={} how={} raw={:#x} old={:#x} new={:#x}",
                task.getpid(),
                how,
                raw_mask,
                old_mask.bits(),
                inner.signal_mask.bits(),
            );
        }
        0
    } else {
        -3
    }
}

pub fn sys_sigreturn() -> isize {
    crate::task::restore_current_signal_context().unwrap_or(-1)
}

fn check_sigaction_error(signal: SignalFlags, _action: usize, _old_action: usize) -> bool {
    signal == SignalFlags::SIGKILL || signal == SignalFlags::SIGSTOP
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UserSignalAction {
    handler: usize,
    flags: usize,
    restorer: usize,
    mask: u64,
}

pub fn sys_sigaction(
    signum: i32,
    action: *const u8,
    old_action: *mut u8,
    sigsetsize: usize,
) -> isize {
    if sigsetsize != 0 && sigsetsize != 8 {
        return -22;
    }
    if signum <= 0 || signum as usize > MAX_SIG {
        return -22;
    }
    let Some(flag) = SignalFlags::from_bits(1u64 << signum) else {
        return -22;
    };
    if check_sigaction_error(flag, action as usize, old_action as usize) {
        return -22;
    }

    // User pointers may demand-fault.  Snapshot them before TaskLock so the
    // synchronous fault handler never re-enters the same task lock.
    let token = current_user_token();
    let new_action = if action.is_null() {
        None
    } else {
        let raw = *translated_ref(token, action as *const UserSignalAction);
        Some(SignalAction {
            handler: raw.handler,
            flags: raw.flags,
            restorer: raw.restorer,
            mask: user_sigset_to_flags(raw.mask),
        })
    };
    let task = current_task().unwrap();
    let actions = {
        let inner = task.inner_exclusive_access();
        inner.signal_actions.clone()
    };
    let previous = {
        let mut table = actions.lock();
        let previous = table.table[signum as usize];
        if let Some(new_action) = new_action {
            table.table[signum as usize] = new_action;
        }
        previous
    };
    #[cfg(feature = "smp-diagnostics")]
    if signum == 14 || signum == 32 || signum == 33 {
        println!(
            "[signal-diag] action tid={} sig={} handler={:#x} flags={:#x} mask={:#x} shared={}",
            task.getpid(),
            signum,
            new_action
                .map(|value| value.handler)
                .unwrap_or(previous.handler),
            new_action
                .map(|value| value.flags)
                .unwrap_or(previous.flags),
            new_action
                .map(|value| value.mask.bits())
                .unwrap_or(previous.mask.bits()),
            Arc::strong_count(&actions),
        );
    }
    if !old_action.is_null() {
        *translated_refmut(token, old_action as *mut UserSignalAction) = UserSignalAction {
            handler: previous.handler,
            flags: previous.flags,
            restorer: previous.restorer,
            mask: flags_to_user_sigset(previous.mask),
        };
    }
    0
}

pub fn sys_brk(brk_addr: usize) -> isize {
    let task = current_task().unwrap();
    let pid = task.getpid();
    let mut inner = task.inner_exclusive_access();

    if brk_addr == 0 {
        return inner.memory_set.program_brk() as isize;
    }

    let heap_bottom = inner.heap_bottom;
    if brk_addr < heap_bottom {
        return inner.memory_set.program_brk() as isize;
    }

    let old_brk = inner.memory_set.program_brk();
    let mut retired_pages = None;

    let old_brk_page = (old_brk + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE;
    let new_brk_page = (brk_addr + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE;
    if new_brk_page > old_brk_page {
        let start_va = polyhal::VirtAddr::new(old_brk_page * crate::config::PAGE_SIZE);
        let end_va = polyhal::VirtAddr::new(new_brk_page * crate::config::PAGE_SIZE);
        if !inner.memory_set.insert_lazy_area(
            start_va,
            end_va,
            crate::mm::MapPermission::R | crate::mm::MapPermission::W | crate::mm::MapPermission::U,
        ) {
            return old_brk as isize;
        }
    } else if new_brk_page < old_brk_page {
        let shrink_start = polyhal::VirtAddr::new(new_brk_page * crate::config::PAGE_SIZE);
        let shrink_end = polyhal::VirtAddr::new(old_brk_page * crate::config::PAGE_SIZE);
        let unmap = inner.memory_set.munmap(shrink_start, shrink_end);
        if unmap.unmapped() {
            retired_pages = Some((
                inner.memory_set.activity_handle(),
                shrink_start.raw(),
                shrink_end.raw() - shrink_start.raw(),
                unmap,
            ));
        } else {
            // No PTE was removed, so there is no remote TLB work to defer.
            unmap.release();
        }
    }
    inner.program_brk = brk_addr;
    inner.memory_set.set_program_brk(brk_addr);
    let tgid = inner.tgid;
    let shared_address_space = if inner.memory_set.is_shared() {
        Some(inner.memory_set.address_space_id())
    } else {
        None
    };
    let may_sync = inner.memory_set.is_shared();
    let snapshot = if may_sync {
        Some(inner.memory_set.shared_metadata_snapshot())
    } else {
        None
    };
    drop(inner);
    if let Some(address_space_id) = shared_address_space {
        sync_shared_address_space_brk(pid, address_space_id, brk_addr);
    } else if let Some(snapshot) = snapshot {
        if thread_group_has_peer(pid, tgid) {
            sync_thread_group_vm_metadata(pid, tgid, brk_addr, &snapshot);
        }
    }
    if let Some((activity, start, len, retired)) = retired_pages {
        let _ = flush_remote_tlb(&activity, start, len);
        retired.release();
    }
    brk_addr as isize
}

pub fn sys_sleep(duration: u64) -> isize {
    let wakeup_time = sys_get_time(0, 0) + duration as isize;
    loop {
        if current_task()
            .map(|task| task.inner_exclusive_access().killed)
            .unwrap_or(false)
        {
            return -4;
        }
        let now = sys_get_time(0, 0);
        if now >= wakeup_time {
            break;
        }
        sys_yield();
    }
    0
}

pub fn sys_shutdown() -> isize {
    crate::println!(
        "[kernel] shutdown requested by tid={}",
        current_task().map(|task| task.getpid()).unwrap_or(0)
    );
    crate::fs::vfs::shutdown_filesystems();
    polyhal::instruction::shutdown();
    0
}

fn get_map_permission(prot: usize) -> crate::mm::MapPermission {
    let mut perm = crate::mm::MapPermission::U;
    if prot & PROT_READ != 0 {
        perm |= crate::mm::MapPermission::R;
    }
    if prot & PROT_WRITE != 0 {
        perm |= crate::mm::MapPermission::R | crate::mm::MapPermission::W;
    }
    if prot & PROT_EXEC != 0 {
        perm |= crate::mm::MapPermission::R | crate::mm::MapPermission::X;
    }
    perm
}

pub fn sys_mmap(
    start: usize,
    len: usize,
    prot: usize,
    flags: usize,
    fd: i32,
    offset: usize,
) -> isize {
    if start % crate::config::PAGE_SIZE != 0 || len == 0 {
        return -22;
    }
    if prot & !VALID_MMAP_PROT != 0 {
        return -22;
    }
    let map_type = flags & MAP_TYPE_MASK;
    let map_shared = map_type == MAP_SHARED || map_type == MAP_SHARED_VALIDATE;
    let map_private = map_type == MAP_PRIVATE;
    if !map_shared && !map_private {
        return -22;
    }
    let map_fixed = (flags & MAP_FIXED) != 0;
    let map_fixed_noreplace = (flags & MAP_FIXED_NOREPLACE) != 0;
    if (map_fixed || map_fixed_noreplace) && start == 0 {
        return -22;
    }

    let task = crate::task::current_task().unwrap();
    let pid = task.getpid();
    let mut inner = task.inner_exclusive_access();
    // MAP_FIXED removes an existing mapping before installing its replacement.
    // Keep its frames alive until this syscall has released TaskLock and all
    // harts using the address space have completed the required shootdown.
    let mut fixed_unmap: Option<(
        crate::mm::AddressSpaceActivityHandle,
        usize,
        usize,
        crate::mm::UnmapResult,
    )> = None;
    macro_rules! finish_fixed_unmap {
        () => {
            if let Some((activity, start, len, retired)) = fixed_unmap.take() {
                let _ = flush_remote_tlb(&activity, start, len);
                retired.release();
            }
        };
    }
    macro_rules! fail_after_fixed_unmap {
        ($value:expr) => {{
            let ret = $value;
            drop(inner);
            finish_fixed_unmap!();
            return ret;
        }};
    }
    macro_rules! sync_and_return {
        ($value:expr, $frame_start:expr, $frame_end:expr) => {{
            let ret = $value;
            let tgid = inner.tgid;
            let may_sync = ret >= 0 && inner.memory_set.is_shared();
            let areas = if may_sync {
                inner
                    .memory_set
                    .area_metadata_range($frame_start, $frame_end)
            } else {
                Vec::new()
            };
            drop(inner);
            if may_sync && thread_group_has_peer(pid, tgid) {
                sync_thread_group_vm_area_metadata(pid, tgid, &areas);
            }
            finish_fixed_unmap!();
            return ret;
        }};
    }

    let permission = get_map_permission(prot);

    let real_start = if start == 0 {
        let max_end: usize = inner.memory_set.max_end_vaddr().into();
        (max_end + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE
            * crate::config::PAGE_SIZE
    } else {
        start
    };
    let Some(real_end) = real_start.checked_add(len) else {
        return -22;
    };

    if (flags & MAP_ANONYMOUS) != 0 {
        // For a non-fixed mmap(NULL, ...), address selection and VMA
        // publication must be one transaction.  CLONE_VM siblings can enter
        // this syscall under different TaskLocks.
        if !map_shared && start == 0 {
            let Some(aligned_len) = page_align_up(len) else {
                fail_after_fixed_unmap!(-12);
            };
            let Some(mapped_start) = inner.memory_set.insert_lazy_area_auto(len, permission)
            else {
                fail_after_fixed_unmap!(-12);
            };
            let Some(mapped_end) = mapped_start
                .raw()
                .checked_add(aligned_len)
                .map(polyhal::VirtAddr::from)
            else {
                fail_after_fixed_unmap!(-12);
            };
            #[cfg(feature = "smp-diagnostics")]
            if len >= 0x10_0000 {
                crate::println!(
                    "[mmap-diag] anon tid={} req={:#x} real={:#x} len={:#x} prot={:#x} flags={:#x} fixed={}",
                    pid,
                    start,
                    mapped_start.raw(),
                    len,
                    prot,
                    flags,
                    map_fixed,
                );
            }
            sync_and_return!(mapped_start.raw() as isize, mapped_start, mapped_end);
        }
        #[cfg(feature = "smp-diagnostics")]
        if len >= 0x10_0000 {
            crate::println!(
                "[mmap-diag] anon tid={} req={:#x} real={:#x} len={:#x} prot={:#x} flags={:#x} fixed={}",
                pid,
                start,
                real_start,
                len,
                prot,
                flags,
                map_fixed,
            );
        }
        let start_addr = polyhal::VirtAddr::from(real_start);
        let end_addr = polyhal::VirtAddr::from(real_end);
        if map_fixed_noreplace && inner.memory_set.is_overlap(start_addr, end_addr) {
            return -17;
        }
        if map_fixed && inner.memory_set.is_overlap(start_addr, end_addr) {
            let retired = inner.memory_set.munmap(start_addr, end_addr);
            if retired.unmapped() {
                fixed_unmap = Some((
                    inner.memory_set.activity_handle(),
                    start_addr.raw(),
                    end_addr.raw() - start_addr.raw(),
                    retired,
                ));
            } else {
                retired.release();
            }
        }
        if map_shared {
            let aligned_len =
                (len + crate::config::PAGE_SIZE - 1) & !(crate::config::PAGE_SIZE - 1);
            let pages = aligned_len / crate::config::PAGE_SIZE;
            let Some(frames) = frames_alloc(pages) else {
                fail_after_fixed_unmap!(-12);
            };
            sync_and_return!(
                match inner.memory_set.insert_shared_frames(
                    start_addr,
                    aligned_len,
                    &frames,
                    permission,
                ) {
                    Some(addr) => addr.raw() as isize,
                    None => -12,
                },
                start_addr,
                polyhal::VirtAddr::from(start_addr.raw() + aligned_len)
            );
        }
        if !inner
            .memory_set
            .insert_lazy_area(start_addr, end_addr, permission)
        {
            fail_after_fixed_unmap!(-12);
        }
        sync_and_return!(real_start as isize, start_addr, end_addr);
    } else {
        let start_addr = polyhal::VirtAddr::from(real_start);
        let end_addr = polyhal::VirtAddr::from(real_end);
        if map_fixed_noreplace && inner.memory_set.is_overlap(start_addr, end_addr) {
            return -17;
        }
        if map_fixed && inner.memory_set.is_overlap(start_addr, end_addr) {
            let retired = inner.memory_set.munmap(start_addr, end_addr);
            if retired.unmapped() {
                fixed_unmap = Some((
                    inner.memory_set.activity_handle(),
                    start_addr.raw(),
                    end_addr.raw() - start_addr.raw(),
                    retired,
                ));
            } else {
                retired.release();
            }
        }

        if fd < 0 || fd as usize >= inner.fd_table.len() {
            fail_after_fixed_unmap!(-9);
        }
        if offset % crate::config::PAGE_SIZE != 0 {
            fail_after_fixed_unmap!(-22);
        }
        let Some(aligned_len) = page_align_up(len) else {
            fail_after_fixed_unmap!(-22);
        };
        if offset.checked_add(aligned_len).is_none() {
            fail_after_fixed_unmap!(-22);
        }

        if let Some(file) = inner.fd_table[fd as usize].clone() {
            if !file.readable() {
                fail_after_fixed_unmap!(-13);
            }
            if map_shared {
                if permission.contains(MapPermission::W) && !file.writable() {
                    fail_after_fixed_unmap!(-13);
                }
                if file.as_any().is::<crate::fs::DevZero>() {
                    let pages = aligned_len / crate::config::PAGE_SIZE;
                    let Some(frames) = frames_alloc(pages) else {
                        fail_after_fixed_unmap!(-12);
                    };
                    sync_and_return!(
                        match inner.memory_set.insert_shared_file_frames(
                            start_addr,
                            aligned_len,
                            &frames,
                            permission,
                            file,
                            offset,
                        ) {
                            Some(addr) => addr.raw() as isize,
                            None => -12,
                        },
                        start_addr,
                        polyhal::VirtAddr::from(start_addr.raw() + aligned_len)
                    );
                }
                if permission.contains(MapPermission::W) {
                    let pages = aligned_len / crate::config::PAGE_SIZE;
                    let Some(frames) =
                        load_shared_file_frames_uncached(file.as_ref(), offset, pages)
                    else {
                        fail_after_fixed_unmap!(-12);
                    };
                    sync_and_return!(
                        match inner.memory_set.insert_shared_file_frames(
                            start_addr,
                            aligned_len,
                            &frames,
                            permission,
                            file,
                            offset,
                        ) {
                            Some(addr) => addr.raw() as isize,
                            None => -12,
                        },
                        start_addr,
                        polyhal::VirtAddr::from(start_addr.raw() + aligned_len)
                    );
                }
                let Some(frames) = shared_file_mapping_frames(file.clone(), offset, aligned_len)
                else {
                    fail_after_fixed_unmap!(-12);
                };
                sync_and_return!(
                    match inner.memory_set.insert_shared_file_frames(
                        start_addr,
                        aligned_len,
                        &frames,
                        permission,
                        file,
                        offset,
                    ) {
                        Some(addr) => addr.raw() as isize,
                        None => -12,
                    },
                    start_addr,
                    polyhal::VirtAddr::from(start_addr.raw() + aligned_len)
                );
            }
            if start == 0 {
                let Some(mapped_start) = inner.memory_set.insert_file_area_auto(
                    aligned_len,
                    permission,
                    file.clone(),
                    offset,
                ) else {
                    fail_after_fixed_unmap!(-12);
                };
                let mapped_end = polyhal::VirtAddr::from(mapped_start.raw() + aligned_len);
                #[cfg(feature = "smp-diagnostics")]
                if permission.contains(MapPermission::X)
                    || file
                        .path()
                        .as_deref()
                        .is_some_and(|path| path.contains("librustc_driver"))
                {
                    crate::println!(
                        "[mmap-diag] file-map tid={} req={:#x} start={:#x} len={:#x} off={:#x} perm={:#x} flags={:#x} fixed={} shared={} path={:?}",
                        pid,
                        start,
                        mapped_start.raw(),
                        aligned_len,
                        offset,
                        permission.bits(),
                        flags,
                        map_fixed,
                        map_shared,
                        file.path(),
                    );
                }
                sync_and_return!(mapped_start.raw() as isize, mapped_start, mapped_end);
            }
            #[cfg(feature = "smp-diagnostics")]
            if permission.contains(MapPermission::X)
                || file
                    .path()
                    .as_deref()
                    .is_some_and(|path| path.contains("librustc_driver"))
            {
                crate::println!(
                    "[mmap-diag] file-map tid={} req={:#x} start={:#x} len={:#x} off={:#x} perm={:#x} flags={:#x} fixed={} shared={} path={:?}",
                    pid,
                    start,
                    start_addr.raw(),
                    aligned_len,
                    offset,
                    permission.bits(),
                    flags,
                    map_fixed,
                    map_shared,
                    file.path(),
                );
            }
            let ok = inner
                .memory_set
                .insert_file_area(start_addr, end_addr, permission, file, offset);
            if ok {
                sync_and_return!(real_start as isize, start_addr, end_addr);
            } else {
                fail_after_fixed_unmap!(-1);
            }
        } else {
            fail_after_fixed_unmap!(-1);
        }
    }
}

pub fn sys_munmap(start: usize, len: usize) -> isize {
    if start % crate::config::PAGE_SIZE != 0 || len == 0 {
        return -1;
    }

    let task = crate::task::current_task().unwrap();
    let pid = task.getpid();
    #[cfg(feature = "smp-diagnostics")]
    if len >= 0x10_0000 {
        crate::println!(
            "[munmap-diag] tid={} start={:#x} len={:#x}",
            pid,
            start,
            len,
        );
    }
    let mut inner = task.inner_exclusive_access();

    let start_addr = polyhal::VirtAddr::from(start);
    let end_addr = polyhal::VirtAddr::from(start + len);

    let unmap = inner.memory_set.munmap(start_addr, end_addr);
    if unmap.unmapped() {
        let tgid = inner.tgid;
        let may_sync = inner.memory_set.is_shared();
        let activity = inner.memory_set.activity_handle();
        drop(inner);
        if may_sync && thread_group_has_peer(pid, tgid) {
            sync_thread_group_vm_unmap_metadata(pid, tgid, start_addr, end_addr);
        }
        let _ = flush_remote_tlb(&activity, start, len);
        unmap.release();
        0
    } else {
        unmap.release();
        -1
    }
}

pub fn sys_msync(start: usize, len: usize, flags: usize) -> isize {
    if (len > 0 && start as u64 + len as u64 > usize::MAX as u64)
        || start % crate::config::PAGE_SIZE != 0
    {
        return -22;
    }
    let task = crate::task::current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let start_addr = polyhal::VirtAddr::from(start);
    inner.memory_set.msync(start_addr, len, flags)
}

pub fn sys_shmget(key: usize, size: usize, flags: usize) -> isize {
    const IPC_PRIVATE: usize = 0;
    const IPC_CREAT: usize = 0o1000;
    const IPC_EXCL: usize = 0o2000;
    if size == 0 {
        return -22;
    }
    if key != IPC_PRIVATE {
        let existing = {
            let segments = SHM_SEGMENTS.exclusive_access();
            segments
                .iter()
                .enumerate()
                .find(|(_, seg)| seg.as_ref().is_some_and(|seg| seg.key == key))
                .map(|(idx, _)| idx)
        };
        if let Some(idx) = existing {
            if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                return -17;
            }
            return (idx + 1) as isize;
        }
        if flags & IPC_CREAT == 0 {
            return -2;
        }
    }

    let aligned_size = (size + crate::config::PAGE_SIZE - 1) & !(crate::config::PAGE_SIZE - 1);
    let pages = aligned_size / crate::config::PAGE_SIZE;
    // Physical allocation may take allocator locks and must not happen while
    // the shared-segment registry is held.
    let Some(frames) = frames_alloc(pages) else {
        return -12;
    };

    let mut segments = SHM_SEGMENTS.exclusive_access();
    // A concurrent hart may have created the keyed segment while frames were
    // allocated.  Recheck before publishing and release the new frames only
    // after the VM lock has been dropped.
    if key != IPC_PRIVATE {
        if let Some((idx, _)) = segments
            .iter()
            .enumerate()
            .find(|(_, seg)| seg.as_ref().is_some_and(|seg| seg.key == key))
        {
            let result = if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                -17
            } else {
                (idx + 1) as isize
            };
            drop(segments);
            drop(frames);
            return result;
        }
    }

    let segment = ShmSegment {
        key,
        size: aligned_size,
        frames,
    };
    if let Some(idx) = segments.iter().position(|seg| seg.is_none()) {
        segments[idx] = Some(segment);
        (idx + 1) as isize
    } else {
        segments.push(Some(segment));
        segments.len() as isize
    }
}

pub fn sys_shmat(shmid: usize, shmaddr: usize, shmflg: usize) -> isize {
    const SHM_RDONLY: usize = 0o10000;
    if shmid == 0 {
        return -22;
    }
    let frame_count = {
        let segments = SHM_SEGMENTS.exclusive_access();
        let Some(Some(segment)) = segments.get(shmid - 1) else {
            return -22;
        };
        segment.frames.len()
    };
    // Reserve before acquiring VmLock.  Cloning a segment only increments
    // frame refcounts; mapping happens after the registry lock is released.
    let mut frames = Vec::with_capacity(frame_count);
    let size = {
        let segments = SHM_SEGMENTS.exclusive_access();
        let Some(Some(segment)) = segments.get(shmid - 1) else {
            return -22;
        };
        for frame in &segment.frames {
            frames.push(frame.shared_clone());
        }
        segment.size
    };
    if frames.len() != frame_count {
        return -22;
    }
    let mut permission = MapPermission::R | MapPermission::U;
    if shmflg & SHM_RDONLY == 0 {
        permission |= MapPermission::W;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let addr = inner.memory_set.insert_shared_frames(
        polyhal::VirtAddr::from(shmaddr),
        size,
        &frames,
        permission,
    );
    match addr {
        Some(addr) => addr.raw() as isize,
        None => -12,
    }
}

pub fn sys_shmdt(_shmaddr: usize) -> isize {
    0
}

pub fn sys_shmctl(shmid: usize, _cmd: usize, _buf: *mut u8) -> isize {
    if shmid == 0 {
        return -22;
    }
    let segments = SHM_SEGMENTS.exclusive_access();
    if shmid - 1 >= segments.len() || segments[shmid - 1].is_none() {
        return -22;
    }
    0
}

pub fn sys_times(tms_ptr: usize) -> isize {
    let task = crate::task::current_task().unwrap();

    task.record_time_suspend();
    task.record_time_resume();

    if tms_ptr != 0 {
        let inner = task.inner_exclusive_access();
        if !inner
            .memory_set
            .valid_user_buf(tms_ptr as *const u8, core::mem::size_of::<Tms>(), true)
        {
            return -14;
        }
        let tms = Tms {
            tms_utime: nsec_to_clock_ticks(inner.utime),
            tms_stime: nsec_to_clock_ticks(inner.stime),
            tms_cutime: nsec_to_clock_ticks(inner.cutime),
            tms_cstime: nsec_to_clock_ticks(inner.cstime),
        };
        let token = inner.get_user_token();
        drop(inner);
        *crate::mm::translated_refmut(token, tms_ptr as *mut Tms) = tms;
    }

    monotonic_user_times_tick() as isize
}

pub fn sys_uname(uts_ptr: usize) -> isize {
    if uts_ptr == 0 {
        return -14;
    }

    let task = crate::task::current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(uts_ptr as *const u8, core::mem::size_of::<UtsName>(), true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let uts = crate::mm::translated_refmut(token, uts_ptr as *mut UtsName);

    *uts = UtsName {
        sysname: [0; UTSNAME_LEN],
        nodename: [0; UTSNAME_LEN],
        release: [0; UTSNAME_LEN],
        version: [0; UTSNAME_LEN],
        machine: [0; UTSNAME_LEN],
        domainname: [0; UTSNAME_LEN],
    };

    UtsName::set_str(&mut uts.sysname, "Linux");
    UtsName::set_str(&mut uts.nodename, "rCore-Tutorial");
    UtsName::set_str(&mut uts.release, "5.10.0");
    UtsName::set_str(&mut uts.version, "v0.1.0");

    #[cfg(target_arch = "riscv64")]
    UtsName::set_str(&mut uts.machine, "riscv64");
    #[cfg(target_arch = "x86_64")]
    UtsName::set_str(&mut uts.machine, "x86_64");
    #[cfg(target_arch = "loongarch64")]
    UtsName::set_str(&mut uts.machine, "loongarch64");

    UtsName::set_str(&mut uts.domainname, "localdomain");

    0
}

pub fn sys_getcpu(cpu_ptr: *mut u32, node_ptr: *mut u32, _cache: usize) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !cpu_ptr.is_null()
        && !inner
            .memory_set
            .valid_user_buf(cpu_ptr as *const u8, core::mem::size_of::<u32>(), true)
    {
        return -14;
    }
    if !node_ptr.is_null()
        && !inner.memory_set.valid_user_buf(
            node_ptr as *const u8,
            core::mem::size_of::<u32>(),
            true,
        )
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    if !cpu_ptr.is_null() {
        *translated_refmut(token, cpu_ptr) = crate::task::current_hart_id() as u32;
    }
    if !node_ptr.is_null() {
        *translated_refmut(token, node_ptr) = 0;
    }
    0
}

pub fn sys_syslog(action: usize, buf: *mut u8, len: usize) -> isize {
    match action {
        0 | 1 => 0, // open/close log
        5 => {
            // clear
            crate::logging::klog_clear();
            0
        }
        2 | 3 | 4 => {
            // read / read all / read+clear
            if buf.is_null() || len == 0 {
                return 0;
            }
            let (part1, part2) = crate::logging::klog_bytes();
            let total = part1.len() + part2.len();
            if total == 0 {
                return 0;
            }
            let copy_len = total.min(len);
            let slice = unsafe { core::slice::from_raw_parts_mut(buf, copy_len) };
            let from_p1 = part1.len().min(copy_len);
            slice[..from_p1].copy_from_slice(&part1[..from_p1]);
            if copy_len > from_p1 {
                let from_p2 = copy_len - from_p1;
                slice[from_p1..].copy_from_slice(&part2[..from_p2]);
            }
            if action == 4 {
                crate::logging::klog_clear();
            }
            copy_len as isize
        }
        9 => {
            // number of unread chars
            let (part1, part2) = crate::logging::klog_bytes();
            (part1.len() + part2.len()) as isize
        }
        _ => -22, // EINVAL
    }
}

pub fn sys_nanosleep(req_ptr: usize, rem_ptr: usize) -> isize {
    let task = crate::task::current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let ts_size = core::mem::size_of::<crate::task::TimeSpec>();
    if !inner
        .memory_set
        .valid_user_buf(req_ptr as *const u8, ts_size, false)
    {
        return -14;
    }
    if rem_ptr != 0
        && !inner
            .memory_set
            .valid_user_buf(rem_ptr as *const u8, ts_size, true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let req = *crate::mm::translated_refmut(token, req_ptr as *mut crate::task::TimeSpec);
    if req.tv_nsec >= 1_000_000_000 {
        return -22;
    }
    let Some(duration_ns) = req
        .tv_sec
        .checked_mul(1_000_000_000)
        .and_then(|sec| sec.checked_add(req.tv_nsec as usize))
    else {
        return -22;
    };
    let wakeup_time = polyhal::time::Time::now().to_nsec() as u64 + duration_ns as u64;
    loop {
        let now = polyhal::time::Time::now().to_nsec() as u64;
        if task.inner_exclusive_access().killed {
            return -4;
        }
        {
            let inner = task.inner_exclusive_access();
            if has_interrupting_signal(&inner) {
                if rem_ptr != 0 {
                    let rem_ns = wakeup_time.saturating_sub(now) as usize;
                    let rem =
                        crate::mm::translated_refmut(token, rem_ptr as *mut crate::task::TimeSpec);
                    rem.tv_sec = rem_ns / 1_000_000_000;
                    rem.tv_nsec = rem_ns % 1_000_000_000;
                }
                return -4;
            }
        }
        if now >= wakeup_time {
            break;
        }
        stdout_release_if_owner();
        crate::task::suspend_current_and_run_next();
    }
    if rem_ptr != 0 {
        let rem = crate::mm::translated_refmut(token, rem_ptr as *mut crate::task::TimeSpec);
        rem.tv_sec = 0;
        rem.tv_nsec = 0;
    }
    0
}

pub fn sys_set_tid_address(tidptr: *mut i32) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    inner.tid_ptr = tidptr as usize;
    task.getpid() as isize
}

pub fn sys_exit_group(status: i32) -> ! {
    let task = current_task().unwrap();
    #[cfg(feature = "smp-diagnostics")]
    if task.getpid() >= 3 {
        crate::println!("[exit-diag] tid={} group-enter", task.getpid());
    }
    force_exit_task_group(&task, status);
    #[cfg(feature = "smp-diagnostics")]
    if task.getpid() >= 3 {
        crate::println!("[exit-diag] tid={} group-complete", task.getpid());
    }

    exit_current_and_run_next(status)
}

pub fn sys_getuid() -> isize {
    current_task()
        .map(|t| t.inner_exclusive_access().uid as isize)
        .unwrap_or(-1)
}

pub fn sys_geteuid() -> isize {
    current_task()
        .map(|t| t.inner_exclusive_access().euid as isize)
        .unwrap_or(-1)
}

pub fn sys_getgid() -> isize {
    current_task()
        .map(|t| t.inner_exclusive_access().gid as isize)
        .unwrap_or(-1)
}

pub fn sys_getegid() -> isize {
    current_task()
        .map(|t| t.inner_exclusive_access().egid as isize)
        .unwrap_or(-1)
}

pub fn sys_gettid() -> isize {
    current_task().map(|t| t.getpid() as isize).unwrap_or(-1)
}

fn is_no_id_change(id: usize) -> bool {
    id == usize::MAX || id == u32::MAX as usize
}

fn parse_optional_id(id: usize) -> Result<Option<u32>, isize> {
    if is_no_id_change(id) {
        Ok(None)
    } else if id > u32::MAX as usize {
        Err(-22)
    } else {
        Ok(Some(id as u32))
    }
}

pub fn sys_setuid(uid: u32) -> isize {
    if let Some(task) = current_task() {
        let mut inner = task.inner_exclusive_access();
        if uid == u32::MAX {
            return -22;
        }
        if inner.euid == 0 {
            inner.uid = uid;
            inner.euid = uid;
            inner.suid = uid;
            return 0;
        }
        if uid == inner.uid || uid == inner.euid || uid == inner.suid {
            inner.euid = uid;
            return 0;
        }
        -1
    } else {
        -1
    }
}

pub fn sys_setgid(gid: u32) -> isize {
    if let Some(task) = current_task() {
        let mut inner = task.inner_exclusive_access();
        if gid == u32::MAX {
            return -22;
        }
        if inner.euid == 0 {
            inner.gid = gid;
            inner.egid = gid;
            inner.sgid = gid;
            return 0;
        }
        if gid == inner.gid || gid == inner.egid || gid == inner.sgid {
            inner.egid = gid;
            return 0;
        }
        -1
    } else {
        -1
    }
}

pub fn sys_umask(mask: u32) -> isize {
    current_task()
        .map(|t| {
            let mut inner = t.inner_exclusive_access();
            let old = inner.umask;
            inner.umask = mask & 0o777;
            old as isize
        })
        .unwrap_or(-1)
}

pub fn sys_getrandom(buf: *mut u8, len: usize, flags: usize) -> isize {
    const GRND_NONBLOCK: usize = 0x0001;
    const GRND_RANDOM: usize = 0x0002;
    const GRND_INSECURE: usize = 0x0004;

    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0 {
        return -22;
    }
    if len == 0 {
        return 0;
    }
    if buf.is_null() {
        return -14;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner.memory_set.valid_user_buf(buf, len, true) {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let user_buf = crate::mm::translated_byte_buffer(token, buf, len);
    let seed = polyhal::time::Time::now().to_nsec() as u64 ^ (task.getpid() as u64);
    for (i, byte) in user_buf.iter_mut().enumerate() {
        let x = seed
            .wrapping_add((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .rotate_left((i & 31) as u32);
        *byte = (x ^ (x >> 32)) as u8;
    }
    len as isize
}

// ---- poll / ppoll ----

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

const POLLIN: i16 = 0x001;
const POLLPRI: i16 = 0x002;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;
const POLLNVAL: i16 = 0x020;

pub fn sys_ppoll(
    fds: *mut u8,
    nfds: usize,
    tmo_p: *const u8,
    _sigmask: *const u8,
    _sigsetsize: usize,
) -> isize {
    let token = current_user_token();
    let buf = translated_byte_buffer(token, fds, nfds * core::mem::size_of::<PollFd>());
    let pollfds: &mut [PollFd] =
        unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut PollFd, nfds) };

    let start_ms = polyhal::time::Time::now().to_msec() as usize;

    loop {
        crate::task::check_itimers();
        crate::net::net_poll();
        let mut ready = 0i32;
        let task = current_task().unwrap();
        let mut inner = task.inner_exclusive_access();
        if has_interrupting_signal(&inner) {
            return -4;
        }

        for pfd in pollfds.iter_mut() {
            pfd.revents = 0;
            if pfd.fd < 0 {
                continue;
            }
            let fd = pfd.fd as usize;

            if fd < inner.fd_table.len() {
                if let Some(file) = &inner.fd_table[fd] {
                    let file = file.clone();
                    drop(inner);
                    let (readable, writable, error) = file.poll();
                    if (pfd.events & POLLIN) != 0 && readable {
                        pfd.revents |= POLLIN;
                    }
                    if (pfd.events & POLLOUT) != 0 && writable {
                        pfd.revents |= POLLOUT;
                    }
                    if error {
                        pfd.revents |= POLLHUP;
                    }
                    if pfd.revents != 0 {
                        ready += 1;
                    }
                    inner = task.inner_exclusive_access();
                    continue;
                }
            }

            pfd.revents |= POLLERR;
            ready += 1;
        }
        drop(inner);
        drop(task);

        if ready > 0 {
            stdout_release_if_owner();
            suspend_current_and_run_next();
            return ready as isize;
        }

        if !tmo_p.is_null() {
            let tmo: &crate::task::TimeSpec =
                translated_ref(token, tmo_p as *const crate::task::TimeSpec);
            let timeout_ms = tmo.tv_sec * 1000 + tmo.tv_nsec / 1_000_000;
            if timeout_ms == 0 {
                return 0;
            }
            let now_ms = polyhal::time::Time::now().to_msec() as usize;
            if now_ms - start_ms >= timeout_ms {
                return 0;
            }
        }

        stdout_release_if_owner();
        suspend_current_and_run_next();
    }
}

pub fn sys_mprotect(start: usize, len: usize, prot: usize) -> isize {
    if start % crate::config::PAGE_SIZE != 0 {
        return -22;
    }
    if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return -22;
    }
    let task = current_task().unwrap();
    let pid = task.getpid();
    let mut inner = task.inner_exclusive_access();
    let ret = inner.memory_set.mprotect(
        polyhal::VirtAddr::from(start),
        len,
        get_map_permission(prot),
    );
    #[cfg(feature = "smp-diagnostics")]
    if ret != 0 || len >= 0x10_0000 {
        crate::println!(
            "[mprotect-diag] tid={} start={:#x} len={:#x} prot={:#x} ret={}",
            pid,
            start,
            len,
            prot,
            ret,
        );
    }
    if ret == 0 {
        let snapshot = shared_vm_sync_snapshot(&inner);
        let activity = inner.memory_set.activity_handle();
        drop(inner);
        sync_thread_group_vm_snapshot(pid, snapshot);
        // `mprotect` only changes PTE permissions; it does not retire frames.
        // The remote fence therefore occurs safely after all TaskLocks and
        // shared-VM metadata locks have been released.
        let _ = flush_remote_tlb(&activity, start, len);
    }
    ret
}

// ---- mremap (216) ----

const MREMAP_MAYMOVE: usize = 1;
const MREMAP_FIXED: usize = 2;
const MREMAP_DONTUNMAP: usize = 4;

pub fn sys_mremap(
    old_addr: usize,
    old_size: usize,
    new_size: usize,
    flags: usize,
    new_addr: usize,
) -> isize {
    #[cfg(any(feature = "smp-diagnostics", feature = "timeout-trace"))]
    crate::println!(
        "[mremap-diag] enter tid={} old={:#x} old_size={:#x} new_size={:#x} flags={:#x} new={:#x}",
        current_task().map(|task| task.getpid()).unwrap_or(0),
        old_addr,
        old_size,
        new_size,
        flags,
        new_addr,
    );
    if old_addr == 0
        || old_addr % crate::config::PAGE_SIZE != 0
        || old_size == 0
        || new_size == 0
        || flags & !(MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP) != 0
    {
        return -22;
    }
    if flags & MREMAP_DONTUNMAP != 0 {
        // DONTUNMAP changes the source mapping into a faulting placeholder.
        // Keep the operation atomic until that distinct Linux behavior is
        // implemented rather than silently losing either mapping.
        return -95;
    }
    if flags & MREMAP_FIXED != 0 {
        if flags & MREMAP_MAYMOVE == 0
            || new_addr == 0
            || new_addr % crate::config::PAGE_SIZE != 0
        {
            return -22;
        }
        // The ordinary MAYMOVE path is complete below.  FIXED additionally
        // needs MAP_FIXED-style destination replacement, which must retain
        // destination frames until the remote TLB shootdown has finished.
        return -95;
    }

    let Some(round_old) = old_size
        .checked_add(crate::config::PAGE_SIZE - 1)
        .map(|size| size & !(crate::config::PAGE_SIZE - 1))
    else {
        return -12;
    };
    let Some(round_new) = new_size
        .checked_add(crate::config::PAGE_SIZE - 1)
        .map(|size| size & !(crate::config::PAGE_SIZE - 1))
    else {
        return -12;
    };

    let task = current_task().unwrap();
    let pid = task.getpid();
    let mut inner = task.inner_exclusive_access();
    let remap = match inner.memory_set.mremap(
        polyhal::VirtAddr::from(old_addr),
        round_old,
        round_new,
        flags & MREMAP_MAYMOVE != 0,
    ) {
        Ok(remap) => remap,
        Err(errno) => {
            #[cfg(feature = "smp-diagnostics")]
            crate::println!("[mremap-diag] reject errno={}", errno);
            return errno;
        }
    };
    let activity = inner.memory_set.activity_handle();
    let snapshot = shared_vm_sync_snapshot(&inner);
    drop(inner);
    sync_thread_group_vm_snapshot(pid, snapshot);
    if remap.old_mapping_changed() {
        let _ = flush_remote_tlb(&activity, old_addr, round_old);
    }
    let result = remap.new_addr() as isize;
    #[cfg(feature = "smp-diagnostics")]
    crate::println!(
        "[mremap-diag] leave tid={} result={:#x} changed={}",
        pid,
        result,
        remap.old_mapping_changed(),
    );
    remap.release();
    result
}

// ---- mincore (232) ----

pub fn sys_mincore(start: usize, len: usize, vec: *mut u8) -> isize {
    if start % crate::config::PAGE_SIZE != 0 || len == 0 {
        return -22;
    }
    let Some(npages) = len
        .checked_add(crate::config::PAGE_SIZE - 1)
        .map(|len| len / crate::config::PAGE_SIZE)
    else {
        return -12;
    };
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner.memory_set.user_range_mapped(start, len) {
        return -12;
    }
    if !inner.memory_set.valid_user_buf(vec, npages, true) {
        return -14;
    }

    let mut residency = Vec::new();
    residency.resize(npages, 0u8);
    let mut page = start;
    for bit in residency.iter_mut() {
        if inner
            .memory_set
            .translate(polyhal::VirtAddr::from(page))
            .is_some()
        {
            *bit = 1;
        }
        page += crate::config::PAGE_SIZE;
    }
    drop(inner);
    let token = current_user_token();
    let buf = crate::mm::translated_byte_buffer(token, vec, npages);
    for (dst, src) in buf.iter_mut().zip(residency.iter()) {
        *dst = *src;
    }
    0
}

// ---- mlock/mlock2/mlockall/munlock/munlockall (228-231) ----

fn page_range_for_lock(start: usize, len: usize) -> Result<(usize, usize, usize), isize> {
    let Some(last) = start.checked_add(len.saturating_sub(1)) else {
        return Err(-12);
    };
    let page_start = start & !(crate::config::PAGE_SIZE - 1);
    let Some(page_end) =
        (last & !(crate::config::PAGE_SIZE - 1)).checked_add(crate::config::PAGE_SIZE)
    else {
        return Err(-12);
    };
    let Some(rounded_len) = page_end.checked_sub(page_start) else {
        return Err(-12);
    };
    Ok((page_start, page_end, rounded_len))
}

fn check_memlock_limit(len: usize) -> isize {
    let Some(task) = current_task() else {
        return -1;
    };
    let inner = task.inner_exclusive_access();
    if inner.euid == 0 {
        return 0;
    }
    if inner.rlimit_memlock == 0 {
        return -1;
    }
    if len > inner.rlimit_memlock {
        return -12;
    }
    0
}

pub fn sys_mlock(start: usize, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    let Ok((page_start, page_end, rounded_len)) = page_range_for_lock(start, len) else {
        return -12;
    };
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner.memory_set.user_range_mapped(start, len) {
        return -12;
    }
    drop(inner);
    let limit = check_memlock_limit(rounded_len);
    if limit != 0 {
        return limit;
    }
    let mut inner = task.inner_exclusive_access();
    let mut page = page_start;
    while page < page_end {
        let vaddr = polyhal::VirtAddr::from(page);
        if inner.memory_set.translate(vaddr).is_none()
            && inner
                .memory_set
                .handle_page_fault(vaddr, crate::mm::PageFaultAccess::Read)
                .is_none()
        {
            return -12;
        }
        page += crate::config::PAGE_SIZE;
    }
    0
}

pub fn sys_munlock(start: usize, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    if page_range_for_lock(start, len).is_err() {
        return -12;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner.memory_set.user_range_mapped(start, len) {
        return -12;
    }
    0
}

pub fn sys_mlockall(flags: usize) -> isize {
    const MCL_CURRENT: usize = 1;
    const MCL_FUTURE: usize = 2;
    if flags == 0 || flags & !(MCL_CURRENT | MCL_FUTURE) != 0 {
        return -22;
    }
    let limit = check_memlock_limit(usize::MAX);
    if limit != 0 {
        return limit;
    }
    0
}

pub fn sys_munlockall() -> isize {
    0
}

pub fn sys_mlock2(start: usize, len: usize, flags: usize) -> isize {
    const MLOCK_ONFAULT: usize = 1;
    if flags & !MLOCK_ONFAULT != 0 {
        return -22;
    }
    if flags == 0 {
        return sys_mlock(start, len);
    }
    if len == 0 {
        return 0;
    }
    let Ok((_, _, rounded_len)) = page_range_for_lock(start, len) else {
        return -12;
    };
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner.memory_set.user_range_mapped(start, len) {
        return -12;
    }
    drop(inner);
    let limit = check_memlock_limit(rounded_len);
    if limit != 0 {
        return limit;
    }
    0
}

// ---- getrlimit/setrlimit (163/164) ----

const RLIMIT_NOFILE: usize = 7;
const RLIMIT_MEMLOCK: usize = 8;
const RLIMIT_STACK: usize = 3;
const RLIMIT_AS: usize = 9;
const RLIM_NLIMITS: usize = 16;
const RLIM_INFINITY: usize = usize::MAX;

#[repr(C)]
struct RLimit {
    rlim_cur: usize,
    rlim_max: usize,
}

pub fn sys_getrlimit(resource: usize, rlim_ptr: *mut u8) -> isize {
    if resource >= RLIM_NLIMITS {
        return -22;
    }
    if rlim_ptr.is_null() {
        return -14;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(rlim_ptr, core::mem::size_of::<RLimit>(), true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    let rlim = crate::mm::translated_refmut(token, rlim_ptr as *mut RLimit);
    match resource {
        RLIMIT_NOFILE => {
            rlim.rlim_cur = inner.rlimit_nofile;
            rlim.rlim_max = inner.rlimit_nofile_max;
        }
        RLIMIT_MEMLOCK => {
            rlim.rlim_cur = inner.rlimit_memlock;
            rlim.rlim_max = inner.rlimit_memlock_max;
        }
        _ => {
            rlim.rlim_cur = RLIM_INFINITY;
            rlim.rlim_max = RLIM_INFINITY;
        }
    }
    0
}

pub fn sys_setrlimit(resource: usize, rlim_ptr: *const u8) -> isize {
    if resource >= RLIM_NLIMITS {
        return -22;
    }
    if rlim_ptr.is_null() {
        return -14;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(rlim_ptr, core::mem::size_of::<RLimit>(), false)
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let rlim = translated_ref(token, rlim_ptr as *const RLimit);
    let mut inner = task.inner_exclusive_access();
    match resource {
        RLIMIT_NOFILE => {
            inner.rlimit_nofile = rlim.rlim_cur.min(1024);
            inner.rlimit_nofile_max = rlim.rlim_max.min(1024);
        }
        RLIMIT_MEMLOCK => {
            inner.rlimit_memlock = rlim.rlim_cur;
            inner.rlimit_memlock_max = rlim.rlim_max;
        }
        _ => {}
    }
    0
}

pub fn sys_prlimit64(
    pid: usize,
    resource: usize,
    new_limit: *const u8,
    old_limit: *mut u8,
) -> isize {
    if resource >= RLIM_NLIMITS {
        return -22;
    }
    let task = if pid == 0 {
        current_task().unwrap()
    } else {
        match pid2task(pid) {
            Some(task) => task,
            None => return -3,
        }
    };
    let caller = current_task().unwrap();
    let caller_inner = caller.inner_exclusive_access();
    if !old_limit.is_null()
        && !caller_inner
            .memory_set
            .valid_user_buf(old_limit, core::mem::size_of::<RLimit>(), true)
    {
        return -14;
    }
    if !new_limit.is_null()
        && !caller_inner
            .memory_set
            .valid_user_buf(new_limit, core::mem::size_of::<RLimit>(), false)
    {
        return -14;
    }
    let token = caller_inner.get_user_token();
    drop(caller_inner);
    if !old_limit.is_null() {
        let old = translated_refmut(token, old_limit as *mut RLimit);
        let inner = task.inner_exclusive_access();
        match resource {
            RLIMIT_NOFILE => {
                old.rlim_cur = inner.rlimit_nofile;
                old.rlim_max = inner.rlimit_nofile_max;
            }
            RLIMIT_MEMLOCK => {
                old.rlim_cur = inner.rlimit_memlock;
                old.rlim_max = inner.rlimit_memlock_max;
            }
            _ => {
                old.rlim_cur = RLIM_INFINITY;
                old.rlim_max = RLIM_INFINITY;
            }
        }
    }
    if !new_limit.is_null() {
        let new = translated_ref(token, new_limit as *const RLimit);
        let mut inner = task.inner_exclusive_access();
        match resource {
            RLIMIT_NOFILE => {
                inner.rlimit_nofile = new.rlim_cur.min(1024);
                inner.rlimit_nofile_max = new.rlim_max.min(1024);
            }
            RLIMIT_MEMLOCK => {
                inner.rlimit_memlock = new.rlim_cur;
                inner.rlimit_memlock_max = new.rlim_max;
            }
            _ => {}
        }
    }
    0
}

// ---- sysinfo (179) ----

const SI_LOAD_SHIFT: usize = 16;

#[repr(C)]
struct SysInfo {
    uptime: isize,
    loads: [usize; 3],
    totalram: usize,
    freeram: usize,
    sharedram: usize,
    bufferram: usize,
    totalswap: usize,
    freeswap: usize,
    procs: u16,
    pad: u16,
    totalhigh: usize,
    freehigh: usize,
    mem_unit: u32,
    _f: [u8; 0],
}

pub fn sys_sysinfo(info_ptr: *mut u8) -> isize {
    if info_ptr.is_null() {
        return -14;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(info_ptr, core::mem::size_of::<SysInfo>(), true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let info = crate::mm::translated_refmut(token, info_ptr as *mut SysInfo);
    let now = polyhal::time::Time::now().to_msec() as isize / 1000;
    info.uptime = now;
    info.loads = [0, 0, 0];
    info.totalram = 256 * 1024 * 1024;
    info.freeram = 128 * 1024 * 1024;
    info.sharedram = 0;
    info.bufferram = 0;
    info.totalswap = 0;
    info.freeswap = 0;
    info.procs = 0;
    info.pad = 0;
    info.totalhigh = 0;
    info.freehigh = 0;
    info.mem_unit = 1;
    0
}

// ---- sigaltstack (132) ----

const SS_DISABLE: u32 = 2;
const SS_ONSTACK: u32 = 1;

#[repr(C)]
struct StackT {
    ss_sp: usize,
    ss_flags: i32,
    ss_size: usize,
}

pub fn sys_sigaltstack(ss: *const u8, old_ss: *mut u8) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    if !old_ss.is_null() {
        let old = crate::mm::translated_refmut(token, old_ss as *mut StackT);
        old.ss_sp = inner.alt_stack;
        old.ss_size = inner.alt_stack_size;
        old.ss_flags = inner.alt_stack_flags as i32;
        if inner.alt_stack_flags & SS_ONSTACK != 0 {
            old.ss_flags = SS_ONSTACK as i32;
        }
    }

    if ss.is_null() {
        return 0;
    }

    let new = crate::mm::translated_ref(token, ss as *const StackT);
    if (new.ss_flags as u32 & SS_DISABLE) != 0 {
        inner.alt_stack_flags = SS_DISABLE;
        inner.alt_stack = 0;
        inner.alt_stack_size = 0;
    } else {
        if new.ss_size < 2048 {
            return -22;
        }
        inner.alt_stack = new.ss_sp;
        inner.alt_stack_size = new.ss_size;
        inner.alt_stack_flags = 0;
    }
    0
}

// ---- sigpending / rt_sigpending (136) ----

pub fn sys_rt_sigpending(set: *mut u8, _sigsize: usize) -> isize {
    if set.is_null() {
        return -14;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let dst = crate::mm::translated_refmut(token, set as *mut u64);
    *dst = flags_to_user_sigset(inner.signals);
    0
}

// ---- sigsuspend / rt_sigsuspend (133) ----

pub fn sys_rt_sigsuspend(mask: *const u8, sigsize: usize) -> isize {
    if sigsize != core::mem::size_of::<u64>() {
        return -22;
    }
    if mask.is_null() {
        return -14;
    }

    let token = current_user_token();
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    if !inner
        .memory_set
        .valid_user_buf(mask, core::mem::size_of::<u64>(), false)
    {
        return -14;
    }

    let saved_mask = inner.signal_mask;
    let new_mask_u64 = *crate::mm::translated_ref(token, mask as *const u64);
    let mut new_mask = user_sigset_to_flags(new_mask_u64);
    // Linux never permits applications to block SIGKILL or SIGSTOP.
    new_mask.remove(SignalFlags::SIGKILL | SignalFlags::SIGSTOP);
    inner.signal_mask = new_mask;
    drop(inner);

    loop {
        crate::task::check_itimers();

        let mut inner2 = task.inner_exclusive_access();
        if inner2.killed {
            inner2.signal_mask = saved_mask;
            return -4;
        }
        if let Some(signal) = crate::task::next_interrupting_signal(&inner2) {
            inner2.signal_mask = saved_mask;
            inner2.sigsuspend_delivery = Some(signal);
            return -4;
        }
        drop(inner2);

        stdout_release_if_owner();
        crate::task::suspend_current_and_run_next();
    }
}

fn signal_number(signal: SignalFlags) -> i32 {
    for sig in 1..=MAX_SIG {
        if let Some(flag) = SignalFlags::from_bits(1u64 << sig) {
            if flag == signal {
                return sig as i32;
            }
        }
    }
    0
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RtSignalInfo {
    si_signo: i32,
    si_errno: i32,
    si_code: i32,
    _pad0: i32,
    si_pid: i32,
    si_uid: u32,
    si_status: i32,
    _pad1: i32,
    si_utime: isize,
    si_stime: isize,
    _pad: [u8; 128 - 40],
}

fn write_signal_siginfo(info_ptr: *mut u8, signum: i32) {
    if info_ptr.is_null() {
        return;
    }
    let token = current_user_token();
    *translated_refmut(token, info_ptr as *mut RtSignalInfo) = RtSignalInfo {
        si_signo: signum,
        si_errno: 0,
        si_code: 0,
        _pad0: 0,
        si_pid: 0,
        si_uid: 0,
        si_status: 0,
        _pad1: 0,
        si_utime: 0,
        si_stime: 0,
        _pad: [0; 128 - 40],
    };
}

pub fn sys_rt_sigtimedwait(
    set: *const u8,
    info: *mut u8,
    timeout: *const u8,
    _sigsetsize: usize,
) -> isize {
    if set.is_null() {
        return -14;
    }
    let token = current_user_token();
    let raw_set = *translated_ref(token, set as *const u64);
    let mut wanted = user_sigset_to_flags(raw_set);
    wanted.remove(SignalFlags::SIGKILL | SignalFlags::SIGSTOP);
    if wanted.is_empty() {
        return -22;
    }

    let timeout_ns = if timeout.is_null() {
        None
    } else {
        let ts = *translated_ref(token, timeout as *const crate::task::TimeSpec);
        if ts.tv_nsec >= 1_000_000_000 {
            return -22;
        }
        Some(
            ts.tv_sec
                .saturating_mul(1_000_000_000)
                .saturating_add(ts.tv_nsec),
        )
    };
    let start_ns = Time::now().to_nsec() as usize;
    let deadline_ns = timeout_ns.map(|limit| start_ns.saturating_add(limit));

    loop {
        let task = current_task().unwrap();
        let found = {
            let mut inner = task.inner_exclusive_access();
            if inner.killed {
                return -4;
            }
            let pending = inner.signals & wanted;
            let mut selected = None;
            for sig in 1..=MAX_SIG {
                if let Some(flag) = SignalFlags::from_bits(1u64 << sig) {
                    if pending.contains(flag) {
                        inner.signals.remove(flag);
                        selected = Some(flag);
                        break;
                    }
                }
            }
            selected
        };

        if let Some(signal) = found {
            let signum = signal_number(signal);
            write_signal_siginfo(info, signum);
            return signum as isize;
        }

        if let Some(deadline) = deadline_ns {
            if timeout_ns == Some(0) {
                return -11;
            }
            let now_ns = Time::now().to_nsec() as usize;
            if now_ns >= deadline {
                return -11;
            }
        }

        block_current_until(BlockReason::SignalWait(wanted), deadline_ns);
    }
}

// ---- alarm (no separate syscall on RISC-V, musl uses setitimer) ----

// ---- clock_getres (114) ----

fn is_supported_clock_id(clk_id: usize) -> bool {
    matches!(clk_id, 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7)
}

pub fn sys_clock_getres(clk_id: usize, res: *mut u8) -> isize {
    if !is_supported_clock_id(clk_id) {
        return -22;
    }
    if res.is_null() {
        return 0;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(res, core::mem::size_of::<crate::task::TimeSpec>(), true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let ts = crate::mm::translated_refmut(token, res as *mut crate::task::TimeSpec);
    ts.tv_sec = 0;
    ts.tv_nsec = 1_000_000; // 1ms resolution
    0
}

// ---- getgroups/setgroups (158/159) ----

pub fn sys_getgroups(size: usize, list: *mut u32) -> isize {
    const NGROUPS_MAX: usize = 65536;
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let groups = inner.supplementary_groups.clone();
    let count = groups.len();
    if size > NGROUPS_MAX {
        return -22;
    }
    if size == 0 {
        return count as isize;
    }
    if size < count {
        return -22;
    }
    if list.is_null() {
        return -14;
    }
    let bytes = count * core::mem::size_of::<u32>();
    if !inner
        .memory_set
        .valid_user_buf(list as *const u8, bytes, true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    for (idx, gid) in groups.iter().enumerate() {
        *crate::mm::translated_refmut(token, unsafe { list.add(idx) }) = *gid;
    }
    count as isize
}

pub fn sys_setgroups(size: usize, list: *const u32) -> isize {
    const NGROUPS_MAX: usize = 65536;
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if inner.euid != 0 {
        return -1;
    }
    if size > NGROUPS_MAX {
        return -22;
    }
    if size > 0 && list.is_null() {
        return -14;
    }
    let bytes = size * core::mem::size_of::<u32>();
    if size > 0
        && !inner
            .memory_set
            .valid_user_buf(list as *const u8, bytes, false)
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let mut groups = Vec::with_capacity(size);
    for idx in 0..size {
        groups.push(*translated_ref(token, unsafe { list.add(idx) }));
    }
    let mut inner = task.inner_exclusive_access();
    inner.supplementary_groups = groups;
    0
}

// ---- UID/GID: getresuid(117)/getresgid(120)/setresuid(147)/setresgid(150)/setreuid(145)/setregid(143) ----

pub fn sys_getresuid(ruid: *mut u32, euid: *mut u32, suid: *mut u32) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let word = core::mem::size_of::<u32>();
    for ptr in [ruid, euid, suid] {
        if ptr.is_null()
            || !inner
                .memory_set
                .valid_user_buf(ptr as *const u8, word, true)
        {
            return -14;
        }
    }
    let token = inner.get_user_token();
    *crate::mm::translated_refmut(token, ruid) = inner.uid;
    *crate::mm::translated_refmut(token, euid) = inner.euid;
    *crate::mm::translated_refmut(token, suid) = inner.suid;
    0
}

pub fn sys_getresgid(rgid: *mut u32, egid: *mut u32, sgid: *mut u32) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let word = core::mem::size_of::<u32>();
    for ptr in [rgid, egid, sgid] {
        if ptr.is_null()
            || !inner
                .memory_set
                .valid_user_buf(ptr as *const u8, word, true)
        {
            return -14;
        }
    }
    let token = inner.get_user_token();
    *crate::mm::translated_refmut(token, rgid) = inner.gid;
    *crate::mm::translated_refmut(token, egid) = inner.egid;
    *crate::mm::translated_refmut(token, sgid) = inner.sgid;
    0
}

pub fn sys_setresuid(ruid: usize, euid: usize, suid: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let ruid = match parse_optional_id(ruid) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let euid = match parse_optional_id(euid) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let suid = match parse_optional_id(suid) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if inner.euid != 0 {
        for id in [ruid, euid, suid].into_iter().flatten() {
            if id != inner.uid && id != inner.euid && id != inner.suid {
                return -1;
            }
        }
    }
    if let Some(id) = ruid {
        inner.uid = id;
    }
    if let Some(id) = euid {
        inner.euid = id;
    }
    if let Some(id) = suid {
        inner.suid = id;
    }
    0
}

pub fn sys_setresgid(rgid: usize, egid: usize, sgid: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let rgid = match parse_optional_id(rgid) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let egid = match parse_optional_id(egid) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let sgid = match parse_optional_id(sgid) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if inner.euid != 0 {
        for id in [rgid, egid, sgid].into_iter().flatten() {
            if id != inner.gid && id != inner.egid && id != inner.sgid {
                return -1;
            }
        }
    }
    if let Some(id) = rgid {
        inner.gid = id;
    }
    if let Some(id) = egid {
        inner.egid = id;
    }
    if let Some(id) = sgid {
        inner.sgid = id;
    }
    0
}

pub fn sys_setreuid(ruid: usize, euid: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let ruid = match parse_optional_id(ruid) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let euid = match parse_optional_id(euid) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let old_uid = inner.uid;
    if inner.euid != 0 {
        if let Some(id) = ruid {
            if id != inner.uid && id != inner.euid {
                return -1;
            }
        }
        if let Some(id) = euid {
            if id != inner.uid && id != inner.euid && id != inner.suid {
                return -1;
            }
        }
    }
    let update_suid = ruid.is_some() || euid.map(|id| id != old_uid).unwrap_or(false);
    if let Some(id) = ruid {
        inner.uid = id;
    }
    if let Some(id) = euid {
        inner.euid = id;
    }
    if update_suid {
        inner.suid = inner.euid;
    }
    0
}

pub fn sys_setregid(rgid: usize, egid: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let rgid = match parse_optional_id(rgid) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let egid = match parse_optional_id(egid) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let old_gid = inner.gid;
    if inner.euid != 0 {
        if let Some(id) = rgid {
            if id != inner.gid && id != inner.egid {
                return -1;
            }
        }
        if let Some(id) = egid {
            if id != inner.gid && id != inner.egid && id != inner.sgid {
                return -1;
            }
        }
    }
    let update_sgid = rgid.is_some() || egid.map(|id| id != old_gid).unwrap_or(false);
    if let Some(id) = rgid {
        inner.gid = id;
    }
    if let Some(id) = egid {
        inner.egid = id;
    }
    if update_sgid {
        inner.sgid = inner.egid;
    }
    0
}

// ---- sched_setaffinity/sched_getaffinity (122/123) ----

pub fn sys_sched_setaffinity(pid: usize, len: usize, mask: *const u8) -> isize {
    if len == 0 {
        return -22;
    }
    if mask.is_null() {
        return -14;
    }
    let caller = current_task().unwrap();
    let target = if pid == 0 {
        caller.clone()
    } else {
        match pid2task(pid) {
            Some(task) => task,
            None => return -3,
        }
    };
    let inner = caller.inner_exclusive_access();
    if !inner.memory_set.valid_user_buf(mask, len, false) {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);

    let mut requested = 0usize;
    for (index, byte) in translated_byte_buffer(token, mask as *mut u8, len)
        .iter()
        .enumerate()
    {
        if index < core::mem::size_of::<usize>() {
            requested |= (*byte as usize) << (index * 8);
        } else if *byte != 0 {
            return -22;
        }
    }
    if requested == 0 || requested & !online_cpu_mask() != 0 {
        return -22;
    }
    if crate::task::update_task_affinity(target, requested) {
        0
    } else {
        -3 // ESRCH: the target became a zombie after PID lookup.
    }
}

pub fn sys_sched_getaffinity(pid: usize, len: usize, mask: *mut u8) -> isize {
    let cpuset_bytes = core::mem::size_of::<usize>();
    if len < cpuset_bytes {
        return -22;
    }
    if mask.is_null() {
        return -14;
    }
    let caller = current_task().unwrap();
    let target = if pid == 0 {
        caller.clone()
    } else {
        match pid2task(pid) {
            Some(task) => task,
            None => return -3,
        }
    };
    let inner = caller.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(mask as *const u8, len, true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    // Do not expose unavailable harts on a QEMU_SMP=1 boot. The stored mask
    // remains useful when the same image is later booted with four CPUs, but
    // userspace must receive the CPUs it can actually run on now.
    let affinity = target.allowed_cpus() & online_cpu_mask();
    let buf = translated_byte_buffer(token, mask, len);
    for (index, byte) in buf.iter_mut().enumerate() {
        *byte = if index < core::mem::size_of::<usize>() {
            ((affinity >> (index * 8)) & 0xff) as u8
        } else {
            0
        };
    }
    cpuset_bytes as isize
}

// ---- getpriority/setpriority (140/141) ----

fn priority_targets(which: usize, who: usize) -> Result<Vec<Arc<TaskControlBlock>>, isize> {
    const PRIO_PROCESS: usize = 0;
    const PRIO_PGRP: usize = 1;
    const PRIO_USER: usize = 2;

    match which {
        PRIO_PROCESS => {
            let pid = if who == 0 {
                current_task().map(|task| task.getpid()).ok_or(-3isize)?
            } else {
                who
            };
            pid2task(pid)
                .map(|task| {
                    let mut tasks = Vec::new();
                    tasks.push(task);
                    tasks
                })
                .ok_or(-3)
        }
        PRIO_PGRP => {
            let pgid = if who == 0 {
                current_task()
                    .map(|task| task.inner_exclusive_access().pgid)
                    .ok_or(-3isize)?
            } else {
                who
            };
            let tasks: Vec<Arc<TaskControlBlock>> = task_snapshot()
                .into_iter()
                .filter(|task| {
                    task.try_inner_borrow()
                        .map(|inner| inner.pgid == pgid)
                        .unwrap_or(false)
                })
                .collect();
            if tasks.is_empty() {
                Err(-3)
            } else {
                Ok(tasks)
            }
        }
        PRIO_USER => {
            let uid = if who == 0 {
                current_task()
                    .map(|task| task.inner_exclusive_access().uid as usize)
                    .ok_or(-3isize)?
            } else {
                who
            };
            let tasks: Vec<Arc<TaskControlBlock>> = task_snapshot()
                .into_iter()
                .filter(|task| {
                    task.try_inner_borrow()
                        .map(|inner| inner.uid as usize == uid || inner.euid as usize == uid)
                        .unwrap_or(false)
                })
                .collect();
            if tasks.is_empty() {
                Err(-3)
            } else {
                Ok(tasks)
            }
        }
        _ => Err(-22),
    }
}

pub fn sys_getpriority(which: usize, who: usize) -> isize {
    let tasks = match priority_targets(which, who) {
        Ok(tasks) => tasks,
        Err(errno) => return errno,
    };
    let best = tasks
        .iter()
        .filter_map(|task| task.try_inner_borrow().map(|inner| inner.nice))
        .min()
        .unwrap_or(0);
    (20 - best) as isize
}

pub fn sys_setpriority(which: usize, who: usize, prio: isize) -> isize {
    let caller = current_task().unwrap();
    let (caller_uid, caller_euid) = {
        let inner = caller.inner_exclusive_access();
        (inner.uid, inner.euid)
    };
    let tasks = match priority_targets(which, who) {
        Ok(tasks) => tasks,
        Err(errno) => return errno,
    };
    let nice = prio.clamp(-20, 19) as i32;
    for task in &tasks {
        let inner = task.inner_exclusive_access();
        if caller_euid != 0 {
            if caller_euid != inner.euid && caller_uid != inner.euid {
                return -1;
            }
            if nice < inner.nice {
                return -13;
            }
        }
    }
    for task in tasks {
        task.inner_exclusive_access().nice = nice;
    }
    0
}

// ---- pselect6 ----

const FD_SETSIZE: usize = 1024;
const FD_SET_LONGS: usize = FD_SETSIZE / 64;
type FdSet = [u64; FD_SET_LONGS];

fn fd_isset(set: &FdSet, fd: usize) -> bool {
    if fd >= FD_SETSIZE {
        return false;
    }
    (set[fd / 64] & (1u64 << (fd % 64))) != 0
}

fn fd_set(set: &mut FdSet, fd: usize) {
    if fd < FD_SETSIZE {
        set[fd / 64] |= 1u64 << (fd % 64);
    }
}

fn fd_clr(set: &mut FdSet, fd: usize) {
    if fd < FD_SETSIZE {
        set[fd / 64] &= !(1u64 << (fd % 64));
    }
}

fn copy_fdset(dst: &mut FdSet, src_ptr: *const u8) {
    let token = current_user_token();
    let src: &FdSet = translated_ref(token, src_ptr as *const FdSet);
    *dst = *src;
}

fn write_fdset(dst_ptr: *mut u8, src: &FdSet) {
    let token = current_user_token();
    *translated_refmut(token, dst_ptr as *mut FdSet) = *src;
}

pub fn sys_pselect6(
    nfds: usize,
    readfds: *mut u8,
    writefds: *mut u8,
    exceptfds: *mut u8,
    timeout: *const u8,
    _sigmask: *const u8,
) -> isize {
    if nfds > FD_SETSIZE {
        return -22;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let fdset_size = core::mem::size_of::<FdSet>();
    if !readfds.is_null() && !inner.memory_set.valid_user_buf(readfds, fdset_size, true) {
        return -14;
    }
    if !writefds.is_null() && !inner.memory_set.valid_user_buf(writefds, fdset_size, true) {
        return -14;
    }
    if !exceptfds.is_null() && !inner.memory_set.valid_user_buf(exceptfds, fdset_size, true) {
        return -14;
    }
    if !timeout.is_null()
        && !inner.memory_set.valid_user_buf(
            timeout,
            core::mem::size_of::<crate::task::TimeSpec>(),
            false,
        )
    {
        return -14;
    }
    drop(inner);

    let token = current_user_token();
    let mut rfds = [0u64; FD_SET_LONGS];
    let mut wfds = [0u64; FD_SET_LONGS];
    let mut efds = [0u64; FD_SET_LONGS];

    if !readfds.is_null() {
        copy_fdset(&mut rfds, readfds);
    }
    if !writefds.is_null() {
        copy_fdset(&mut wfds, writefds);
    }
    if !exceptfds.is_null() {
        copy_fdset(&mut efds, exceptfds);
    }
    let orig_rfds = rfds;
    let orig_wfds = wfds;
    let orig_efds = efds;

    let timeout_ms = if !timeout.is_null() {
        let ts: &crate::task::TimeSpec =
            translated_ref(token, timeout as *const crate::task::TimeSpec);
        ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
    } else {
        usize::MAX
    };

    let start_ms = polyhal::time::Time::now().to_msec() as usize;

    loop {
        crate::task::check_itimers();
        rfds = orig_rfds;
        wfds = orig_wfds;
        efds = orig_efds;
        crate::net::net_poll();
        let mut ready = 0i32;
        let n = nfds.min(FD_SETSIZE);

        let task = current_task().unwrap();
        let mut inner = task.inner_exclusive_access();
        if has_interrupting_signal(&inner) {
            return -4;
        }

        let mut fd = 0usize;
        while fd < n {
            let check_read = fd_isset(&rfds, fd);
            let check_write = fd_isset(&wfds, fd);
            let check_except = fd_isset(&efds, fd);

            if !check_read && !check_write && !check_except {
                fd += 1;
                continue;
            }

            let mut readable = false;
            let mut writable = false;
            let mut error = false;

            if fd < inner.fd_table.len() {
                if let Some(file) = &inner.fd_table[fd] {
                    let file = file.clone();
                    drop(inner);
                    let (r, w, e) = file.poll();
                    readable = r;
                    writable = w;
                    error = e;
                    inner = task.inner_exclusive_access();
                } else {
                    drop(inner);
                    return -9;
                }
            } else {
                drop(inner);
                return -9;
            }

            if check_read && readable {
                fd_set(&mut rfds, fd);
                ready += 1;
            } else {
                fd_clr(&mut rfds, fd);
            }
            if check_write && writable {
                fd_set(&mut wfds, fd);
                ready += 1;
            } else {
                fd_clr(&mut wfds, fd);
            }
            if check_except && error {
                fd_set(&mut efds, fd);
                ready += 1;
            } else {
                fd_clr(&mut efds, fd);
            }

            fd += 1;
        }

        drop(inner);
        drop(task);

        if ready > 0 {
            if !readfds.is_null() {
                write_fdset(readfds, &rfds);
            }
            if !writefds.is_null() {
                write_fdset(writefds, &wfds);
            }
            if !exceptfds.is_null() {
                write_fdset(exceptfds, &efds);
            }
            stdout_release_if_owner();
            suspend_current_and_run_next();
            return ready as isize;
        }

        if timeout_ms == 0 {
            if !readfds.is_null() {
                write_fdset(readfds, &rfds);
            }
            if !writefds.is_null() {
                write_fdset(writefds, &wfds);
            }
            if !exceptfds.is_null() {
                write_fdset(exceptfds, &efds);
            }
            return 0;
        }
        if timeout_ms != usize::MAX {
            let now_ms = polyhal::time::Time::now().to_msec() as usize;
            if now_ms - start_ms >= timeout_ms {
                if !readfds.is_null() {
                    write_fdset(readfds, &rfds);
                }
                if !writefds.is_null() {
                    write_fdset(writefds, &wfds);
                }
                if !exceptfds.is_null() {
                    write_fdset(exceptfds, &efds);
                }
                return 0;
            }
        }

        stdout_release_if_owner();
        suspend_current_and_run_next();
    }
}

pub fn sys_futex(
    uaddr: *mut u32,
    futex_op: usize,
    val: usize,
    val2: usize,
    uaddr2: *mut u32,
    val3: usize,
) -> isize {
    if uaddr.is_null() {
        return -14;
    }
    if (uaddr as usize) % core::mem::align_of::<u32>() != 0 {
        return -22;
    }
    let cur = futex_word_load(uaddr);
    let op = futex_op & 0x7F;
    let shared = futex_op & 128 == 0;
    let key = {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        futex_keys_for_addr(&inner, uaddr as usize, shared)[0]
    };
    #[cfg(feature = "smp-diagnostics")]
    record_futex_event(0x100 | futex_op, key, val, cur as isize);
    let result = match op {
        0 => {
            if cur != val as u32 {
                return -11;
            }
            match futex_wait_current(key, uaddr, val as u32, futex_relative_timeout_ns(val2)) {
                FutexWaitResult::Woken => 0,
                FutexWaitResult::TimedOut => -110,
                FutexWaitResult::Interrupted => -4,
                FutexWaitResult::ValueChanged => -11,
            }
        }
        9 => {
            if cur != val as u32 {
                return -11;
            }
            match futex_wait_current(key, uaddr, val as u32, futex_absolute_timeout_ns(val2)) {
                FutexWaitResult::Woken => 0,
                FutexWaitResult::TimedOut => -110,
                FutexWaitResult::Interrupted => -4,
                FutexWaitResult::ValueChanged => -11,
            }
        }
        1 | 10 => futex_wake_key(key, val),
        3 => {
            if uaddr2.is_null() {
                return -14;
            }
            let new_key = {
                let task = current_task().unwrap();
                let inner = task.inner_exclusive_access();
                futex_keys_for_addr(&inner, uaddr2 as usize, shared)[0]
            };
            futex_requeue_key(key, new_key, val, val2)
        }
        4 => {
            if uaddr2.is_null() {
                return -14;
            }
            if cur != val3 as u32 {
                return -11;
            }
            let new_key = {
                let task = current_task().unwrap();
                let inner = task.inner_exclusive_access();
                futex_keys_for_addr(&inner, uaddr2 as usize, shared)[0]
            };
            futex_requeue_key(key, new_key, val, val2)
        }
        6..=8 | 11..=13 => 0,
        _ => -38,
    };
    #[cfg(feature = "smp-diagnostics")]
    record_futex_event(op, key, val, result);
    result
}

enum FutexWaitResult {
    Woken,
    TimedOut,
    Interrupted,
    ValueChanged,
}

/// Read a futex word with acquire semantics after the syscall has validated
/// its user range.  `translated_ref` is deliberately not used while holding
/// `FutexLock`: address-space inspection must complete before a spin lock is
/// acquired, while this atomic load closes the wait-versus-wake race.
#[inline]
fn futex_word_load(uaddr: *const u32) -> u32 {
    // Linux requires a naturally aligned futex word.  `sys_futex` enforces
    // that contract before this helper is reachable.
    unsafe { (&*(uaddr as *const AtomicU32)).load(Ordering::Acquire) }
}

fn futex_relative_timeout_ns(timeout_ptr: usize) -> Option<usize> {
    if timeout_ptr == 0 {
        return None;
    }
    let token = current_user_token();
    let ts = *translated_ref(token, timeout_ptr as *const crate::task::TimeSpec);
    if ts.tv_nsec >= 1_000_000_000 {
        return Some(polyhal::time::Time::now().to_nsec() as usize);
    }
    let duration = ts
        .tv_sec
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec);
    Some((polyhal::time::Time::now().to_nsec() as usize).saturating_add(duration))
}

fn futex_absolute_timeout_ns(timeout_ptr: usize) -> Option<usize> {
    if timeout_ptr == 0 {
        return None;
    }
    let token = current_user_token();
    let ts = *translated_ref(token, timeout_ptr as *const crate::task::TimeSpec);
    if ts.tv_nsec >= 1_000_000_000 {
        return Some(polyhal::time::Time::now().to_nsec() as usize);
    }
    Some(
        ts.tv_sec
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec),
    )
}

pub(crate) fn remove_futex_waiter(pid: usize) -> bool {
    let mut removed = false;
    let mut waiters = FUTEX_WAITERS.exclusive_access();
    waiters.retain(|_, queue| {
        let old_len = queue.len();
        queue.retain(|task| task.getpid() != pid);
        if queue.len() != old_len {
            removed = true;
        }
        !queue.is_empty()
    });
    removed
}

#[cfg(feature = "smp-diagnostics")]
pub(crate) fn dump_futex_waiters() {
    let snapshot: Vec<(usize, usize, Vec<usize>)> = {
        let waiters = FUTEX_WAITERS.exclusive_access();
        waiters
            .iter()
            .map(|(key, queue)| {
                (
                    key.namespace,
                    key.addr,
                    queue.iter().map(|task| task.getpid()).collect(),
                )
            })
            .collect()
    };
    for (namespace, addr, tids) in snapshot {
        crate::println!(
            "[futex-stall] namespace={:#x} addr={:#x} tids={:?}",
            namespace,
            addr,
            tids,
        );
    }
    let end = FUTEX_EVENT_NEXT.load(Ordering::Acquire);
    let start = end.saturating_sub(FUTEX_EVENT_CAPACITY);
    for sequence in start..end {
        let slot = sequence % FUTEX_EVENT_CAPACITY;
        crate::println!(
            "[futex-event] seq={} tid={} op={} namespace={:#x} addr={:#x} val={} ret={}",
            sequence,
            FUTEX_EVENT_TID[slot].load(Ordering::Relaxed),
            FUTEX_EVENT_OP[slot].load(Ordering::Relaxed),
            FUTEX_EVENT_NAMESPACE[slot].load(Ordering::Relaxed),
            FUTEX_EVENT_ADDR[slot].load(Ordering::Relaxed),
            FUTEX_EVENT_VAL[slot].load(Ordering::Relaxed),
            FUTEX_EVENT_RET[slot].load(Ordering::Relaxed) as isize,
        );
    }
}

fn futex_wake_key(key: FutexKey, count: usize) -> isize {
    if count == 0 {
        return 0;
    }
    let mut wake_tasks = Vec::new();
    {
        let mut waiters = FUTEX_WAITERS.exclusive_access();
        let Some(queue) = waiters.get_mut(&key) else {
            return 0;
        };
        while wake_tasks.len() < count && !queue.is_empty() {
            wake_tasks.push(queue.remove(0));
        }
        if queue.is_empty() {
            waiters.remove(&key);
        }
    }
    let mut woke = 0isize;
    for task in wake_tasks {
        if crate::task::wake_task(task) {
            woke += 1;
        }
    }
    woke
}

fn futex_requeue_key(
    old_key: FutexKey,
    new_key: FutexKey,
    wake_count: usize,
    requeue_count: usize,
) -> isize {
    let mut waiters = FUTEX_WAITERS.exclusive_access();
    let Some(queue) = waiters.get_mut(&old_key) else {
        return 0;
    };
    let mut wake_tasks = Vec::new();
    while wake_tasks.len() < wake_count && !queue.is_empty() {
        wake_tasks.push(queue.remove(0));
    }
    let mut moved_tasks = Vec::new();
    while moved_tasks.len() < requeue_count && !queue.is_empty() {
        moved_tasks.push(queue.remove(0));
    }
    if queue.is_empty() {
        waiters.remove(&old_key);
    }
    let moved = moved_tasks.len();
    if moved > 0 {
        waiters
            .entry(new_key)
            .or_insert_with(Vec::new)
            .extend(moved_tasks);
    }
    drop(waiters);

    let mut woke = 0isize;
    for task in wake_tasks {
        if crate::task::wake_task(task) {
            woke += 1;
        }
    }
    woke + moved as isize
}

fn futex_wait_current(
    key: FutexKey,
    uaddr: *const u32,
    expected: u32,
    deadline_ns: Option<usize>,
) -> FutexWaitResult {
    if let Some(deadline) = deadline_ns {
        if polyhal::time::Time::now().to_nsec() as usize >= deadline {
            return FutexWaitResult::TimedOut;
        }
    }
    stdout_release_if_owner();
    let task = current_task().unwrap();
    let pid = task.getpid();
    task.record_time_suspend();

    let task_cx_ptr = {
        // FutexLock precedes SchedulerLock.  Holding it until `begin_block`
        // has published `Blocking` makes registration and state transition a
        // single operation from the perspective of FUTEX_WAKE.
        let mut waiters = FUTEX_WAITERS.exclusive_access();
        if futex_word_load(uaddr) != expected {
            task.record_time_resume();
            return FutexWaitResult::ValueChanged;
        }
        let queue = waiters.entry(key).or_insert_with(Vec::new);
        queue.retain(|queued| queued.getpid() != pid);
        queue.push(task.clone());
        crate::task::begin_block(
            &task,
            Some(BlockReason::Futex(key.namespace, key.addr)),
            crate::task::BlockQueue::None,
            deadline_ns,
        )
    };
    crate::task::schedule(task_cx_ptr);
    task.record_time_resume();
    let mut still_waiting = false;
    {
        let mut waiters = FUTEX_WAITERS.exclusive_access();
        if let Some(queue) = waiters.get_mut(&key) {
            let old_len = queue.len();
            queue.retain(|queued| queued.getpid() != pid);
            still_waiting = queue.len() != old_len;
            if queue.is_empty() {
                waiters.remove(&key);
            }
        }
    }
    if deadline_ns.is_some() {
        let _ = crate::task::remove_sleep(pid);
    }
    let inner = task.inner_exclusive_access();
    let interrupted = has_interrupting_signal(&inner);
    #[cfg(feature = "smp-diagnostics")]
    if interrupted {
        crate::println!(
            "[futex-diag] tid={} key={:#x}:{:#x} killed={} exec_exit={} signals={:#x} mask={:#x} handling={}",
            pid,
            key.namespace,
            key.addr,
            inner.killed,
            inner.terminating_for_exec,
            inner.signals.bits(),
            inner.signal_mask.bits(),
            inner.handling_sig,
        );
    }
    drop(inner);
    if interrupted {
        FutexWaitResult::Interrupted
    } else if still_waiting && deadline_ns.is_some() {
        FutexWaitResult::TimedOut
    } else {
        FutexWaitResult::Woken
    }
}

pub fn sys_madvise(start: usize, len: usize, advice: usize) -> isize {
    const MADV_NORMAL: usize = 0;
    const MADV_RANDOM: usize = 1;
    const MADV_SEQUENTIAL: usize = 2;
    const MADV_WILLNEED: usize = 3;
    const MADV_DONTNEED: usize = 4;
    const MADV_FREE: usize = 8;
    const MADV_REMOVE: usize = 9;
    const MADV_DONTFORK: usize = 10;
    const MADV_DOFORK: usize = 11;
    const MADV_MERGEABLE: usize = 12;
    const MADV_UNMERGEABLE: usize = 13;
    const MADV_HUGEPAGE: usize = 14;
    const MADV_NOHUGEPAGE: usize = 15;
    const MADV_DONTDUMP: usize = 16;
    const MADV_DODUMP: usize = 17;
    const MADV_WIPEONFORK: usize = 18;
    const MADV_KEEPONFORK: usize = 19;
    const MADV_COLD: usize = 20;
    const MADV_PAGEOUT: usize = 21;

    if len == 0 {
        return 0;
    }
    if start % crate::config::PAGE_SIZE != 0 {
        return -22;
    }
    let Some(end) = start.checked_add(len.saturating_sub(1)) else {
        return -22;
    };
    if end < start {
        return -22;
    }

    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if !inner.memory_set.user_range_mapped(start, len) {
        return -12;
    }

    let ret = match advice {
        MADV_NORMAL | MADV_RANDOM | MADV_SEQUENTIAL | MADV_WILLNEED | MADV_DONTFORK
        | MADV_DOFORK | MADV_HUGEPAGE | MADV_NOHUGEPAGE | MADV_DONTDUMP | MADV_DODUMP
        | MADV_KEEPONFORK | MADV_COLD | MADV_PAGEOUT => 0,
        MADV_DONTNEED => inner
            .memory_set
            .madvise_private_anonymous_dontneed(polyhal::VirtAddr::from(start), len),
        MADV_FREE | MADV_REMOVE | MADV_MERGEABLE | MADV_UNMERGEABLE | MADV_WIPEONFORK => -22,
        _ => -22,
    };
    #[cfg(feature = "smp-diagnostics")]
    if ret != 0 || len >= 0x10_0000 {
        crate::println!(
            "[madvise-diag] tid={} start={:#x} len={:#x} advice={} ret={}",
            task.getpid(),
            start,
            len,
            advice,
            ret,
        );
    }
    ret
}

pub fn sys_set_robust_list(head: usize, len: usize) -> isize {
    if len != ROBUST_LIST_LEN {
        return -22;
    }
    if head == 0 {
        process_current_robust_list(true);
        return 0;
    }
    let task = current_task().unwrap();
    {
        let inner = task.inner_exclusive_access();
        if !inner
            .memory_set
            .valid_user_buf(head as *const u8, ROBUST_LIST_LEN, false)
        {
            return -14;
        }
    }
    clear_stale_musl_killlock(&task, head);
    let mut inner = task.inner_exclusive_access();
    inner.robust_list_head = head;
    inner.robust_list_len = len;
    0
}

pub fn sys_get_robust_list(pid: usize, head_ptr: *mut usize, len_ptr: *mut usize) -> isize {
    if head_ptr.is_null() || len_ptr.is_null() {
        return -14;
    }
    let current = current_task().unwrap();
    {
        let inner = current.inner_exclusive_access();
        if !inner.memory_set.valid_user_buf(
            head_ptr as *const u8,
            core::mem::size_of::<usize>(),
            true,
        ) || !inner.memory_set.valid_user_buf(
            len_ptr as *const u8,
            core::mem::size_of::<usize>(),
            true,
        ) {
            return -14;
        }
    }
    let target = if pid == 0 || pid == current.getpid() {
        current
    } else {
        match pid2task(pid) {
            Some(task) => task,
            None => return -3,
        }
    };
    let (head, len) = {
        let inner = target.inner_exclusive_access();
        (inner.robust_list_head, inner.robust_list_len)
    };
    let token = current_user_token();
    *translated_refmut(token, head_ptr) = head;
    *translated_refmut(token, len_ptr) = if len == 0 { ROBUST_LIST_LEN } else { len };
    0
}

const ITIMER_REAL: usize = 0;
const ITIMER_VIRTUAL: usize = 1;
const ITIMER_PROF: usize = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct ITimerVal {
    it_interval: TimeVal,
    it_value: TimeVal,
}

pub fn sys_getitimer(which: usize, curr_value_ptr: *mut u8) -> isize {
    if which > ITIMER_PROF {
        return -22;
    }
    if curr_value_ptr.is_null() {
        return -14;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(curr_value_ptr, core::mem::size_of::<ITimerVal>(), true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    let now = polyhal::time::Time::now().to_usec() as u64;
    let remaining_usec = if inner.itimer_expire > now {
        inner.itimer_expire - now
    } else {
        0
    };

    let itv = if inner.itimer_expire == 0 {
        ITimerVal {
            it_interval: TimeVal { sec: 0, usec: 0 },
            it_value: TimeVal { sec: 0, usec: 0 },
        }
    } else {
        let old_interval_sec = (inner.itimer_interval_us / 1_000_000) as isize;
        let old_interval_usec = (inner.itimer_interval_us % 1_000_000) as isize;
        let old_rem_sec = (remaining_usec / 1_000_000) as isize;
        let old_rem_usec = (remaining_usec % 1_000_000) as isize;
        ITimerVal {
            it_interval: TimeVal {
                sec: old_interval_sec,
                usec: old_interval_usec,
            },
            it_value: TimeVal {
                sec: old_rem_sec,
                usec: old_rem_usec,
            },
        }
    };
    drop(inner);
    *translated_refmut(token, curr_value_ptr as *mut ITimerVal) = itv;
    0
}

pub fn sys_setitimer(which: usize, new_value: *const u8, old_value: *mut u8) -> isize {
    if which > ITIMER_PROF {
        return -22;
    }

    let token = current_user_token();
    let task = current_task().unwrap();
    let now = polyhal::time::Time::now().to_usec() as u64;

    if !old_value.is_null() {
        {
            let inner = task.inner_exclusive_access();
            if !inner
                .memory_set
                .valid_user_buf(old_value, core::mem::size_of::<ITimerVal>(), true)
            {
                return -14;
            }
        }
        let mut inner = task.inner_exclusive_access();
        let remaining_usec = if inner.itimer_expire > now {
            inner.itimer_expire - now
        } else {
            0
        };
        let old_interval_sec = (inner.itimer_interval_us / 1_000_000) as isize;
        let old_interval_usec = (inner.itimer_interval_us % 1_000_000) as isize;
        let old_rem_sec = (remaining_usec / 1_000_000) as isize;
        let old_rem_usec = (remaining_usec % 1_000_000) as isize;
        let old_itv = if inner.itimer_expire == 0 {
            ITimerVal {
                it_interval: TimeVal { sec: 0, usec: 0 },
                it_value: TimeVal { sec: 0, usec: 0 },
            }
        } else {
            ITimerVal {
                it_interval: TimeVal {
                    sec: old_interval_sec,
                    usec: old_interval_usec,
                },
                it_value: TimeVal {
                    sec: old_rem_sec,
                    usec: old_rem_usec,
                },
            }
        };
        drop(inner);
        *translated_refmut(token, old_value as *mut ITimerVal) = old_itv;
    }

    if new_value.is_null() {
        return 0;
    }

    {
        let inner = task.inner_exclusive_access();
        if !inner
            .memory_set
            .valid_user_buf(new_value, core::mem::size_of::<ITimerVal>(), false)
        {
            return -14;
        }
    }
    let new_itv: &ITimerVal = translated_ref(token, new_value as *const ITimerVal);
    if new_itv.it_value.sec < 0
        || new_itv.it_value.usec < 0
        || new_itv.it_value.usec >= 1_000_000
        || new_itv.it_interval.sec < 0
        || new_itv.it_interval.usec < 0
        || new_itv.it_interval.usec >= 1_000_000
    {
        return -22;
    }
    let Some(new_value_usec) = (new_itv.it_value.sec as u64)
        .checked_mul(1_000_000)
        .and_then(|sec| sec.checked_add(new_itv.it_value.usec as u64))
    else {
        return -22;
    };
    let Some(new_interval_usec) = (new_itv.it_interval.sec as u64)
        .checked_mul(1_000_000)
        .and_then(|sec| sec.checked_add(new_itv.it_interval.usec as u64))
    else {
        return -22;
    };

    let expire = {
        let mut inner = task.inner_exclusive_access();
        if new_value_usec == 0 {
            inner.itimer_expire = 0;
        } else {
            let Some(expire) = now.checked_add(new_value_usec) else {
                return -22;
            };
            inner.itimer_expire = expire;
        }
        inner.itimer_interval_us = new_interval_usec;
        inner.itimer_expire
    };
    #[cfg(feature = "smp-diagnostics")]
    crate::println!(
        "[itimer-diag] set tid={} which={} now={} expire={} interval={}",
        task.getpid(),
        which,
        now,
        expire,
        new_interval_usec,
    );
    0
}

pub fn sys_clock_gettime(clk_id: usize, ts: *mut u8) -> isize {
    if !is_supported_clock_id(clk_id) {
        return -22;
    }
    if ts.is_null() {
        return -14;
    }

    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(ts, core::mem::size_of::<crate::task::TimeSpec>(), true)
    {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let now_us = monotonic_user_time_us();
    let ts_out: &mut crate::task::TimeSpec =
        translated_refmut(token, ts as *mut crate::task::TimeSpec);
    ts_out.tv_sec = (now_us / 1_000_000) as usize;
    ts_out.tv_nsec = ((now_us % 1_000_000) * 1000) as usize;
    0
}

pub fn sys_clock_nanosleep(_clk_id: usize, _flags: usize, req: usize, rem: usize) -> isize {
    sys_nanosleep(req, rem)
}

pub fn sys_readv(fd: usize, iov: *const u8, iovcnt: usize) -> isize {
    use crate::mm::translated_ref;
    const IOV_MAX: usize = 1024;
    let token = current_user_token();
    #[repr(C)]
    struct IoVec {
        iov_base: usize,
        iov_len: usize,
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
        .valid_user_buf(iov, iovcnt * core::mem::size_of::<IoVec>(), false)
    {
        return -14;
    }
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    if file.is_dir() {
        return -21;
    }
    if !file.readable() {
        return -9;
    }
    drop(inner);
    let mut total = 0isize;
    let mut requested = 0usize;
    for i in 0..iovcnt {
        let iov_ptr = unsafe { iov.add(i * core::mem::size_of::<IoVec>()) };
        let iov: &IoVec = translated_ref(token, iov_ptr as *const IoVec);
        if iov.iov_len == 0 {
            continue;
        }
        if iov.iov_len > isize::MAX as usize {
            return -22;
        }
        requested = match requested.checked_add(iov.iov_len) {
            Some(sum) if sum <= isize::MAX as usize => sum,
            _ => return -22,
        };
        let mut staging = match super::fs::copy_to_user_after_io(
            &task,
            token,
            iov.iov_base as *mut u8,
            iov.iov_len,
        ) {
            Ok(staging) => staging,
            Err(errno) => return if total > 0 { total } else { errno },
        };
        let n = file.read(staging.as_mut_slice());
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        if n as usize > iov.iov_len {
            return if total > 0 { total } else { -5 };
        }
        if let Err(errno) = super::fs::copy_staged_output_to_user(
            &task,
            token,
            iov.iov_base as *mut u8,
            &staging.as_slice()[..n as usize],
        ) {
            return if total > 0 { total } else { errno };
        }
        total += n as isize;
        if (n as usize) < iov.iov_len {
            break;
        }
    }
    total
}

pub fn sys_writev(fd: usize, iov: *const u8, iovcnt: usize) -> isize {
    use crate::mm::translated_ref;
    const IOV_MAX: usize = 1024;
    let token = current_user_token();
    #[repr(C)]
    struct IoVec {
        iov_base: usize,
        iov_len: usize,
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
        .valid_user_buf(iov, iovcnt * core::mem::size_of::<IoVec>(), false)
    {
        return -14;
    }
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    if file.is_dir() {
        return -21;
    }
    if !file.writable() {
        return -9;
    }
    drop(inner);
    let mut buffers = Vec::new();
    let mut requested = 0usize;
    for i in 0..iovcnt {
        let iov_ptr = unsafe { iov.add(i * core::mem::size_of::<IoVec>()) };
        let iov: &IoVec = translated_ref(token, iov_ptr as *const IoVec);
        if iov.iov_len == 0 {
            continue;
        }
        requested = match requested.checked_add(iov.iov_len) {
            Some(sum) if sum <= isize::MAX as usize => sum,
            _ => return -22,
        };
        let staging = match super::fs::copy_from_user_for_io(
            &task,
            token,
            iov.iov_base as *mut u8,
            iov.iov_len,
        ) {
            Ok(staging) => staging,
            Err(errno) => return errno,
        };
        buffers.push(staging);
    }
    let mut total = 0isize;
    for mut staging in buffers {
        let len = staging.as_slice().len();
        let n = file.write(staging.as_mut_slice());
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        total += n as isize;
        if (n as usize) < len {
            break;
        }
    }
    total
}

pub fn sys_sendfile(out_fd: usize, in_fd: usize, offset_ptr: *mut usize, count: usize) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();

    if in_fd >= inner.fd_table.len() || inner.fd_table[in_fd].is_none() {
        return -9;
    }
    if out_fd >= inner.fd_table.len() || inner.fd_table[out_fd].is_none() {
        return -9;
    }
    if !offset_ptr.is_null()
        && !inner.memory_set.valid_user_buf(
            offset_ptr as *const u8,
            core::mem::size_of::<usize>(),
            true,
        )
    {
        return -14;
    }

    let in_file = inner.fd_table[in_fd].as_ref().unwrap().clone();
    let out_file = inner.fd_table[out_fd].as_ref().unwrap().clone();

    if !in_file.readable() || !out_file.writable() {
        return -9;
    }

    let offset_state = if !offset_ptr.is_null() {
        let off = in_file.get_offset();
        let user_off = *translated_ref(token, offset_ptr);
        if user_off > isize::MAX as usize {
            return -22;
        }
        in_file.seek(user_off);
        Some((off, user_off))
    } else {
        None
    };

    drop(inner);

    let mut total: isize = 0;
    let mut buf = [0u8; 4096];
    while (total as usize) < count {
        let chunk = (count - total as usize).min(buf.len());
        let n = in_file.read(&mut buf[..chunk]);
        if n <= 0 {
            break;
        }
        let written = out_file.write(&mut buf[..n as usize]);
        if written <= 0 {
            break;
        }
        total += written;
        if written < n {
            break;
        }
    }

    if let Some((saved_off, user_off)) = offset_state {
        *translated_refmut(token, offset_ptr) = user_off.saturating_add(total.max(0) as usize);
        in_file.seek(saved_off);
    }

    total
}

pub fn sys_getrusage(who: usize, usage: *mut u8) -> isize {
    const RUSAGE_SELF: usize = 0;
    const RUSAGE_THREAD: usize = 1;
    const RUSAGE_CHILDREN: usize = usize::MAX;
    if who != RUSAGE_SELF && who != RUSAGE_THREAD && who != RUSAGE_CHILDREN {
        return -22;
    }
    if usage.is_null() {
        return -14;
    }
    let task = current_task().unwrap();
    {
        let inner = task.inner_exclusive_access();
        if !inner.memory_set.valid_user_buf(
            usage as *const u8,
            core::mem::size_of::<RUsage>(),
            true,
        ) {
            return -14;
        }
    }
    task.record_time_suspend();
    task.record_time_resume();
    let inner = task.inner_exclusive_access();
    let (utime, stime) = if who == RUSAGE_SELF || who == RUSAGE_THREAD {
        (inner.utime, inner.stime)
    } else {
        (inner.cutime, inner.cstime)
    };
    let token = inner.get_user_token();
    drop(inner);
    let rusage = RUsage {
        ru_utime: nsec_to_timeval(utime),
        ru_stime: nsec_to_timeval(stime),
        ru_maxrss: 0,
        ru_ixrss: 0,
        ru_idrss: 0,
        ru_isrss: 0,
        ru_minflt: 0,
        ru_majflt: 0,
        ru_nswap: 0,
        ru_inblock: 0,
        ru_oublock: 0,
        ru_msgsnd: 0,
        ru_msgrcv: 0,
        ru_nsignals: 0,
        ru_nvcsw: 0,
        ru_nivcsw: 0,
    };
    *translated_refmut(token, usage as *mut RUsage) = rusage;
    0
}
