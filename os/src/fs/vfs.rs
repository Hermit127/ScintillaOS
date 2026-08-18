use crate::sync::{
    DentryLock, FsAuxLock, FsGlobalLock, FsGlobalReadGuard, FsGuard, FsLock, SleepGuard,
    SleepLock, VfsLock,
};
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ffi::c_char;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use lazy_static::*;

use super::block_device_stream::{BlockDeviceStream, BlockDeviceStreamOp, Ext4Wrapper};
use lwext4_rust::bindings::*;
use lwext4_rust::Ext4BlockWrapper;

const PATH_OWNER_CACHE_MAX_ENTRIES: usize = 1024;
const INODE_RUNTIME_SIZE_MAX_ENTRIES: usize = 262_144;
const INODE_RUNTIME_SIZE_UNKNOWN: u64 = u64::MAX;
const DENTRY_PARENT_EPOCH_UNKNOWN: u64 = u64::MAX;
const PENDING_WRITEBACK_CHUNK_SIZE: usize = 1024 * 1024;
const PENDING_WRITEBACK_PER_INODE_LIMIT: usize = 8 * 1024 * 1024;
const PENDING_WRITEBACK_GLOBAL_MAX: usize = 64 * 1024 * 1024;
const ROOT_LWEXT4_MOUNT: &str = "/__rootfs/";
const ROOT_LWEXT4_BIN: &str = "/__rootfs/bin";
// The first SMP release has no mount hotplug requirement.  Reserving the
// small mount table up front keeps Vec growth (and therefore allocation) out
// of VfsLock critical sections.
const MOUNT_TABLE_CAPACITY: usize = 16;

lazy_static! {
    // lwext4 keeps process-global mount/cache state internally.  Per-volume
    // locks are insufficient in the dual-disk SMP configuration: operations
    // on rootfs and the test disk must not enter lwext4 concurrently.
    static ref LWEXT4_GLOBAL: FsGlobalLock<()> =
        FsGlobalLock::new_named((), "LwExt4Global");
    static ref PATH_OWNER_CACHE: VfsLock<BTreeMap<String, (u32, u32)>> =
        VfsLock::new_named(BTreeMap::new(), "VfsPathOwnerCache");
    static ref INODE_RUNTIME_SIZES: FsAuxLock<BTreeMap<(usize, u32), Arc<InodeRuntimeSize>>> =
        FsAuxLock::new_named(BTreeMap::new(), "VfsInodeRuntimeSizes");
}

static PENDING_WRITE_BYTES: AtomicUsize = AtomicUsize::new(0);
static NEXT_PENDING_DIRTY_SEQUENCE: AtomicU64 = AtomicU64::new(1);
#[cfg(feature = "lock-stats")]
static PENDING_CACHE_READS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static PENDING_CACHE_READ_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static PENDING_CACHE_OVERLAY_READS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static PENDING_CACHE_OVERLAY_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static PENDING_CACHE_FLUSHES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static PENDING_CACHE_FLUSHED_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static PENDING_CACHE_PRESSURE_FLUSHES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static PENDING_CACHE_DEFERRED_CLOSES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static VFS_INODE_INFO_QUERIES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static VFS_INODE_INFO_FOUND: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static VFS_INODE_INFO_ABSENT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static VFS_INODE_INFO_ERRORS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static VFS_POSITIVE_DENTRY_HITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static VFS_READDIR_DENTRIES_PUBLISHED: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static VFS_DIRFD_EPOCH_HITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static VFS_DIRFD_REVALIDATIONS: AtomicUsize = AtomicUsize::new(0);

struct InodeRuntimeSize {
    value: AtomicU64,
    mutation: SleepLock<()>,
    cached_file_handle: SleepLock<Option<CachedExt4File>>,
    pending_writes: SleepLock<PendingFileWrites>,
    pending_dirty: AtomicBool,
    dirty_sequence: AtomicU64,
}

struct CachedExt4File(ext4_file);

// The cached descriptor is an immutable mount/inode template. All operations
// on copies still enter VfsSuperBlock::lock_fs before touching lwext4 state.
unsafe impl Send for CachedExt4File {}

struct PendingWriteRange {
    offset: usize,
    data: Vec<u8>,
}

struct PendingFileWrites {
    super_block: Option<Arc<VfsSuperBlock>>,
    file: Option<ext4_file>,
    ranges: Vec<PendingWriteRange>,
    buffered_bytes: usize,
    timestamp: u32,
}

unsafe impl Send for PendingFileWrites {}

impl PendingFileWrites {
    const fn new() -> Self {
        Self {
            super_block: None,
            file: None,
            ranges: Vec::new(),
            buffered_bytes: 0,
            timestamp: 0,
        }
    }

    fn push(
        &mut self,
        super_block: Arc<VfsSuperBlock>,
        file: ext4_file,
        offset: usize,
        data: &[u8],
        timestamp: u32,
    ) -> Option<bool> {
        if data.is_empty() {
            return Some(false);
        }
        if let Some(last) = self.ranges.last_mut() {
            if last.offset.checked_add(last.data.len()) == Some(offset) {
                last.data.try_reserve(data.len()).ok()?;
                last.data.extend_from_slice(data);
                self.buffered_bytes = self.buffered_bytes.saturating_add(data.len());
                self.timestamp = timestamp;
                return Some(self.buffered_bytes >= PENDING_WRITEBACK_PER_INODE_LIMIT);
            }
        }

        let mut stored = Vec::new();
        stored.try_reserve_exact(data.len()).ok()?;
        stored.extend_from_slice(data);
        self.ranges.try_reserve(1).ok()?;
        self.ranges.push(PendingWriteRange {
            offset,
            data: stored,
        });
        self.buffered_bytes = self.buffered_bytes.saturating_add(data.len());
        self.timestamp = timestamp;
        if self.super_block.is_none() {
            self.super_block = Some(super_block);
            self.file = Some(file);
        }
        Some(self.buffered_bytes >= PENDING_WRITEBACK_PER_INODE_LIMIT)
    }

    fn read_fully(&self, offset: usize, buf: &mut [u8], file_size: u64) -> Option<usize> {
        let file_size = usize::try_from(file_size).unwrap_or(usize::MAX);
        if offset >= file_size || buf.is_empty() {
            return Some(0);
        }
        let read_len = buf.len().min(file_size - offset);
        let end = offset.checked_add(read_len)?;

        let mut covered_until = offset;
        while covered_until < end {
            let mut next = covered_until;
            for range in &self.ranges {
                let range_end = range.offset.saturating_add(range.data.len());
                if range.offset <= covered_until && range_end > next {
                    next = range_end.min(end);
                }
            }
            if next == covered_until {
                return None;
            }
            covered_until = next;
        }

        // Ranges retain write order, so later overlapping writes win.
        for range in &self.ranges {
            let range_end = range.offset.saturating_add(range.data.len());
            let copy_start = range.offset.max(offset);
            let copy_end = range_end.min(end);
            if copy_start >= copy_end {
                continue;
            }
            let source_start = copy_start - range.offset;
            let destination_start = copy_start - offset;
            let len = copy_end - copy_start;
            buf[destination_start..destination_start + len]
                .copy_from_slice(&range.data[source_start..source_start + len]);
        }
        Some(read_len)
    }

    fn read_plan(&self, offset: usize, request_len: usize, file_size: u64) -> (usize, usize) {
        let file_size = usize::try_from(file_size).unwrap_or(usize::MAX);
        let read_len = request_len.min(file_size.saturating_sub(offset));
        let backing_size = self
            .file
            .as_ref()
            .map(|file| usize::try_from(file.fsize).unwrap_or(usize::MAX))
            .unwrap_or(0);
        (read_len, backing_size)
    }

    fn overlay_read(&self, offset: usize, buf: &mut [u8]) {
        let end = offset.saturating_add(buf.len());
        // Ranges retain write order, so later overlapping writes win.
        for range in &self.ranges {
            let range_end = range.offset.saturating_add(range.data.len());
            let copy_start = range.offset.max(offset);
            let copy_end = range_end.min(end);
            if copy_start >= copy_end {
                continue;
            }
            let source_start = copy_start - range.offset;
            let destination_start = copy_start - offset;
            let len = copy_end - copy_start;
            buf[destination_start..destination_start + len]
                .copy_from_slice(&range.data[source_start..source_start + len]);
        }
    }
}

impl InodeRuntimeSize {
    fn new() -> Self {
        Self {
            value: AtomicU64::new(INODE_RUNTIME_SIZE_UNKNOWN),
            mutation: SleepLock::new_named((), "VfsInodeMutation"),
            cached_file_handle: SleepLock::new_named(None, "VfsCachedFileHandle"),
            pending_writes: SleepLock::new_named(
                PendingFileWrites::new(),
                "VfsPendingWrites",
            ),
            pending_dirty: AtomicBool::new(false),
            dirty_sequence: AtomicU64::new(0),
        }
    }

    fn load(&self) -> Option<u64> {
        match self.value.load(Ordering::Acquire) {
            INODE_RUNTIME_SIZE_UNKNOWN => None,
            size => Some(size),
        }
    }

    fn initialize(&self, size: u64) -> u64 {
        match self.value.compare_exchange(
            INODE_RUNTIME_SIZE_UNKNOWN,
            size,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => size,
            Err(current) => current,
        }
    }

    fn store(&self, size: u64) {
        self.value.store(size, Ordering::Release);
    }
}

fn pending_writeback_limit() -> usize {
    // Pending ranges live in the kernel heap rather than in reclaimable user
    // frames. Keep their aggregate below one quarter of small heaps and cap it
    // on large machines so extra metadata capacity cannot turn into an
    // equally large write buffer.
    (crate::mm::heap_total_bytes() / 4)
        .max(PENDING_WRITEBACK_PER_INODE_LIMIT.saturating_mul(2))
        .min(PENDING_WRITEBACK_GLOBAL_MAX)
}

fn subtract_pending_write_bytes(bytes: usize) {
    if bytes == 0 {
        return;
    }
    let _ = PENDING_WRITE_BYTES.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_sub(bytes))
    });
}

fn add_pending_write_bytes(bytes: usize) -> usize {
    let mut result = 0usize;
    let _ = PENDING_WRITE_BYTES.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        result = current.saturating_add(bytes);
        Some(result)
    });
    result
}

fn discard_pending_write_state(state: &InodeRuntimeSize) {
    let discarded = {
        let mut slot = state.pending_writes.lock();
        core::mem::replace(&mut *slot, PendingFileWrites::new())
    };
    state.pending_dirty.store(false, Ordering::Release);
    state.dirty_sequence.store(0, Ordering::Release);
    state.value.store(INODE_RUNTIME_SIZE_UNKNOWN, Ordering::Release);
    *state.cached_file_handle.lock() = None;
    subtract_pending_write_bytes(discarded.buffered_bytes);
}

fn flush_pending_write_state(state: &InodeRuntimeSize) -> bool {
    if !state.pending_dirty.load(Ordering::Acquire) {
        return true;
    }
    let pending = {
        let mut slot = state.pending_writes.lock();
        if slot.ranges.is_empty() {
            state.pending_dirty.store(false, Ordering::Release);
            state.dirty_sequence.store(0, Ordering::Release);
            return true;
        }
        core::mem::replace(&mut *slot, PendingFileWrites::new())
    };

    let Some(super_block) = pending.super_block.as_ref() else {
        let mut slot = state.pending_writes.lock();
        *slot = pending;
        return false;
    };
    let Some(mut file) = pending.file else {
        let mut slot = state.pending_writes.lock();
        *slot = pending;
        return false;
    };

    let mut ok = true;
    'ranges: for range in &pending.ranges {
        let mut written = 0usize;
        while written < range.data.len() {
            let chunk_len = (range.data.len() - written).min(PENDING_WRITEBACK_CHUNK_SIZE);
            let mut rw_count = 0usize;
            let rc = {
                let _fs_lock = super_block.lock_fs();
                unsafe {
                    ext4_fseek(
                        &mut file,
                        range.offset.saturating_add(written) as i64,
                        SEEK_SET as u32,
                    );
                    ext4_fwrite_with_times(
                        &mut file,
                        range.data[written..written + chunk_len].as_ptr()
                            as *const core::ffi::c_void,
                        chunk_len,
                        &mut rw_count,
                        pending.timestamp,
                        pending.timestamp,
                    )
                }
            };
            if rc != EOK as i32 || rw_count == 0 || rw_count > chunk_len {
                ok = false;
                break 'ranges;
            }
            written += rw_count;
        }
    }

    if ok {
        state.pending_dirty.store(false, Ordering::Release);
        state.dirty_sequence.store(0, Ordering::Release);
        subtract_pending_write_bytes(pending.buffered_bytes);
        #[cfg(feature = "lock-stats")]
        {
            PENDING_CACHE_FLUSHES.fetch_add(1, Ordering::Relaxed);
            PENDING_CACHE_FLUSHED_BYTES.fetch_add(pending.buffered_bytes, Ordering::Relaxed);
        }
        true
    } else {
        let mut slot = state.pending_writes.lock();
        debug_assert!(slot.ranges.is_empty());
        *slot = pending;
        state.pending_dirty.store(true, Ordering::Release);
        false
    }
}

fn inode_runtime_key_from_parts(super_block: &VfsSuperBlock, inode: u32) -> (usize, u32) {
    (super_block as *const VfsSuperBlock as usize, inode)
}

fn inode_runtime_state_by_key(key: (usize, u32)) -> Arc<InodeRuntimeSize> {
    let mut sizes = INODE_RUNTIME_SIZES.exclusive_access();
    if let Some(state) = sizes.get(&key) {
        return state.clone();
    }
    if sizes.len() >= INODE_RUNTIME_SIZE_MAX_ENTRIES {
        let unused = sizes
            .iter()
            .find(|(_, state)| {
                Arc::strong_count(state) == 1
                    && !state.pending_dirty.load(Ordering::Acquire)
            })
            .map(|(&key, _)| key);
        if let Some(unused) = unused {
            sizes.remove(&unused);
        }
    }
    let state = Arc::new(InodeRuntimeSize::new());
    sizes.insert(key, state.clone());
    state
}

fn inode_runtime_state(super_block: &Arc<VfsSuperBlock>, inode: u32) -> Arc<InodeRuntimeSize> {
    inode_runtime_state_by_key((Arc::as_ptr(super_block) as usize, inode))
}

fn balance_pending_writeback() -> bool {
    if PENDING_WRITE_BYTES.load(Ordering::Acquire) <= pending_writeback_limit() {
        return true;
    }
    let mut candidates = {
        let sizes = INODE_RUNTIME_SIZES.exclusive_access();
        sizes
            .values()
            .filter(|state| state.pending_dirty.load(Ordering::Acquire))
            .cloned()
            .collect::<Vec<_>>()
    };
    candidates.sort_by_key(|state| state.dirty_sequence.load(Ordering::Acquire));

    for state in candidates {
        if PENDING_WRITE_BYTES.load(Ordering::Acquire) <= pending_writeback_limit() {
            break;
        }
        let _mutation = state.mutation.lock();
        if !state.pending_dirty.load(Ordering::Acquire) {
            continue;
        }
        if !flush_pending_write_state(&state) {
            return false;
        }
        #[cfg(feature = "lock-stats")]
        PENDING_CACHE_PRESSURE_FLUSHES.fetch_add(1, Ordering::Relaxed);
    }
    true
}

pub(crate) fn flush_and_reset_inode_runtime(super_block: usize, inode: u32) -> bool {
    let state = {
        let sizes = INODE_RUNTIME_SIZES.exclusive_access();
        sizes.get(&(super_block, inode)).cloned()
    };
    let Some(state) = state else {
        return true;
    };
    let _mutation = state.mutation.lock();
    if !flush_pending_write_state(&state) {
        return false;
    }
    discard_pending_write_state(&state);
    true
}

#[cfg(feature = "mount-watchdog")]
static MOUNT_WATCHDOG_STARTED_US: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "mount-watchdog")]
static MOUNT_WATCHDOG_STAGE: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "mount-watchdog")]
static MOUNT_WATCHDOG_REPORTED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "mount-watchdog")]
pub(crate) fn mount_watchdog_begin() {
    MOUNT_WATCHDOG_STAGE.store(1, Ordering::Release);
    MOUNT_WATCHDOG_REPORTED.store(false, Ordering::Release);
    MOUNT_WATCHDOG_STARTED_US.store(
        polyhal::time::Time::now().to_usec() as u64,
        Ordering::Release,
    );
}

#[cfg(feature = "mount-watchdog")]
pub(crate) fn mount_watchdog_stage(stage: usize) {
    if MOUNT_WATCHDOG_STARTED_US.load(Ordering::Acquire) != 0 {
        MOUNT_WATCHDOG_STAGE.store(stage, Ordering::Release);
    }
}

#[cfg(feature = "mount-watchdog")]
pub(crate) fn mount_watchdog_finish() {
    MOUNT_WATCHDOG_STAGE.store(0, Ordering::Release);
    MOUNT_WATCHDOG_STARTED_US.store(0, Ordering::Release);
}

#[cfg(feature = "mount-watchdog")]
pub(crate) fn mount_watchdog_poll() {
    let started = MOUNT_WATCHDOG_STARTED_US.load(Ordering::Acquire);
    if started == 0 {
        return;
    }
    let now = polyhal::time::Time::now().to_usec() as u64;
    if now.saturating_sub(started) < 3_000_000
        || MOUNT_WATCHDOG_REPORTED.swap(true, Ordering::AcqRel)
    {
        return;
    }
    crate::println!(
        "[mount-watchdog] stalled stage={} global-owner={}",
        MOUNT_WATCHDOG_STAGE.load(Ordering::Acquire),
        LWEXT4_GLOBAL.owner(),
    );
}

pub(crate) struct VfsFsGuard<'a> {
    // Fields drop in declaration order, reversing the acquisition sequence.
    // Release the global registry reference before waking per-volume waiters.
    _global: FsGlobalReadGuard<'a, ()>,
    local: FsGuard<'a, FsInner>,
}

impl Deref for VfsFsGuard<'_> {
    type Target = FsInner;

    fn deref(&self) -> &Self::Target {
        &self.local
    }
}

impl DerefMut for VfsFsGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.local
    }
}

fn record_path_owner(fs_path: &str, uid: u32, gid: u32) {
    let mut cache = PATH_OWNER_CACHE.exclusive_access();
    if !cache.contains_key(fs_path) && cache.len() >= PATH_OWNER_CACHE_MAX_ENTRIES {
        cache.clear();
    }
    cache.insert(String::from(fs_path), (uid, gid));
}

fn cached_path_owner(fs_path: &str) -> Option<(u32, u32)> {
    PATH_OWNER_CACHE.exclusive_access().get(fs_path).copied()
}

// ==================== DirEntry (local) ====================

pub struct DirEntry {
    pub inode: u32,
    name_bytes: Vec<u8>,
    pub de_type: u8,
}

impl DirEntry {
    pub fn get_name(&self) -> String {
        String::from_utf8_lossy(&self.name_bytes).into_owned()
    }
    pub fn get_de_type(&self) -> u8 {
        // ext4 stores its own compact directory-entry type values. Linux
        // exposes a different DT_* encoding through getdents64.
        match self.de_type as u32 {
            EXT4_DE_FIFO => 1,
            EXT4_DE_CHRDEV => 2,
            EXT4_DE_DIR => 4,
            EXT4_DE_BLKDEV => 6,
            EXT4_DE_REG_FILE => 8,
            EXT4_DE_SYMLINK => 10,
            EXT4_DE_SOCK => 12,
            _ => 0,
        }
    }
}

// ==================== InodeInfo ====================

#[derive(Clone, Copy)]
pub struct InodeInfo {
    pub inode_num: u32,
    pub is_dir: bool,
    pub is_link: bool,
    pub mode: u32,
    pub size: u64,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    pub blocks_count: u64,
    pub links_count: u32,
    pub uid: u32,
    pub gid: u32,
}

fn inode_info_from_raw(
    raw_ino: u32,
    raw_inode: &ext4_inode,
    cached_owner: Option<(u32, u32)>,
) -> InodeInfo {
    let mode = u16::from_le(raw_inode.mode) as u32;
    let file_type = mode & 0o170000;
    let is_dir = file_type == 0o040000;
    let is_link = file_type == 0o120000;
    let size_lo = u32::from_le(raw_inode.size_lo) as u64;
    let size = if file_type == 0o100000 {
        size_lo | ((u32::from_le(raw_inode.size_hi) as u64) << 32)
    } else {
        size_lo
    };
    let (uid, gid) = cached_owner.unwrap_or_else(|| {
        (
            u16::from_le(raw_inode.uid) as u32,
            u16::from_le(raw_inode.gid) as u32,
        )
    });

    InodeInfo {
        inode_num: raw_ino,
        is_dir,
        is_link,
        mode,
        size,
        atime: u32::from_le(raw_inode.access_time),
        mtime: u32::from_le(raw_inode.modification_time),
        ctime: u32::from_le(raw_inode.change_inode_time),
        blocks_count: u32::from_le(raw_inode.blocks_count_lo) as u64,
        links_count: u16::from_le(raw_inode.links_count) as u32,
        uid,
        gid,
    }
}

// ==================== VfsSuperBlock ====================

pub enum FsInner {
    Ext4(Ext4Wrapper),
}

pub struct VfsSuperBlock {
    pub fs_type: &'static str,
    lwext4_mount_path: String,
    /// Ext4/lwext4 is not reentrant.  The first SMP revision serializes each
    /// mounted superblock explicitly, rather than relying on a global
    /// single-core cell.
    pub fs_data: FsLock<FsInner>,
}

impl VfsSuperBlock {
    fn commit_inode_runtime_size(&self, inode: u32, size: u64) {
        inode_runtime_state_by_key(inode_runtime_key_from_parts(self, inode)).store(size);
    }

    /// Serialize direct lwext4 operations. lwext4 keeps a process-global
    /// mount registry and lazily initialized checksum tables in addition to
    /// each volume's journal and block cache.
    #[track_caller]
    pub(crate) fn lock_fs(&self) -> VfsFsGuard<'_> {
        let local = self.fs_data.exclusive_access();
        let global = LWEXT4_GLOBAL.read();
        VfsFsGuard {
            _global: global,
            local,
        }
    }

    pub(crate) fn fs_has_waiters(&self) -> bool {
        self.fs_data.has_waiters()
    }

    pub fn new_ext4(
        blk_device: Arc<dyn crate::drivers::BlockDevice>,
        fs_type: &'static str,
    ) -> Option<Arc<Self>> {
        Self::new_ext4_at(blk_device, fs_type, "/mp/")
    }

    pub fn new_ext4_at(
        blk_device: Arc<dyn crate::drivers::BlockDevice>,
        fs_type: &'static str,
        mount_point: &str,
    ) -> Option<Arc<Self>> {
        const FALLBACK_DISK_SIZE: u64 = 512 * 1024 * 1024;
        #[cfg(feature = "mount-watchdog")]
        mount_watchdog_stage(6);
        #[cfg(feature = "mount-trace")]
        crate::println!("[mount-trace] capacity-begin point={}", mount_point);
        let total_size = blk_device.capacity_bytes().unwrap_or(FALLBACK_DISK_SIZE);
        #[cfg(feature = "mount-watchdog")]
        mount_watchdog_stage(7);
        #[cfg(feature = "mount-trace")]
        crate::println!(
            "[mount-trace] capacity-done point={} size={}",
            mount_point,
            total_size
        );
        let stream = BlockDeviceStream::new(blk_device, total_size);
        #[cfg(feature = "mount-watchdog")]
        mount_watchdog_stage(8);
        #[cfg(feature = "mount-trace")]
        crate::println!("[mount-trace] stream-ready point={}", mount_point);
        // lwext4 keeps mount/cache state globally, not per Ext4BlockWrapper.
        // Mount creation must use the same serialization as normal filesystem
        // operations, otherwise a second disk mount can race a rootfs access
        // on another hart.
        #[cfg(feature = "smp-diagnostics")]
        crate::println!(
            "[mount-diag] create-begin point={} size={}",
            mount_point,
            total_size
        );
        let ext4 = {
            #[cfg(feature = "mount-watchdog")]
            mount_watchdog_stage(9);
            #[cfg(feature = "mount-trace")]
            crate::println!("[mount-trace] lwext4-lock-begin point={}", mount_point);
            let _global = LWEXT4_GLOBAL.exclusive_access();
            #[cfg(feature = "mount-watchdog")]
            mount_watchdog_stage(10);
            #[cfg(feature = "mount-trace")]
            crate::println!("[mount-trace] lwext4-lock-done point={}", mount_point);
            #[cfg(feature = "smp-diagnostics")]
            crate::println!("[mount-diag] global-acquired point={}", mount_point);
            #[cfg(feature = "mount-trace")]
            crate::println!("[mount-trace] lwext4-create-begin point={}", mount_point);
            Ext4BlockWrapper::<BlockDeviceStreamOp>::new_with_name(stream, "ext4_fs", mount_point)
                .ok()?
        };
        #[cfg(feature = "mount-watchdog")]
        mount_watchdog_stage(11);
        #[cfg(feature = "mount-trace")]
        crate::println!("[mount-trace] lwext4-create-done point={}", mount_point);
        #[cfg(feature = "smp-diagnostics")]
        crate::println!("[mount-diag] create-done point={}", mount_point);
        Some(Arc::new(VfsSuperBlock {
            fs_type,
            lwext4_mount_path: String::from(mount_point),
            fs_data: FsLock::new_named(FsInner::Ext4(Ext4Wrapper(ext4)), "VfsFsData"),
        }))
    }

    #[track_caller]
    fn with_fs<R>(&self, f: impl FnOnce(&Ext4Wrapper) -> R) -> R {
        let data = self.lock_fs();
        match &*data {
            FsInner::Ext4(wrapper) => f(wrapper),
        }
    }

    pub fn force_umount(&self) {
        let mut data = self.fs_data.exclusive_access();
        let _global = LWEXT4_GLOBAL.exclusive_access();
        if let FsInner::Ext4(ref mut wrapper) = *data {
            unsafe {
                wrapper.0.lwext4_umount().ok();
            }
        }
    }

    // ---------- path-based ext4 operations ----------

    pub fn dir_get_entries(&self, fs_path: &str) -> Vec<DirEntry> {
        const MAX_DIR_ENTRIES: usize = 4096;
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let mut d: ext4_dir = unsafe { core::mem::zeroed() };
            let mut entries = Vec::new();

            unsafe {
                let rc = ext4_dir_open(&mut d, c_path.as_ptr() as *const c_char);
                if rc != EOK as i32 {
                    return entries;
                }
                loop {
                    if entries.len() >= MAX_DIR_ENTRIES {
                        break;
                    }
                    let de = ext4_dir_entry_next(&mut d);
                    if de.is_null() {
                        break;
                    }
                    let dentry = &*de;
                    let name_len = (dentry.name_length as usize).min(dentry.name.len());
                    let name_bytes = dentry.name[..name_len].to_vec();
                    entries.push(DirEntry {
                        inode: dentry.inode,
                        name_bytes,
                        de_type: dentry.inode_type,
                    });
                }
                ext4_dir_close(&mut d);
            }
            entries
        })
    }

    fn get_inode_info_result(&self, fs_path: &str) -> Result<InodeInfo, i32> {
        #[cfg(feature = "lock-stats")]
        VFS_INODE_INFO_QUERIES.fetch_add(1, Ordering::Relaxed);
        // The owner cache is VFS metadata. Take this snapshot before
        // `with_fs` acquires the superblock FsLock, preserving the documented
        // `VfsLock -> FsLock` order.
        let cached_owner = cached_path_owner(fs_path);
        let result = self.with_fs(|_fs| {
            let c_path = CString::new(fs_path).map_err(|_| EINVAL as i32)?;
            let mut raw_ino: u32 = 0;
            let mut raw_inode: ext4_inode = unsafe { core::mem::zeroed() };
            let raw_rc = unsafe {
                ext4_raw_inode_fill(
                    c_path.as_ptr() as *const c_char,
                    &mut raw_ino,
                    &mut raw_inode,
                )
            };
            #[cfg(feature = "buildstorm-diagnostics")]
            if fs_path.contains("/target/debug/") && raw_rc != EOK as i32 && raw_rc != ENOENT as i32
            {
                crate::println!(
                    "[buildstorm-fs] inode-query-failed raw_rc={} path={}",
                    raw_rc,
                    fs_path,
                );
            }
            if raw_rc != EOK as i32 {
                return Err(raw_rc);
            }

            // ext4_raw_inode_fill resolves the path once and copies a coherent
            // on-disk inode snapshot. Decoding that snapshot avoids repeating
            // the same path walk for mode, owner, three timestamps and size.
            Ok(inode_info_from_raw(raw_ino, &raw_inode, cached_owner))
        });
        #[cfg(feature = "lock-stats")]
        match &result {
            Ok(_) => {
                VFS_INODE_INFO_FOUND.fetch_add(1, Ordering::Relaxed);
            }
            Err(rc) if *rc == ENOENT as i32 => {
                VFS_INODE_INFO_ABSENT.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                VFS_INODE_INFO_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
        }
        result
    }

    pub fn get_inode_info(&self, fs_path: &str) -> Option<InodeInfo> {
        self.get_inode_info_result(fs_path).ok()
    }

    pub fn read_at(&self, fs_path: &str, offset: usize, buf: &mut [u8]) -> usize {
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return 0,
            };
            let c_path = c_path.into_raw();
            let flags = CString::new("rb")
                .ok()
                .unwrap_or_else(|| CString::new("").unwrap());
            let flags_ptr = flags.into_raw();

            let mut file: ext4_file = unsafe { core::mem::zeroed() };
            let rc = unsafe { ext4_fopen(&mut file, c_path, flags_ptr) };
            unsafe {
                drop(CString::from_raw(c_path));
                drop(CString::from_raw(flags_ptr));
            }
            if rc != EOK as i32 {
                #[cfg(feature = "buildstorm-diagnostics")]
                if fs_path.contains("/target/debug/") {
                    crate::println!(
                        "[buildstorm-fs] fallback-write-open-failed rc={} off={} len={} path={}",
                        rc,
                        offset,
                        buf.len(),
                        fs_path,
                    );
                }
                return 0;
            }

            let file_size = unsafe { ext4_fsize(&mut file) as usize };
            if offset >= file_size {
                unsafe {
                    ext4_fclose(&mut file);
                }
                return 0;
            }
            let max_read = core::cmp::min(buf.len(), file_size - offset);

            unsafe {
                ext4_fseek(&mut file, offset as i64, SEEK_SET as u32);
            }

            let mut total = 0usize;
            while total < max_read {
                let chunk_len = max_read - total;
                let mut rw_count: usize = 0;
                #[cfg(feature = "smp-diagnostics")]
                let watched_before = crate::mm::watched_frame_is_intact();
                let read_rc = unsafe {
                    ext4_fread(
                        &mut file,
                        buf[total..total + chunk_len].as_mut_ptr() as *mut core::ffi::c_void,
                        chunk_len,
                        &mut rw_count,
                    )
                };
                #[cfg(feature = "smp-diagnostics")]
                if watched_before && !crate::mm::watched_frame_is_intact() {
                    crate::println!(
                        "[frame-diag] watched page changed during ext4_fread path={} off={:#x} requested={} returned={} rc={}",
                        fs_path,
                        offset + total,
                        chunk_len,
                        rw_count,
                        read_rc,
                    );
                }
                // The C API reports its transfer size through an out pointer.
                // Never let an invalid value advance Rust's slice cursor past
                // the caller-provided buffer.
                if read_rc != EOK as i32 || rw_count == 0 || rw_count > chunk_len {
                    break;
                }
                total += rw_count;
                if rw_count < chunk_len {
                    break;
                }
            }
            unsafe {
                ext4_fclose(&mut file);
            }

            total
        })
    }

    pub fn write_at(&self, fs_path: &str, offset: usize, buf: &[u8], timestamp: u32) -> usize {
        self.write_at_raw(fs_path, offset, buf, timestamp)
    }

    fn write_at_raw(&self, fs_path: &str, offset: usize, buf: &[u8], timestamp: u32) -> usize {
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return 0,
            };
            let c_path = c_path.into_raw();
            let flags = CString::new("r+")
                .ok()
                .unwrap_or_else(|| CString::new("").unwrap());
            let flags_ptr = flags.into_raw();

            let mut file: ext4_file = unsafe { core::mem::zeroed() };
            let rc = unsafe { ext4_fopen(&mut file, c_path, flags_ptr) };
            unsafe {
                drop(CString::from_raw(c_path));
                drop(CString::from_raw(flags_ptr));
            }
            if rc != EOK as i32 {
                return 0;
            }

            let old_size = unsafe { ext4_fsize(&mut file) };
            if offset as u64 > old_size
                && unsafe { ext4_ftruncate(&mut file, offset as u64) } != EOK as i32
            {
                unsafe {
                    ext4_fclose(&mut file);
                }
                return 0;
            }
            unsafe {
                ext4_fseek(&mut file, offset as i64, SEEK_SET as u32);
            }

            let mut rw_count: usize = 0;
            let write_rc = unsafe {
                ext4_fwrite_with_times(
                    &mut file,
                    buf.as_ptr() as *const core::ffi::c_void,
                    buf.len(),
                    &mut rw_count,
                    timestamp,
                    timestamp,
                )
            };
            let new_size = unsafe { ext4_fsize(&mut file) };
            if write_rc == EOK as i32 {
                self.commit_inode_runtime_size(file.inode, new_size);
            }
            unsafe {
                ext4_fclose(&mut file);
            }

            if write_rc != EOK as i32 {
                #[cfg(feature = "buildstorm-diagnostics")]
                if fs_path.contains("/target/debug/") {
                    crate::println!(
                        "[buildstorm-fs] fallback-write-failed rc={} off={} len={} written={} path={}",
                        write_rc,
                        offset,
                        buf.len(),
                        rw_count,
                        fs_path,
                    );
                }
                return 0;
            }
            rw_count
        })
    }

    fn create_file(
        &self,
        parent_path: &str,
        parent_inode: u32,
        name: &str,
        is_dir: bool,
        mode: u32,
        uid: u32,
        gid: u32,
        keep_file_open: bool,
    ) -> Option<(InodeInfo, Option<ext4_file>)> {
        let full_path = if parent_path == "/" {
            format!("/{}", name)
        } else if parent_path.ends_with('/') {
            format!("{}{}", parent_path, name)
        } else {
            format!("{}/{}", parent_path, name)
        };
        // A newly created Linux inode has meaningful atime/mtime/ctime even
        // before its first data write.  This matters for build tools that
        // compare a freshly emitted target with its inputs.  The low-level
        // create-at helper initializes a blank lwext4 inode, so publish the
        // kernel realtime timestamp explicitly instead of exposing epoch 0.
        let now = crate::time::realtime_sec().clamp(0, u32::MAX as i64) as u32;

        let (created, _raw_rc) = self.with_fs(|_fs| {
            let c_mount = match CString::new(self.lwext4_mount_path.as_bytes()) {
                Ok(path) => path,
                Err(_) => return (None, -(EINVAL as i32)),
            };
            let c_name = match CString::new(name.as_bytes()) {
                Ok(name) => name,
                Err(_) => return (None, -(EINVAL as i32)),
            };
            let mut file: ext4_file = unsafe { core::mem::zeroed() };
            let create_rc = unsafe {
                ext4_create_at_with_attrs(
                    &mut file,
                    c_mount.as_ptr() as *const c_char,
                    parent_inode,
                    c_name.as_ptr() as *const c_char,
                    is_dir,
                    mode,
                    uid,
                    gid,
                    now,
                )
            };
            if create_rc != EOK as i32 {
                return (None, create_rc);
            }

            let size = unsafe { ext4_fsize(&mut file) };
            let inode_num = file.inode;
            if !is_dir {
                self.commit_inode_runtime_size(inode_num, size);
            }
            let file_handle = if !is_dir && keep_file_open {
                Some(file)
            } else {
                unsafe {
                    ext4_fclose(&mut file);
                }
                None
            };
            (
                Some((
                    InodeInfo {
                        inode_num,
                        is_dir,
                        is_link: false,
                        mode,
                        size,
                        atime: now,
                        mtime: now,
                        ctime: now,
                        blocks_count: 0,
                        links_count: if is_dir { 2 } else { 1 },
                        uid,
                        gid,
                    },
                    file_handle,
                )),
                EOK as i32,
            )
        });
        if created.is_some() {
            crate::fs::inode::update_path_cached_times(
                &full_path,
                Some(now as i64),
                Some(now as i64),
                Some(now as i64),
            );
            record_path_owner(&full_path, uid, gid);
        }
        #[cfg(feature = "buildstorm-diagnostics")]
        if created.is_none() && full_path.contains("/target/debug/") {
            crate::println!(
                "[buildstorm-fs] create-failed dir={} rc={} path={}",
                is_dir,
                _raw_rc,
                full_path,
            );
        }
        created
    }

    pub fn truncate_file(&self, fs_path: &str, size: u64) -> bool {
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return false,
            };
            let c_path = c_path.into_raw();
            let flags = CString::new("r+")
                .ok()
                .unwrap_or_else(|| CString::new("").unwrap());
            let flags_ptr = flags.into_raw();

            let mut file: ext4_file = unsafe { core::mem::zeroed() };
            let rc = unsafe { ext4_fopen(&mut file, c_path, flags_ptr) };
            unsafe {
                drop(CString::from_raw(c_path));
                drop(CString::from_raw(flags_ptr));
            }
            if rc != EOK as i32 {
                #[cfg(feature = "buildstorm-diagnostics")]
                if fs_path.contains("/target/debug/") {
                    crate::println!(
                        "[buildstorm-fs] truncate-open-failed rc={} size={} path={}",
                        rc,
                        size,
                        fs_path,
                    );
                }
                return false;
            }

            #[cfg(feature = "buildstorm-diagnostics")]
            let old_size = unsafe { ext4_fsize(&mut file) };
            let inode = file.inode;
            let operation_rc = unsafe { ext4_ftruncate(&mut file, size) };
            let ok = operation_rc == EOK as i32;
            if ok {
                self.commit_inode_runtime_size(inode, size);
            }
            unsafe {
                ext4_fclose(&mut file);
            }
            #[cfg(feature = "buildstorm-diagnostics")]
            if !ok && fs_path.contains("/target/debug/") {
                crate::println!(
                    "[buildstorm-fs] truncate-write-failed rc={} inode={} old_size={} size={} path={}",
                    operation_rc,
                    inode,
                    old_size,
                    size,
                    fs_path,
                );
            }
            ok
        })
    }

    pub fn unlink_file(&self, fs_path: &str) -> bool {
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return false,
            };
            unsafe { ext4_fremove(c_path.as_ptr() as *const c_char) == EOK as i32 }
        })
    }

    pub fn unlink_dir(&self, fs_path: &str) -> bool {
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return false,
            };
            unsafe { ext4_dir_rm(c_path.as_ptr() as *const c_char) == EOK as i32 }
        })
    }

    pub fn unlink_child(
        &self,
        parent_inode: u32,
        child_inode: u32,
        name: &str,
        is_dir: bool,
    ) -> i32 {
        self.with_fs(|_fs| {
            let c_mount = match CString::new(self.lwext4_mount_path.as_bytes()) {
                Ok(path) => path,
                Err(_) => return EINVAL as i32,
            };
            let c_name = match CString::new(name.as_bytes()) {
                Ok(name) => name,
                Err(_) => return EINVAL as i32,
            };
            unsafe {
                ext4_remove_at(
                    c_mount.as_ptr() as *const c_char,
                    parent_inode,
                    child_inode,
                    c_name.as_ptr() as *const c_char,
                    is_dir,
                )
            }
        })
    }

    pub fn rename_child(
        &self,
        source_parent_inode: u32,
        source_inode: u32,
        source_name: &str,
        target_parent_inode: u32,
        target_name: &str,
        replaced_name: &str,
        flags: u32,
    ) -> (i32, u32, bool) {
        self.with_fs(|_fs| {
            let c_mount = match CString::new(self.lwext4_mount_path.as_bytes()) {
                Ok(path) => path,
                Err(_) => return (EINVAL as i32, 0, false),
            };
            let c_source_name = match CString::new(source_name.as_bytes()) {
                Ok(name) => name,
                Err(_) => return (EINVAL as i32, 0, false),
            };
            let c_target_name = match CString::new(target_name.as_bytes()) {
                Ok(name) => name,
                Err(_) => return (EINVAL as i32, 0, false),
            };
            let c_replaced_name = match CString::new(replaced_name.as_bytes()) {
                Ok(name) => name,
                Err(_) => return (EINVAL as i32, 0, false),
            };
            let mut replaced_inode = 0u32;
            let mut replaced_is_dir = false;
            let rc = unsafe {
                ext4_rename_at2(
                    c_mount.as_ptr() as *const c_char,
                    source_parent_inode,
                    source_inode,
                    c_source_name.as_ptr() as *const c_char,
                    target_parent_inode,
                    c_target_name.as_ptr() as *const c_char,
                    c_replaced_name.as_ptr() as *const c_char,
                    flags,
                    &mut replaced_inode,
                    &mut replaced_is_dir,
                )
            };
            (rc, replaced_inode, replaced_is_dir)
        })
    }

    pub fn set_atime(&self, fs_path: &str, atime: u32) {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe {
                    ext4_atime_set(c_path.as_ptr() as *const c_char, atime);
                }
            }
        })
    }

    pub fn set_mtime(&self, fs_path: &str, mtime: u32) {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe {
                    ext4_mtime_set(c_path.as_ptr() as *const c_char, mtime);
                }
            }
        })
    }

    pub fn set_ctime(&self, fs_path: &str, ctime: u32) {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe {
                    ext4_ctime_set(c_path.as_ptr() as *const c_char, ctime);
                }
            }
        })
    }

    pub fn set_mode(&self, fs_path: &str, mode: u32) -> i32 {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                let r = unsafe { ext4_mode_set(c_path.as_ptr() as *const c_char, mode) };
                r
            } else {
                -1
            }
        })
    }

    pub fn set_owner(&self, fs_path: &str, uid: u32, gid: u32) -> i32 {
        let result = self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe { ext4_owner_set(c_path.as_ptr() as *const c_char, uid, gid) }
            } else {
                -1
            }
        });
        // The owner cache is VFS metadata (rank 8), so update it only after
        // with_fs has released the lwext4 global and per-volume locks.
        if result == EOK as i32 {
            record_path_owner(fs_path, uid, gid);
        }
        result
    }

    pub fn fsync(&self, fs_path: &str) -> i32 {
        self.with_fs(|wrapper| {
            if let Ok(c_path) = CString::new(fs_path) {
                let rc = unsafe { ext4_cache_flush(c_path.as_ptr() as *const c_char) };
                if rc == EOK as i32 {
                    wrapper.0.flush_device().map(|_| 0).unwrap_or(-1)
                } else {
                    rc
                }
            } else {
                -1
            }
        })
    }

    pub fn sync_all(&self) -> i32 {
        self.with_fs(|wrapper| {
            let mount_path = CString::new(self.lwext4_mount_path.as_str()).unwrap();
            let rc = unsafe { ext4_cache_flush(mount_path.as_ptr() as *const c_char) };
            if rc == EOK as i32 {
                wrapper.0.flush_device().map(|_| 0).unwrap_or(-1)
            } else {
                rc
            }
        })
    }

    pub fn get_fs_stats(&self) -> ext4_mount_stats {
        self.with_fs(|wrapper| wrapper.0.get_mount_stats())
    }
}

// ==================== Dentry ====================

pub struct Dentry {
    pub name: String,
    pub path: String,
    pub fs_path: String,
    pub inode_num: u32,
    pub is_dir: bool,
    pub is_link: bool,
    pub parent: Option<Weak<Dentry>>,
    pub super_block: Arc<VfsSuperBlock>,
    runtime_size: Option<Arc<InodeRuntimeSize>>,
    /// Large build directories need logarithmic lookup and insertion. This
    /// sleeping lock may allocate B-tree nodes, but is never held across Ext4.
    pub children: SleepLock<Option<BTreeMap<String, Arc<Dentry>>>>,
    /// Serializes namespace mutations in this directory. The epoch below is
    /// only a lockless cache sequence; slow ext4 operations must not make
    /// competing creators spin for their full duration.
    child_mutation: SleepLock<()>,
    /// Even values describe a stable cache generation. Odd values mean a
    /// directory mutation is updating the child cache.
    child_cache_epoch: AtomicU64,
    /// Parent namespace generation at the last successful path validation.
    /// A stable generation lets openat(dirfd, ...) reuse the directory inode
    /// without re-walking the same path for every child operation.
    parent_validation_epoch: AtomicU64,
    /// ENOENT results remain valid until the next mutation in this directory.
    negative_children: SleepLock<Option<BTreeSet<String>>>,
    /// This is only an in-memory metadata cache.  Ext4 lookup happens after
    /// dropping the lock, then the result is stored in a second short phase.
    inode_info: DentryLock<Option<InodeInfo>>,
}

impl Dentry {
    fn new_root(sb: Arc<VfsSuperBlock>) -> Arc<Self> {
        Arc::new(Dentry {
            name: String::from("/"),
            path: String::from("/"),
            fs_path: String::from(ROOT_LWEXT4_MOUNT),
            inode_num: 2,
            is_dir: true,
            is_link: false,
            parent: None,
            super_block: sb,
            runtime_size: None,
            children: SleepLock::new_named(None, "DentryChildren"),
            child_mutation: SleepLock::new_named((), "DentryChildMutation"),
            child_cache_epoch: AtomicU64::new(0),
            parent_validation_epoch: AtomicU64::new(DENTRY_PARENT_EPOCH_UNKNOWN),
            negative_children: SleepLock::new_named(None, "DentryNegativeChildren"),
            inode_info: DentryLock::new_named(None, "DentryInodeInfo"),
        })
    }

    fn new_mount_root(sb: Arc<VfsSuperBlock>, mnt_path: String, lwext4_mount: &str) -> Arc<Self> {
        Arc::new(Dentry {
            name: String::from("/"),
            path: mnt_path,
            fs_path: String::from(lwext4_mount),
            inode_num: 2,
            is_dir: true,
            is_link: false,
            parent: None,
            super_block: sb,
            runtime_size: None,
            children: SleepLock::new_named(None, "DentryChildren"),
            child_mutation: SleepLock::new_named((), "DentryChildMutation"),
            child_cache_epoch: AtomicU64::new(0),
            parent_validation_epoch: AtomicU64::new(DENTRY_PARENT_EPOCH_UNKNOWN),
            negative_children: SleepLock::new_named(None, "DentryNegativeChildren"),
            inode_info: DentryLock::new_named(None, "DentryInodeInfo"),
        })
    }

    fn new_child(
        parent: &Arc<Dentry>,
        name: &str,
        inode_num: u32,
        is_dir: bool,
        is_link: bool,
    ) -> Arc<Self> {
        let path = if parent.path == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", parent.path, name)
        };
        let fs_path = if parent.fs_path == "/" {
            format!("/{}", name)
        } else if parent.fs_path.ends_with('/') {
            format!("{}{}", parent.fs_path, name)
        } else {
            format!("{}/{}", parent.fs_path, name)
        };
        let runtime_size =
            (!is_dir && !is_link).then(|| inode_runtime_state(&parent.super_block, inode_num));
        let parent_validation_epoch = parent
            .child_cache_snapshot_epoch()
            .unwrap_or(DENTRY_PARENT_EPOCH_UNKNOWN);
        Arc::new(Dentry {
            name: String::from(name),
            path,
            fs_path,
            inode_num,
            is_dir,
            is_link,
            parent: Some(Arc::downgrade(parent)),
            super_block: parent.super_block.clone(),
            runtime_size,
            children: SleepLock::new_named(None, "DentryChildren"),
            child_mutation: SleepLock::new_named((), "DentryChildMutation"),
            child_cache_epoch: AtomicU64::new(0),
            parent_validation_epoch: AtomicU64::new(parent_validation_epoch),
            negative_children: SleepLock::new_named(None, "DentryNegativeChildren"),
            inode_info: DentryLock::new_named(None, "DentryInodeInfo"),
        })
    }

    pub fn inode_info(&self) -> Option<InodeInfo> {
        let cached = { *self.inode_info.exclusive_access() };
        let mut info = if let Some(info) = cached {
            info
        } else {
            let info = self.super_block.get_inode_info(&self.fs_path)?;
            *self.inode_info.exclusive_access() = Some(info);
            info
        };
        if !info.is_dir && !info.is_link {
            if let Some(runtime_size) = self.runtime_size.as_ref() {
                let size = runtime_size
                    .load()
                    .unwrap_or_else(|| runtime_size.initialize(info.size));
                info.size = size;
                info.blocks_count = (size + 511) / 512;
            }
        }
        Some(info)
    }

    pub fn refresh_inode_info(&self) -> Option<InodeInfo> {
        let info = self.super_block.get_inode_info(&self.fs_path)?;
        *self.inode_info.exclusive_access() = Some(info);
        Some(info)
    }

    pub fn set_cached_inode_info(&self, info: InodeInfo) {
        *self.inode_info.exclusive_access() = Some(info);
    }

    pub(crate) fn cached_inode_info_snapshot(&self) -> Option<InodeInfo> {
        *self.inode_info.exclusive_access()
    }

    pub fn invalidate_inode_info(&self) {
        *self.inode_info.exclusive_access() = None;
    }

    pub fn update_local_cached_size(&self, size: u64) {
        if let Some(ref mut info) = *self.inode_info.exclusive_access() {
            info.size = size;
            info.blocks_count = (size + 511) / 512;
        }
    }

    pub(crate) fn commit_cached_size(&self, size: u64) {
        self.update_local_cached_size(size);
        if let Some(runtime_size) = self.runtime_size.as_ref() {
            runtime_size.store(size);
        }
    }

    pub(crate) fn runtime_file_size(&self) -> Option<u64> {
        self.runtime_size
            .as_ref()
            .and_then(|runtime_size| runtime_size.load())
    }

    pub(crate) fn cached_file_handle(&self) -> Option<ext4_file> {
        self.runtime_size
            .as_ref()
            .and_then(|runtime_size| {
                runtime_size
                    .cached_file_handle
                    .lock()
                    .as_ref()
                    .map(|cached| cached.0)
            })
    }

    pub(crate) fn cache_file_handle(&self, mut file: ext4_file) {
        let Some(runtime_size) = self.runtime_size.as_ref() else {
            return;
        };
        file.fpos = 0;
        file.fsize = runtime_size
            .load()
            .unwrap_or_else(|| runtime_size.initialize(file.fsize));
        *runtime_size.cached_file_handle.lock() = Some(CachedExt4File(file));
    }

    pub(crate) fn lock_file_mutation(&self) -> Option<SleepGuard<'_, ()>> {
        self.runtime_size
            .as_ref()
            .map(|runtime_size| runtime_size.mutation.lock())
    }

    pub(crate) fn child_cache_snapshot_epoch(&self) -> Option<u64> {
        let epoch = self.child_cache_epoch.load(Ordering::Acquire);
        (epoch & 1 == 0).then_some(epoch)
    }

    pub(crate) fn parent_epoch_is_current(&self) -> bool {
        let Some(parent) = self.parent.as_ref() else {
            return true;
        };
        let Some(parent) = parent.upgrade() else {
            return false;
        };
        let expected = self.parent_validation_epoch.load(Ordering::Acquire);
        let current = parent.child_cache_epoch.load(Ordering::Acquire);
        let current = expected != DENTRY_PARENT_EPOCH_UNKNOWN
            && current & 1 == 0
            && current == expected;
        #[cfg(feature = "lock-stats")]
        if current {
            VFS_DIRFD_EPOCH_HITS.fetch_add(1, Ordering::Relaxed);
        }
        current
    }

    pub(crate) fn parent_epoch_for_revalidation(&self) -> Option<u64> {
        #[cfg(feature = "lock-stats")]
        VFS_DIRFD_REVALIDATIONS.fetch_add(1, Ordering::Relaxed);
        self.parent
            .as_ref()
            .and_then(Weak::upgrade)
            .and_then(|parent| parent.child_cache_snapshot_epoch())
    }

    pub(crate) fn mark_parent_epoch_validated(&self, epoch: u64) {
        let Some(parent) = self.parent.as_ref().and_then(Weak::upgrade) else {
            return;
        };
        if parent.child_cache_epoch.load(Ordering::Acquire) == epoch && epoch & 1 == 0 {
            self.parent_validation_epoch.store(epoch, Ordering::Release);
        }
    }

    pub(crate) fn buffer_pending_write(
        &self,
        file: ext4_file,
        offset: usize,
        data: &[u8],
        timestamp: u32,
    ) -> Option<(bool, bool)> {
        let runtime_size = self.runtime_size.as_ref()?;
        let flush_inode = runtime_size.pending_writes.lock().push(
            self.super_block.clone(),
            file,
            offset,
            data,
            timestamp,
        )?;
        if !runtime_size.pending_dirty.swap(true, Ordering::AcqRel) {
            let sequence = NEXT_PENDING_DIRTY_SEQUENCE.fetch_add(1, Ordering::AcqRel);
            runtime_size
                .dirty_sequence
                .store(sequence.max(1), Ordering::Release);
        }
        let pending_bytes = add_pending_write_bytes(data.len());
        Some((flush_inode, pending_bytes > pending_writeback_limit()))
    }

    pub(crate) fn read_pending_fully(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        let runtime_size = self.runtime_size.as_ref()?;
        if !runtime_size.pending_dirty.load(Ordering::Acquire) {
            return None;
        }
        let file_size = runtime_size.load()?;
        let read = runtime_size
            .pending_writes
            .lock()
            .read_fully(offset, buf, file_size)?;
        #[cfg(feature = "lock-stats")]
        {
            PENDING_CACHE_READS.fetch_add(1, Ordering::Relaxed);
            PENDING_CACHE_READ_BYTES.fetch_add(read, Ordering::Relaxed);
        }
        Some(read)
    }

    /// Return the logical read length and the size currently present on disk.
    /// Callers hold the inode mutation lock until `overlay_pending_read` returns,
    /// so writeback and newer writes cannot change the two-layer snapshot.
    pub(crate) fn pending_read_plan(
        &self,
        offset: usize,
        request_len: usize,
    ) -> Option<(usize, usize)> {
        let runtime_size = self.runtime_size.as_ref()?;
        if !runtime_size.pending_dirty.load(Ordering::Acquire) {
            return None;
        }
        let file_size = runtime_size.load()?;
        let pending = runtime_size.pending_writes.lock();
        if pending.ranges.is_empty() {
            return None;
        }
        Some(pending.read_plan(offset, request_len, file_size))
    }

    pub(crate) fn overlay_pending_read(&self, offset: usize, buf: &mut [u8]) -> bool {
        let Some(runtime_size) = self.runtime_size.as_ref() else {
            return false;
        };
        if !runtime_size.pending_dirty.load(Ordering::Acquire) {
            return false;
        }
        let pending = runtime_size.pending_writes.lock();
        if pending.ranges.is_empty() {
            return false;
        }
        pending.overlay_read(offset, buf);
        #[cfg(feature = "lock-stats")]
        {
            PENDING_CACHE_OVERLAY_READS.fetch_add(1, Ordering::Relaxed);
            PENDING_CACHE_OVERLAY_BYTES.fetch_add(buf.len(), Ordering::Relaxed);
        }
        true
    }

    pub(crate) fn has_pending_writes(&self) -> bool {
        self.runtime_size
            .as_ref()
            .is_some_and(|state| state.pending_dirty.load(Ordering::Acquire))
    }

    pub(crate) fn record_pending_close(&self) {
        #[cfg(feature = "lock-stats")]
        if self.has_pending_writes() {
            PENDING_CACHE_DEFERRED_CLOSES.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn flush_pending_writes(&self) -> bool {
        let Some(runtime_size) = self.runtime_size.as_ref() else {
            return true;
        };
        if !runtime_size.pending_dirty.load(Ordering::Acquire) {
            return true;
        }
        let _mutation = runtime_size.mutation.lock();
        flush_pending_write_state(runtime_size)
    }

    pub(crate) fn flush_pending_writes_during_mutation(&self) -> bool {
        self.runtime_size
            .as_ref()
            .map(|runtime_size| flush_pending_write_state(runtime_size))
            .unwrap_or(true)
    }

    pub(crate) fn balance_pending_writes(&self) -> bool {
        balance_pending_writeback()
    }

    pub(crate) fn discard_pending_writes(&self) {
        let Some(runtime_size) = self.runtime_size.as_ref() else {
            return;
        };
        let _mutation = runtime_size.mutation.lock();
        discard_pending_write_state(runtime_size);
    }

    pub(crate) fn discard_pending_writes_during_mutation(&self) {
        if let Some(runtime_size) = self.runtime_size.as_ref() {
            discard_pending_write_state(runtime_size);
        }
    }

    pub fn update_cached_mode(&self, mode: u32) {
        if let Some(ref mut info) = *self.inode_info.exclusive_access() {
            info.mode = mode;
            let file_type = mode & 0o170000;
            info.is_dir = file_type == 0o040000;
            info.is_link = file_type == 0o120000;
        }
    }

    pub fn update_cached_owner(&self, uid: u32, gid: u32, mode: u32, ctime: u32) {
        if let Some(ref mut info) = *self.inode_info.exclusive_access() {
            info.uid = uid;
            info.gid = gid;
            info.mode = mode;
            info.ctime = ctime;
        }
    }

    pub fn update_cached_times(&self, atime: Option<u32>, mtime: Option<u32>, ctime: Option<u32>) {
        if let Some(ref mut info) = *self.inode_info.exclusive_access() {
            if let Some(atime) = atime {
                info.atime = atime;
            }
            if let Some(mtime) = mtime {
                info.mtime = mtime;
            }
            if let Some(ctime) = ctime {
                info.ctime = ctime;
            }
        }
    }
}

// ==================== Mount ====================

pub struct Mount {
    pub mnt_point: Arc<Dentry>,
    pub mnt_root: Arc<Dentry>,
}

lazy_static! {
    pub static ref MOUNT_TABLE: VfsLock<Vec<Mount>> =
        VfsLock::new_named(Vec::with_capacity(MOUNT_TABLE_CAPACITY), "VfsMountTable");
    static ref READONLY_MOUNT_PATHS: VfsLock<Vec<String>> =
        VfsLock::new_named(Vec::with_capacity(MOUNT_TABLE_CAPACITY), "VfsReadonlyMounts");
    // Quota accounting is updated both before and after direct lwext4
    // transactions.  It never calls back into VFS/Fs, so give it a dedicated
    // post-Fs class instead of pretending it is ordinary dentry metadata.
    static ref TMPFS_MOUNTS: FsAuxLock<Vec<TmpfsMount>> =
        FsAuxLock::new_named(Vec::with_capacity(MOUNT_TABLE_CAPACITY), "VfsTmpfsMounts");
}

struct TmpfsMount {
    path: String,
    limit: Option<u64>,
    used: u64,
    files: BTreeMap<String, u64>,
}

fn normalize_mount_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('\0').trim_end_matches('/');
    if trimmed.is_empty() {
        String::from("/")
    } else {
        String::from(trimmed)
    }
}

fn path_is_under_mount(path: &str, mount_path: &str) -> bool {
    if mount_path == "/" {
        return true;
    }
    path == mount_path
        || path
            .strip_prefix(mount_path)
            .map(|suffix| suffix.starts_with('/'))
            .unwrap_or(false)
}

pub fn vfs_set_readonly_mount(path: &str, readonly: bool) {
    let normalized = normalize_mount_path(path);
    let mut paths = READONLY_MOUNT_PATHS.exclusive_access();
    if readonly {
        if !paths.iter().any(|p| p == &normalized) {
            paths.push(normalized);
        }
    } else {
        paths.retain(|p| p != &normalized);
    }
}

pub fn vfs_path_is_readonly(path: &str) -> bool {
    let normalized = normalize_mount_path(path);
    READONLY_MOUNT_PATHS
        .exclusive_access()
        .iter()
        .any(|mount_path| path_is_under_mount(&normalized, mount_path))
}

pub fn vfs_dentry_is_readonly(dentry: &Arc<Dentry>) -> bool {
    vfs_path_is_readonly(&dentry.path)
}

pub fn vfs_set_tmpfs_mount(path: &str, limit: Option<u64>) {
    let normalized = normalize_mount_path(path);
    let mut mounts = TMPFS_MOUNTS.exclusive_access();
    if let Some(mount) = mounts.iter_mut().find(|mount| mount.path == normalized) {
        mount.limit = limit;
        mount.used = 0;
        mount.files.clear();
        return;
    }
    mounts.push(TmpfsMount {
        path: normalized,
        limit,
        used: 0,
        files: BTreeMap::new(),
    });
}

pub fn vfs_clear_tmpfs_mount(path: &str) {
    let normalized = normalize_mount_path(path);
    TMPFS_MOUNTS
        .exclusive_access()
        .retain(|mount| mount.path != normalized);
}

pub fn vfs_tmpfs_can_resize(path: &str, file_key: &str, old_size: u64, new_size: u64) -> bool {
    let normalized = normalize_mount_path(path);
    let mut mounts = TMPFS_MOUNTS.exclusive_access();
    let Some(mount) = mounts
        .iter_mut()
        .filter(|mount| path_is_under_mount(&normalized, &mount.path))
        .max_by_key(|mount| mount.path.len())
    else {
        return true;
    };

    if !mount.files.contains_key(file_key) {
        mount.used = mount.used.saturating_add(old_size);
        mount.files.insert(String::from(file_key), old_size);
    }

    let current = mount.files.get(file_key).copied().unwrap_or(old_size);
    if new_size <= current {
        return true;
    }
    let delta = new_size - current;
    mount
        .limit
        .map(|limit| mount.used.saturating_add(delta) <= limit)
        .unwrap_or(true)
}

pub fn vfs_tmpfs_commit_resize(path: &str, file_key: &str, old_size: u64, new_size: u64) {
    let normalized = normalize_mount_path(path);
    let mut mounts = TMPFS_MOUNTS.exclusive_access();
    let Some(mount) = mounts
        .iter_mut()
        .filter(|mount| path_is_under_mount(&normalized, &mount.path))
        .max_by_key(|mount| mount.path.len())
    else {
        return;
    };

    let current = mount.files.get(file_key).copied().unwrap_or(old_size);
    if new_size >= current {
        mount.used = mount.used.saturating_add(new_size - current);
    } else {
        mount.used = mount.used.saturating_sub(current - new_size);
    }
    mount.files.insert(String::from(file_key), new_size);
}

fn lookup_mount(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    let table = MOUNT_TABLE.exclusive_access();
    for m in table.iter() {
        if Arc::ptr_eq(&m.mnt_point, dentry) || m.mnt_point.path == dentry.path {
            return Some(m.mnt_root.clone());
        }
    }
    None
}

fn is_mount_root(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    let table = MOUNT_TABLE.exclusive_access();
    for m in table.iter() {
        if Arc::ptr_eq(&m.mnt_root, dentry) || m.mnt_root.path == dentry.path {
            return Some(m.mnt_point.clone());
        }
    }
    None
}

// ==================== Path Resolution ====================

// Cap one exceptionally large directory to roughly 1/32 of the kernel heap.
// The estimate includes the Dentry, path/name strings, Arc and B-tree node.
const DENTRY_CACHE_ESTIMATED_ENTRY_BYTES: usize = 512;
const DENTRY_CHILD_CACHE_MIN_LIMIT: usize = 1024;
const DENTRY_CHILD_CACHE_MAX_LIMIT: usize = 32 * 1024;

fn dentry_child_cache_limit() -> usize {
    (crate::mm::heap_total_bytes() / (DENTRY_CACHE_ESTIMATED_ENTRY_BYTES * 32))
        .clamp(DENTRY_CHILD_CACHE_MIN_LIMIT, DENTRY_CHILD_CACHE_MAX_LIMIT)
}

fn dentry_negative_cache_limit() -> usize {
    (dentry_child_cache_limit() / 4).max(256)
}

pub fn dentry_parent(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    dentry.parent.as_ref().and_then(Weak::upgrade)
}

fn vfs_parent(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    if let Some(mnt_point) = is_mount_root(dentry) {
        return dentry_parent(&mnt_point);
    }
    dentry_parent(dentry)
}

const MAX_SYMLINK_DEPTH: usize = 40;
const MAX_SYMLINK_TARGET_LEN: usize = 4096;

pub(crate) fn read_dentry_symlink_bytes(dentry: &Arc<Dentry>) -> Option<Vec<u8>> {
    let info = dentry.inode_info()?;
    if !info.is_link {
        return None;
    }
    let size = usize::try_from(info.size).ok()?;
    if size > MAX_SYMLINK_TARGET_LEN {
        return None;
    }

    let mut target_buf = alloc::vec![0u8; size.saturating_add(1).max(1)];
    let mut rcnt = 0usize;
    let c_path = CString::new(dentry.fs_path.as_bytes()).ok()?;
    let rc = {
        let _fs_lock = dentry.super_block.lock_fs();
        unsafe {
            ext4_readlink(
                c_path.as_ptr() as *const c_char,
                target_buf.as_mut_ptr() as *mut c_char,
                target_buf.len(),
                &mut rcnt,
            )
        }
    };
    if rc != 0 || rcnt == 0 || rcnt > target_buf.len() {
        return None;
    }

    target_buf.truncate(rcnt);
    while target_buf.last() == Some(&0) {
        target_buf.pop();
    }
    if target_buf.is_empty() {
        None
    } else {
        Some(target_buf)
    }
}

pub(crate) fn read_dentry_symlink(dentry: &Arc<Dentry>) -> Option<String> {
    String::from_utf8(read_dentry_symlink_bytes(dentry)?).ok()
}

fn begin_child_cache_mutation(parent: &Dentry) -> u64 {
    let epoch = parent.child_cache_epoch.load(Ordering::Acquire);
    debug_assert_eq!(epoch & 1, 0);
    let updating = epoch.wrapping_add(1);
    parent
        .child_cache_epoch
        .store(updating, Ordering::Release);
    updating
}

fn finish_child_cache_mutation(parent: &Dentry, updating: u64) {
    debug_assert_eq!(updating & 1, 1);
    parent
        .child_cache_epoch
        .store(updating.wrapping_add(1), Ordering::Release);
}

pub(crate) struct ChildCacheMutation<'a> {
    parent: &'a Dentry,
    _guard: SleepGuard<'a, ()>,
    updating: u64,
}

impl Drop for ChildCacheMutation<'_> {
    fn drop(&mut self) {
        finish_child_cache_mutation(&self.parent, self.updating);
    }
}

pub(crate) fn begin_cached_child_mutation(
    parent: &Arc<Dentry>,
) -> ChildCacheMutation<'_> {
    let guard = parent.child_mutation.lock();
    let mutation = ChildCacheMutation {
        parent,
        _guard: guard,
        updating: begin_child_cache_mutation(parent),
    };
    // The odd epoch prevents concurrent lookups from publishing an ENOENT
    // observed before or during the namespace operation.
    let retired = parent.negative_children.exclusive_access().take();
    drop(retired);
    mutation
}

fn child_cache_epoch_matches(parent: &Dentry, expected_epoch: Option<u64>) -> bool {
    match expected_epoch {
        Some(epoch) => epoch & 1 == 0 && parent.child_cache_epoch.load(Ordering::Acquire) == epoch,
        None => true,
    }
}

fn negative_child_cached(parent: &Dentry, name: &str, expected_epoch: u64) -> bool {
    if !child_cache_epoch_matches(parent, Some(expected_epoch)) {
        return false;
    }
    let cached = {
        let negative = parent.negative_children.exclusive_access();
        negative.as_ref().is_some_and(|set| set.contains(name))
    };
    cached && child_cache_epoch_matches(parent, Some(expected_epoch))
}

fn cache_negative_child(parent: &Dentry, name: &str, expected_epoch: Option<u64>) {
    if !child_cache_epoch_matches(parent, expected_epoch) || expected_epoch.is_none() {
        return;
    }

    let mut candidate = Some(String::from(name));
    let mut retired = None;
    {
        let mut negative = parent.negative_children.exclusive_access();
        if !child_cache_epoch_matches(parent, expected_epoch) {
            return;
        }
        let set = negative.get_or_insert_with(BTreeSet::new);
        if !set.contains(name) {
            if set.len() >= dentry_negative_cache_limit() {
                retired = set.pop_first();
            }
            set.insert(
                candidate
                    .take()
                    .expect("negative dentry candidate remains owned"),
            );
        }
    }
    drop(retired);
    drop(candidate);
}

fn insert_positive_child(
    parent: &Arc<Dentry>,
    child: Arc<Dentry>,
    expected_epoch: Option<u64>,
) -> Option<Arc<Dentry>> {
    if !child_cache_epoch_matches(parent, expected_epoch) {
        return None;
    }
    let key = child.name.clone();
    let mut retired_child = None;
    let result = {
        let mut children = parent.children.exclusive_access();
        if !child_cache_epoch_matches(parent, expected_epoch) {
            None
        } else {
            let map = children.get_or_insert_with(BTreeMap::new);
            if let Some(cached) = map.get(key.as_str()) {
                if cached.inode_num == child.inode_num {
                    Some(cached.clone())
                } else {
                    retired_child = map.insert(key, child.clone());
                    Some(child)
                }
            } else {
                if map.len() >= dentry_child_cache_limit() {
                    let mut removed = false;
                    map.retain(|_, cached| {
                        if !removed && Arc::strong_count(cached) == 1 {
                            retired_child = Some(cached.clone());
                            removed = true;
                            false
                        } else {
                            true
                        }
                    });
                    if !removed {
                        return None;
                    }
                }
                map.insert(key, child.clone());
                Some(child)
            }
        }
    };
    drop(retired_child);
    result
}

pub(crate) fn cache_child_during_mutation(parent: &Arc<Dentry>, child: Arc<Dentry>) -> Arc<Dentry> {
    insert_positive_child(parent, child.clone(), None).unwrap_or(child)
}

pub(crate) fn cache_renamed_leaf_during_mutation(
    parent: &Arc<Dentry>,
    name: &str,
    source: &Arc<Dentry>,
) -> Arc<Dentry> {
    debug_assert!(!source.is_dir);
    let moved = Dentry::new_child(
        parent,
        name,
        source.inode_num,
        false,
        source.is_link,
    );
    if let Some(info) = source.cached_inode_info_snapshot() {
        moved.set_cached_inode_info(info);
    }
    cache_child_during_mutation(parent, moved)
}

pub(crate) fn invalidate_cached_child_during_mutation(parent: &Arc<Dentry>, name: &str) {
    let mut retired_positive = None;
    {
        let mut children = parent.children.exclusive_access();
        if let Some(map) = children.as_mut() {
            retired_positive = map.remove(name);
        }
    }
    drop(retired_positive);
}

pub(crate) fn invalidate_cached_child(parent: &Arc<Dentry>, name: &str) {
    let _mutation = begin_cached_child_mutation(parent);
    invalidate_cached_child_during_mutation(parent, name);
}

pub(crate) fn mark_cached_child_absent(parent: &Arc<Dentry>, name: &str) {
    {
        let _mutation = begin_cached_child_mutation(parent);
        invalidate_cached_child_during_mutation(parent, name);
    }
    let epoch = parent.child_cache_epoch.load(Ordering::Acquire);
    cache_negative_child(parent, name, Some(epoch));
}

fn cached_positive_child(parent: &Dentry, name: &str) -> Option<Arc<Dentry>> {
    let children = parent.children.exclusive_access();
    let cached = children.as_ref().and_then(|map| map.get(name).cloned());
    #[cfg(feature = "lock-stats")]
    if cached.is_some() {
        VFS_POSITIVE_DENTRY_HITS.fetch_add(1, Ordering::Relaxed);
    }
    cached
}

pub(crate) fn cache_readdir_entries(parent: &Arc<Dentry>, dirents: &[u8], epoch: u64) {
    const DIRENT64_FIXED_SIZE: usize = 19;
    if !child_cache_epoch_matches(parent, Some(epoch)) {
        return;
    }

    let mut pending = Vec::new();
    let mut offset = 0usize;
    while offset.saturating_add(DIRENT64_FIXED_SIZE) <= dirents.len() {
        let record = &dirents[offset..];
        let record_len = u16::from_le_bytes([record[16], record[17]]) as usize;
        if record_len < DIRENT64_FIXED_SIZE || offset.saturating_add(record_len) > dirents.len() {
            break;
        }
        let inode = u64::from_le_bytes([
            record[0], record[1], record[2], record[3], record[4], record[5], record[6], record[7],
        ]);
        let d_type = record[18];
        let name_field = &record[DIRENT64_FIXED_SIZE..record_len];
        let name_len = name_field
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name_field.len());
        let name = core::str::from_utf8(&name_field[..name_len]).ok();
        if let (Ok(inode), Some(name)) = (u32::try_from(inode), name) {
            if inode != 0 && name != "." && name != ".." && d_type != 0 {
                let child = Dentry::new_child(
                    parent,
                    name,
                    inode,
                    d_type == 4,
                    d_type == 10,
                );
                pending.push((String::from(name), child));
            }
        }
        offset += record_len;
    }
    if pending.is_empty() || !child_cache_epoch_matches(parent, Some(epoch)) {
        return;
    }

    let limit = dentry_child_cache_limit();
    let mut retired = Vec::with_capacity(pending.len());
    #[cfg(feature = "lock-stats")]
    let mut published = 0usize;
    {
        let mut children = parent.children.exclusive_access();
        if !child_cache_epoch_matches(parent, Some(epoch)) {
            return;
        }
        let map = children.get_or_insert_with(BTreeMap::new);
        for (name, child) in pending {
            if map
                .get(name.as_str())
                .is_some_and(|cached| cached.inode_num == child.inode_num)
            {
                continue;
            }
            if map.len() >= limit && !map.contains_key(name.as_str()) {
                continue;
            }
            if let Some(old) = map.insert(name, child) {
                retired.push(old);
            }
            #[cfg(feature = "lock-stats")]
            {
                published += 1;
            }
        }
    }
    drop(retired);
    #[cfg(feature = "lock-stats")]
    VFS_READDIR_DENTRIES_PUBLISHED.fetch_add(published, Ordering::Relaxed);
}

fn child_fs_path(parent: &Dentry, name: &str) -> String {
    if parent.fs_path == "/" {
        format!("/{}", name)
    } else if parent.fs_path.ends_with('/') {
        format!("{}{}", parent.fs_path, name)
    } else {
        format!("{}/{}", parent.fs_path, name)
    }
}

fn mounted_child(child: Arc<Dentry>) -> Arc<Dentry> {
    lookup_mount(&child).unwrap_or(child)
}

pub(crate) fn vfs_lookup_during_mutation(
    parent: &Arc<Dentry>,
    name: &str,
) -> Option<Arc<Dentry>> {
    if let Some(cached) = vfs_cached_lookup_during_mutation(parent, name) {
        return Some(cached);
    }

    let info = parent
        .super_block
        .get_inode_info_result(&child_fs_path(parent, name))
        .ok()?;
    let child = Dentry::new_child(
        parent,
        name,
        info.inode_num,
        info.is_dir,
        info.is_link,
    );
    child.set_cached_inode_info(info);
    Some(mounted_child(cache_child_during_mutation(parent, child)))
}

pub(crate) fn vfs_cached_lookup_during_mutation(
    parent: &Arc<Dentry>,
    name: &str,
) -> Option<Arc<Dentry>> {
    match name {
        "." => return Some(parent.clone()),
        ".." => return vfs_parent(parent).or_else(|| Some(parent.clone())),
        _ => {}
    }

    cached_positive_child(parent, name).map(mounted_child)
}

pub fn vfs_lookup(parent: &Arc<Dentry>, name: &str) -> Option<Arc<Dentry>> {
    match name {
        "." => return Some(parent.clone()),
        ".." => {
            if let Some(task) = crate::task::current_task() {
                let root = task.inner_exclusive_access().root.clone();
                if Arc::ptr_eq(parent, &root) || parent.path == root.path {
                    return Some(parent.clone());
                }
            }
            return vfs_parent(parent).or_else(|| Some(parent.clone()));
        }
        _ => {}
    }

    loop {
        let lookup_epoch = parent.child_cache_epoch.load(Ordering::Acquire);
        if lookup_epoch & 1 != 0 {
            // Namespace mutations publish an odd epoch before touching ext4.
            // Waiting on the per-directory lock prevents an uncached duplicate
            // Dentry from escaping and serializes same-directory operations.
            let guard = parent.child_mutation.lock();
            drop(guard);
            continue;
        }

        let cached = cached_positive_child(parent, name);
        if !child_cache_epoch_matches(parent, Some(lookup_epoch)) {
            continue;
        }
        if let Some(cached) = cached {
            return Some(mounted_child(cached));
        }
        if negative_child_cached(parent, name, lookup_epoch) {
            return None;
        }

        // lwext4 serializes physical lookups anyway.  Queue cache misses on
        // the directory namespace lock, then recheck after acquiring it so
        // concurrent readers share the first reader's positive/negative result.
        let _lookup = parent.child_mutation.lock();
        let lookup_epoch = parent.child_cache_epoch.load(Ordering::Acquire);
        if lookup_epoch & 1 != 0 {
            continue;
        }
        if let Some(cached) = cached_positive_child(parent, name) {
            return Some(mounted_child(cached));
        }
        if negative_child_cached(parent, name, lookup_epoch) {
            return None;
        }

        let info = match parent
            .super_block
            .get_inode_info_result(&child_fs_path(parent, name))
        {
            Ok(info) => info,
            Err(rc) => {
                if !child_cache_epoch_matches(parent, Some(lookup_epoch)) {
                    continue;
                }
                if rc == ENOENT as i32 {
                    cache_negative_child(parent, name, Some(lookup_epoch));
                    if !child_cache_epoch_matches(parent, Some(lookup_epoch)) {
                        continue;
                    }
                }
                return None;
            }
        };
        let (child_inode, is_dir, is_link) = (info.inode_num, info.is_dir, info.is_link);

        let child = Dentry::new_child(parent, name, child_inode, is_dir, is_link);
        child.set_cached_inode_info(info);

        if let Some(canonical) = insert_positive_child(parent, child.clone(), Some(lookup_epoch)) {
            return Some(mounted_child(canonical));
        }
        if child_cache_epoch_matches(parent, Some(lookup_epoch)) {
            // A full cache may deliberately leave a stable lookup uncached.
            return Some(mounted_child(child));
        }
    }
}

fn vfs_resolve_path_inner(
    start: &Arc<Dentry>,
    path: &str,
    follow_final_symlink: bool,
) -> Option<Arc<Dentry>> {
    let path = path.trim_matches('\0');
    if path.is_empty() {
        return None;
    }

    let is_abs = path.starts_with('/');
    let root = crate::task::current_task()
        .map(|t| t.inner_exclusive_access().root.clone())
        .unwrap_or_else(|| ROOT_DENTRY.clone());
    let initial = if is_abs { root.clone() } else { start.clone() };

    // Most build-system paths contain no symlink. Walk borrowed components
    // first and only allocate a rewrite queue when a symlink is encountered.
    let mut current = initial.clone();
    let mut borrowed_components = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .peekable();
    let mut needs_symlink_resolution = false;
    while let Some(component) = borrowed_components.next() {
        let next = match vfs_lookup(&current, component) {
            Some(next) => next,
            None => {
                #[cfg(feature = "buildstorm-diagnostics")]
                if path.contains("scintillaos-fs-stress") {
                    let cached = cached_positive_child(&current, component).map(|child| {
                        (
                            Arc::as_ptr(&child) as usize,
                            child.inode_num,
                            child.is_dir,
                        )
                    });
                    let physical = current
                        .super_block
                        .get_inode_info_result(&child_fs_path(&current, component))
                        .map(|info| (info.inode_num, info.is_dir, info.is_link));
                    crate::println!(
                        "[buildstorm-fs] resolve-component-missing path={} current={} current_ptr={:#x} current_inode={} component={} epoch={} cached={:?} physical={:?}",
                        path,
                        current.fs_path,
                        Arc::as_ptr(&current) as usize,
                        current.inode_num,
                        component,
                        current.child_cache_epoch.load(Ordering::Acquire),
                        cached,
                        physical,
                    );
                }
                return None;
            }
        };
        if next.is_link && (follow_final_symlink || borrowed_components.peek().is_some()) {
            needs_symlink_resolution = true;
            break;
        }
        current = next;
    }
    if !needs_symlink_resolution {
        return lookup_mount(&current).or(Some(current));
    }

    let mut current = initial;
    let mut components: VecDeque<String> = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(String::from)
        .collect();
    let mut symlink_depth = 0usize;

    while let Some(component) = components.pop_front() {
        let next = vfs_lookup(&current, component.as_str())?;
        if next.is_link && (follow_final_symlink || !components.is_empty()) {
            symlink_depth += 1;
            if symlink_depth > MAX_SYMLINK_DEPTH {
                return None;
            }

            let target = read_dentry_symlink(&next)?;
            if target.starts_with('/') {
                current = root.clone();
            }
            let mut target_components: Vec<String> = target
                .split('/')
                .filter(|part| !part.is_empty() && *part != ".")
                .map(String::from)
                .collect();
            while let Some(part) = target_components.pop() {
                components.push_front(part);
            }
            continue;
        }

        current = next;
    }

    lookup_mount(&current).or(Some(current))
}

pub fn vfs_resolve_path(start: &Arc<Dentry>, path: &str) -> Option<Arc<Dentry>> {
    vfs_resolve_path_inner(start, path, false)
}

pub fn vfs_resolve_with_fallback(start: &Arc<Dentry>, path: &str) -> Option<Arc<Dentry>> {
    if let Some(d) = vfs_resolve_path(start, path) {
        return Some(d);
    }
    let bare = path.trim_matches('\0').trim_start_matches("./");
    if !bare.starts_with('/') && !bare.contains('/') && bare != "." && bare != ".." {
        let bin_path = format!("/bin/{}", bare);
        if let Some(d) = vfs_resolve_path(start, &bin_path) {
            return Some(d);
        }
    }
    None
}

pub fn vfs_resolve_parent_and_name(
    start: &Arc<Dentry>,
    path: &str,
) -> Option<(Arc<Dentry>, String)> {
    let path = path.trim_matches('\0');
    if path.is_empty() {
        return None;
    }

    let filename = path
        .rsplit('/')
        .find(|component| !component.is_empty() && *component != ".")?;
    let filename_offset = filename.as_ptr() as usize - path.as_ptr() as usize;
    let parent_path = &path[..filename_offset];
    let parent_path = if parent_path.is_empty() {
        if path.starts_with('/') {
            "/"
        } else {
            "."
        }
    } else {
        parent_path
    };
    let parent = vfs_resolve_path_inner(start, parent_path, true)?;
    if !parent.is_dir {
        return None;
    }

    Some((parent, String::from(filename)))
}

// ==================== File Creation ====================

pub fn vfs_create(
    parent: &Arc<Dentry>,
    name: &str,
    is_dir: bool,
    mode: u32,
) -> Option<Arc<Dentry>> {
    // Keep cached lookups non-authoritative from the physical create through
    // publication of the new canonical dentry.
    let _mutation = begin_cached_child_mutation(parent);
    vfs_create_during_mutation(parent, name, is_dir, mode)
}

pub(crate) fn vfs_create_during_mutation(
    parent: &Arc<Dentry>,
    name: &str,
    is_dir: bool,
    mode: u32,
) -> Option<Arc<Dentry>> {
    vfs_create_during_mutation_inner(parent, name, is_dir, mode, false).map(|(dentry, _)| dentry)
}

pub(crate) fn vfs_create_open_file_during_mutation(
    parent: &Arc<Dentry>,
    name: &str,
    mode: u32,
) -> Option<(Arc<Dentry>, ext4_file)> {
    let (dentry, file_handle) =
        vfs_create_during_mutation_inner(parent, name, false, mode, true)?;
    Some((dentry, file_handle?))
}

fn vfs_create_during_mutation_inner(
    parent: &Arc<Dentry>,
    name: &str,
    is_dir: bool,
    mode: u32,
    keep_file_open: bool,
) -> Option<(Arc<Dentry>, Option<ext4_file>)> {
    #[cfg(feature = "buildstorm-diagnostics")]
    if name == "invoked.timestamp" || name == "build-script-build-script-build" {
        crate::println!(
            "[buildstorm-fs] traced-vfs-create name={} dir={} mode={:#o} parent={} parent_ptr={:#x} parent_inode={} epoch={}",
            name,
            is_dir,
            mode,
            parent.fs_path,
            Arc::as_ptr(parent) as usize,
            parent.inode_num,
            parent.child_cache_epoch.load(Ordering::Acquire),
        );
    }
    let (uid, egid, umask) = crate::task::current_task()
        .map(|t| {
            let inner = t.inner_exclusive_access();
            (inner.euid, inner.egid, inner.umask)
        })
        .unwrap_or((0, 0, 0o22));
    let parent_info = parent.inode_info();
    let parent_sgid = parent_info
        .as_ref()
        .map(|info| (info.mode & 0o2000) != 0)
        .unwrap_or(false);
    let gid = if parent_sgid {
        parent_info.as_ref().map(|info| info.gid).unwrap_or(egid)
    } else {
        egid
    };
    let mode_type = mode & 0o170000;
    let file_type = if is_dir {
        0o040000
    } else if mode_type != 0 {
        mode_type
    } else {
        0o100000
    };
    let mut effective_perm = mode & !umask & 0o7777;
    if is_dir && parent_sgid {
        effective_perm |= 0o2000;
    }
    let effective_mode = file_type | effective_perm;
    let (info, file_handle) = parent.super_block.create_file(
        &parent.fs_path,
        parent.inode_num,
        name,
        is_dir,
        effective_mode,
        uid,
        gid,
        keep_file_open,
    )?;

    let child = Dentry::new_child(parent, name, info.inode_num, info.is_dir, info.is_link);
    child.set_cached_inode_info(info);

    Some((cache_child_during_mutation(parent, child), file_handle))
}

// ==================== Mount Operations ====================

pub fn vfs_mount(mnt_point: Arc<Dentry>, sb: Arc<VfsSuperBlock>, fs_path: &str) -> bool {
    let path = mnt_point.path.clone();
    {
        let table = MOUNT_TABLE.exclusive_access();
        if table
            .iter()
            .any(|m| m.mnt_point.path == path && path != "/")
            || table.len() == MOUNT_TABLE_CAPACITY
        {
            return false;
        }
    }

    let mnt_root = Dentry::new_mount_root(sb, mnt_point.path.clone(), fs_path);

    let mut table = MOUNT_TABLE.exclusive_access();
    // Recheck after building the new dentry: another hart may have mounted in
    // the interval.  The preallocated table must never grow under VfsLock.
    if table
        .iter()
        .any(|m| m.mnt_point.path == path && path != "/")
        || table.len() == MOUNT_TABLE_CAPACITY
    {
        return false;
    }
    table.push(Mount {
        mnt_point: mnt_point.clone(),
        mnt_root: mnt_root.clone(),
    });

    true
}

pub fn vfs_umount(mnt_point: &Arc<Dentry>) -> bool {
    let path = &mnt_point.path;
    // Destructing a Mount drops Arcs and may release filesystem state.  Move
    // it out before leaving the VFS domain rather than dropping it in-lock.
    let removed = {
        let mut table = MOUNT_TABLE.exclusive_access();
        table
            .iter()
            .position(|m| &m.mnt_point.path == path)
            .map(|idx| table.remove(idx))
    };
    removed.is_some()
}

// ==================== Root Filesystem ====================

use crate::drivers::{BLOCK_DEVICE, ROOT_BLOCK_DEVICE};

lazy_static! {
    pub static ref ROOT_SB: Arc<VfsSuperBlock> = {
        VfsSuperBlock::new_ext4_at(ROOT_BLOCK_DEVICE.clone(), "ext4", ROOT_LWEXT4_MOUNT)
            .expect("Failed to mount root ext4 filesystem")
    };
    pub static ref ROOT_DENTRY: Arc<Dentry> = Dentry::new_root(ROOT_SB.clone());
}

pub fn mount_root() {
    let root = ROOT_DENTRY.clone();
    MOUNT_TABLE.exclusive_access().push(Mount {
        mnt_point: root.clone(),
        mnt_root: root,
    });
}

fn mounted_super_blocks() -> Vec<Arc<VfsSuperBlock>> {
    let mut super_blocks = Vec::new();
    {
        let table = MOUNT_TABLE.exclusive_access();
        for mount in table.iter() {
            let sb = mount.mnt_root.super_block.clone();
            if !super_blocks.iter().any(|known| Arc::ptr_eq(known, &sb)) {
                super_blocks.push(sb);
            }
        }
    }
    if !super_blocks
        .iter()
        .any(|known| Arc::ptr_eq(known, &ROOT_SB))
    {
        super_blocks.push(ROOT_SB.clone());
    }
    super_blocks
}

fn flush_all_pending_file_writes() -> bool {
    let states = {
        let sizes = INODE_RUNTIME_SIZES.exclusive_access();
        sizes.values().cloned().collect::<Vec<_>>()
    };
    let mut ok = true;
    for state in states {
        if !state.pending_dirty.load(Ordering::Acquire) {
            continue;
        }
        let _mutation = state.mutation.lock();
        ok &= flush_pending_write_state(&state);
    }
    ok
}

#[cfg(feature = "lock-stats")]
pub fn dump_pending_write_stats() {
    crate::println!(
        "[pending-write-stats] dirty_bytes={} limit={} cache_reads={} cache_read_bytes={} overlay_reads={} overlay_bytes={} flushes={} flushed_bytes={} pressure_flushes={} deferred_closes={}",
        PENDING_WRITE_BYTES.load(Ordering::Relaxed),
        pending_writeback_limit(),
        PENDING_CACHE_READS.load(Ordering::Relaxed),
        PENDING_CACHE_READ_BYTES.load(Ordering::Relaxed),
        PENDING_CACHE_OVERLAY_READS.load(Ordering::Relaxed),
        PENDING_CACHE_OVERLAY_BYTES.load(Ordering::Relaxed),
        PENDING_CACHE_FLUSHES.load(Ordering::Relaxed),
        PENDING_CACHE_FLUSHED_BYTES.load(Ordering::Relaxed),
        PENDING_CACHE_PRESSURE_FLUSHES.load(Ordering::Relaxed),
        PENDING_CACHE_DEFERRED_CLOSES.load(Ordering::Relaxed),
    );
}

#[cfg(feature = "lock-stats")]
pub fn dump_vfs_cache_stats() {
    crate::println!(
        "[vfs-cache-stats] inode_queries={} found={} absent={} errors={} positive_hits={} readdir_published={} dirfd_epoch_hits={} dirfd_revalidations={} child_limit={} negative_limit={}",
        VFS_INODE_INFO_QUERIES.load(Ordering::Relaxed),
        VFS_INODE_INFO_FOUND.load(Ordering::Relaxed),
        VFS_INODE_INFO_ABSENT.load(Ordering::Relaxed),
        VFS_INODE_INFO_ERRORS.load(Ordering::Relaxed),
        VFS_POSITIVE_DENTRY_HITS.load(Ordering::Relaxed),
        VFS_READDIR_DENTRIES_PUBLISHED.load(Ordering::Relaxed),
        VFS_DIRFD_EPOCH_HITS.load(Ordering::Relaxed),
        VFS_DIRFD_REVALIDATIONS.load(Ordering::Relaxed),
        dentry_child_cache_limit(),
        dentry_negative_cache_limit(),
    );
}

pub fn sync_filesystems() -> i32 {
    let mut result = if flush_all_pending_file_writes() { 0 } else { -1 };
    for sb in mounted_super_blocks() {
        let rc = sb.sync_all();
        if result == 0 && rc != 0 {
            result = rc;
        }
    }
    result
}

/// Flush and unmount every ext4 volume immediately before powering off.
///
/// The mount table can contain multiple dentries backed by the same superblock,
/// so deduplicate the snapshot before invoking lwext4.  Locks are released
/// before flushing/unmounting to preserve the VFS lock order.
pub fn shutdown_filesystems() {
    let super_blocks = mounted_super_blocks();

    let _ = flush_all_pending_file_writes();
    for sb in &super_blocks {
        let _ = sb.sync_all();
    }
    // Unmount child/test filesystems before the root filesystem.
    for sb in super_blocks.iter().rev() {
        sb.force_umount();
    }
}

/// Mount the test disk (bus 0) at the specified mount point dentry.
pub fn mount_test_disk_at(mnt_point: Arc<Dentry>) -> bool {
    let test_sb = match VfsSuperBlock::new_ext4_at(BLOCK_DEVICE.clone(), "ext4", "/__mount_vda/") {
        Some(sb) => sb,
        None => return false,
    };
    vfs_mount(mnt_point, test_sb, "/__mount_vda/")
}

pub fn list_apps() {
    println!("/**** APPS ****");
    ROOT_SB
        .dir_get_entries(ROOT_LWEXT4_BIN)
        .iter()
        .filter(|e| {
            let name = e.get_name();
            name != "." && name != ".." && !name.is_empty()
        })
        .for_each(|e| println!("{}", e.get_name()));
    println!("**************/");
}
