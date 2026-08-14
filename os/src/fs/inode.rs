use super::vfs::{
    begin_cached_child_mutation, cache_readdir_entries, dentry_parent,
    cache_renamed_leaf_during_mutation, flush_and_reset_inode_runtime, invalidate_cached_child,
    invalidate_cached_child_during_mutation, mark_cached_child_absent, read_dentry_symlink, vfs_create,
    vfs_cached_lookup_during_mutation, vfs_create_open_file_during_mutation,
    vfs_lookup_during_mutation,
    vfs_resolve_parent_and_name, vfs_resolve_path, vfs_resolve_with_fallback,
    vfs_tmpfs_can_resize, vfs_tmpfs_commit_resize, Dentry,
};
use super::{File, FilePageCacheKey, Kstat, S_IFDIR, S_IFLNK, S_IFREG};
use crate::mm::{
    cache_file_read, read_file_page_cache, reserve_file_page_cache_read,
    wait_for_file_page_load, FilePageCacheRead, FilePageCacheReadLoad,
    FilePageCacheWriteGuard, FILE_PAGE_READAHEAD_SIZE,
};
use crate::sync::{SleepLock, VfsLock};
use alloc::collections::BTreeMap;
use alloc::ffi::CString;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::*;
use lwext4_rust::bindings::*;

const EXT4_BLOCK_SIZE: u32 = 4096;
// Match foreground transactions to the block cache's bounded writeback batch.
// Large writes retain per-inode ordering but let unrelated inodes run between
// chunks instead of monopolizing lwext4's per-volume lock.
const EXT4_WRITE_LOCK_CHUNK_SIZE: usize = 32 * 1024;
const EXT4_WRITE_UNCONTENDED_CHUNK_SIZE: usize = 256 * 1024;
const READ_AHEAD_POOL_SIZE: usize = 16;
const PATH_TIME_CACHE_MAX_ENTRIES: usize = 1024;
const UNLINKED_FILE_PREFIX: &str = ".os-unlinked-";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct OpenInodeKey {
    super_block: usize,
    inode: u32,
}

struct OpenInodeState {
    count: usize,
    orphan_path: Option<String>,
    orphan_needs_flush: bool,
    unlink_in_progress: bool,
}

struct OpenInodeCleanup {
    orphan_path: String,
    needs_flush: bool,
}

struct ReadAheadPool {
    buffers: [Option<Vec<u8>>; READ_AHEAD_POOL_SIZE],
}

impl ReadAheadPool {
    const fn new() -> Self {
        Self {
            buffers: [const { None }; READ_AHEAD_POOL_SIZE],
        }
    }

    fn take(&mut self) -> Option<Vec<u8>> {
        self.buffers.iter_mut().find_map(Option::take)
    }

    fn put(&mut self, buffer: Vec<u8>) -> Result<(), Vec<u8>> {
        let Some(slot) = self.buffers.iter_mut().find(|slot| slot.is_none()) else {
            return Err(buffer);
        };
        *slot = Some(buffer);
        Ok(())
    }
}

struct ReadAheadBuffer(Option<Vec<u8>>);

impl Deref for ReadAheadBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.0
            .as_deref()
            .expect("read-ahead buffer remains owned until drop")
    }
}

impl DerefMut for ReadAheadBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
            .as_deref_mut()
            .expect("read-ahead buffer remains owned until drop")
    }
}

impl Drop for ReadAheadBuffer {
    fn drop(&mut self) {
        let Some(mut buffer) = self.0.take() else {
            return;
        };
        buffer.clear();
        let retired = READ_AHEAD_POOL.exclusive_access().put(buffer).err();
        drop(retired);
    }
}

#[derive(Clone, Copy, Default)]
struct CachedTimes {
    atime: Option<i64>,
    mtime: Option<i64>,
    ctime: Option<i64>,
}

lazy_static! {
    /// Path metadata is VFS state. It is always accessed before entering an
    /// Ext4 operation so the global order remains `VfsLock -> FsLock`.
    static ref PATH_TIME_CACHE: VfsLock<BTreeMap<String, CachedTimes>> =
        VfsLock::new_named(BTreeMap::new(), "VfsPathTimeCache");
    static ref PATH_SPECIAL_MODE_CACHE: VfsLock<BTreeMap<String, u32>> =
        VfsLock::new_named(BTreeMap::new(), "VfsPathSpecialModeCache");
    static ref PATH_FS_FLAGS_CACHE: VfsLock<BTreeMap<String, usize>> =
        VfsLock::new_named(BTreeMap::new(), "VfsPathFsFlagsCache");
    /// Tracks live lwext4 handles so unlink can preserve Linux's
    /// "open file survives directory removal" semantics.
    ///
    /// The table is VFS metadata and is never held while entering lwext4.
    static ref OPEN_INODES: VfsLock<BTreeMap<OpenInodeKey, OpenInodeState>> =
        VfsLock::new_named(BTreeMap::new(), "VfsOpenInodes");
    static ref READ_AHEAD_POOL: VfsLock<ReadAheadPool> =
        VfsLock::new_named(ReadAheadPool::new(), "VfsReadAheadPool");
}

static NEXT_UNLINKED_FILE: AtomicUsize = AtomicUsize::new(0);

fn open_inode_key(dentry: &Arc<Dentry>) -> Option<OpenInodeKey> {
    if dentry.is_dir || dentry.is_link {
        return None;
    }
    Some(OpenInodeKey {
        super_block: Arc::as_ptr(&dentry.super_block) as usize,
        inode: dentry.inode_num,
    })
}

fn page_cache_key_from_open_key(key: OpenInodeKey) -> FilePageCacheKey {
    FilePageCacheKey {
        super_block: key.super_block,
        inode: key.inode,
    }
}

fn dentry_file_page_cache_key(dentry: &Dentry) -> Option<FilePageCacheKey> {
    if dentry.is_dir || dentry.is_link {
        return None;
    }
    Some(FilePageCacheKey {
        super_block: Arc::as_ptr(&dentry.super_block) as usize,
        inode: dentry.inode_num,
    })
}

fn invalidate_dentry_file_pages(dentry: &Dentry) {
    if let Some(key) = dentry_file_page_cache_key(dentry) {
        crate::mm::invalidate_file_page_cache(key);
    }
}

fn begin_dentry_cached_write(dentry: &Dentry) -> FilePageCacheWriteGuard {
    FilePageCacheWriteGuard::new(dentry_file_page_cache_key(dentry))
}

fn register_open_inode(key: OpenInodeKey) {
    let mut open = OPEN_INODES.exclusive_access();
    let state = open.entry(key).or_insert(OpenInodeState {
        count: 0,
        orphan_path: None,
        orphan_needs_flush: false,
        unlink_in_progress: false,
    });
    state.count += 1;
}

fn unregister_open_inode(key: OpenInodeKey) -> Option<OpenInodeCleanup> {
    let mut open = OPEN_INODES.exclusive_access();
    let state = open.get_mut(&key)?;
    if state.count == 0 {
        return None;
    }
    state.count -= 1;
    if state.count != 0 || state.unlink_in_progress {
        return None;
    }
    let orphan_path = state.orphan_path.take();
    let needs_flush = state.orphan_needs_flush;
    open.remove(&key);
    orphan_path.map(|orphan_path| OpenInodeCleanup {
        orphan_path,
        needs_flush,
    })
}

fn unlinked_file_path(dentry: &Dentry, inode: u32) -> String {
    let sequence = NEXT_UNLINKED_FILE.fetch_add(1, Ordering::Relaxed);
    let parent = dentry
        .fs_path
        .rfind('/')
        .map(|index| &dentry.fs_path[..index])
        .unwrap_or("");
    if parent.is_empty() {
        alloc::format!("/{}{}-{}", UNLINKED_FILE_PREFIX, inode, sequence)
    } else {
        alloc::format!("{}/{}{}-{}", parent, UNLINKED_FILE_PREFIX, inode, sequence)
    }
}

fn replacement_orphan_path(parent: &Dentry) -> String {
    let sequence = NEXT_UNLINKED_FILE.fetch_add(1, Ordering::Relaxed);
    if parent.fs_path == "/" {
        alloc::format!("/{}rename-{}", UNLINKED_FILE_PREFIX, sequence)
    } else if parent.fs_path.ends_with('/') {
        alloc::format!("{}{}rename-{}", parent.fs_path, UNLINKED_FILE_PREFIX, sequence)
    } else {
        alloc::format!("{}/{}rename-{}", parent.fs_path, UNLINKED_FILE_PREFIX, sequence)
    }
}

fn retain_replaced_open_inode(
    super_block: usize,
    inode: u32,
    orphan_path: &str,
    needs_flush: bool,
) -> bool {
    let key = OpenInodeKey { super_block, inode };
    let mut open = OPEN_INODES.exclusive_access();
    let Some(state) = open.get_mut(&key) else {
        return false;
    };
    if state.count == 0 {
        open.remove(&key);
        return false;
    }
    state.orphan_path = Some(String::from(orphan_path));
    state.orphan_needs_flush = needs_flush;
    state.unlink_in_progress = false;
    true
}

/// Rename an open file to a hidden orphan and defer physical removal until
/// the last open handle closes.
///
/// Reservation and completion are separate VFS critical sections.  lwext4 is
/// entered between them with no VFS metadata lock held, preserving the global
/// `VfsLock -> FsLock` order and allowing a concurrent close to publish count
/// zero without losing the final cleanup.
fn defer_open_file_unlink(dentry: &Arc<Dentry>) -> Option<bool> {
    let info = dentry.inode_info()?;
    if info.links_count > 1 {
        return None;
    }
    let key = OpenInodeKey {
        super_block: Arc::as_ptr(&dentry.super_block) as usize,
        inode: info.inode_num,
    };
    let orphan_path = unlinked_file_path(dentry, info.inode_num);
    let old_path = CString::new(dentry.fs_path.as_bytes()).ok()?;
    let new_path = CString::new(orphan_path.as_bytes()).ok()?;
    {
        let mut open = OPEN_INODES.exclusive_access();
        let state = open.get_mut(&key)?;
        if state.count == 0 || state.orphan_path.is_some() || state.unlink_in_progress {
            return None;
        }
        state.unlink_in_progress = true;
    }

    let renamed = {
        let _fs_lock = dentry.super_block.lock_fs();
        unsafe {
            ext4_frename(
                old_path.as_ptr() as *const core::ffi::c_char,
                new_path.as_ptr() as *const core::ffi::c_char,
            ) == EOK as i32
        }
    };
    let cleanup_path = {
        let mut open = OPEN_INODES.exclusive_access();
        let mut remove_state = false;
        let mut cleanup = None;
        if let Some(state) = open.get_mut(&key) {
            state.unlink_in_progress = false;
            if renamed {
                state.orphan_path = Some(orphan_path.clone());
                state.orphan_needs_flush = false;
            }
            if state.count == 0 {
                cleanup = state.orphan_path.take();
                remove_state = true;
            }
        } else if renamed {
            // A defensive fallback for a corrupted/missing registry entry:
            // do not leave the hidden file behind indefinitely.
            cleanup = Some(orphan_path.clone());
        }
        if remove_state {
            open.remove(&key);
        }
        cleanup
    };
    if let Some(path) = cleanup_path {
        dentry.discard_pending_writes_during_mutation();
        let _ = dentry.super_block.unlink_file(&path);
    }
    Some(renamed)
}

pub fn record_special_path_mode(fs_path: &str, mode_type: u32) {
    if mode_type != 0 && mode_type != S_IFREG {
        PATH_SPECIAL_MODE_CACHE
            .exclusive_access()
            .insert(String::from(fs_path), mode_type);
    }
}

fn path_special_mode(fs_path: &str) -> Option<u32> {
    PATH_SPECIAL_MODE_CACHE
        .exclusive_access()
        .get(fs_path)
        .copied()
}

pub fn update_path_cached_times(
    fs_path: &str,
    atime: Option<i64>,
    mtime: Option<i64>,
    ctime: Option<i64>,
) {
    let mut cache = PATH_TIME_CACHE.exclusive_access();
    if !cache.contains_key(fs_path) && cache.len() >= PATH_TIME_CACHE_MAX_ENTRIES {
        cache.clear();
    }
    let entry = cache
        .entry(String::from(fs_path))
        .or_insert_with(CachedTimes::default);
    if let Some(atime) = atime {
        entry.atime = Some(atime);
    }
    if let Some(mtime) = mtime {
        entry.mtime = Some(mtime);
    }
    if let Some(ctime) = ctime {
        entry.ctime = Some(ctime);
    }
}

fn path_cached_times(fs_path: &str) -> CachedTimes {
    PATH_TIME_CACHE
        .exclusive_access()
        .get(fs_path)
        .copied()
        .unwrap_or_default()
}

pub fn set_path_fs_flags(fs_path: &str, flags: usize) {
    let mut cache = PATH_FS_FLAGS_CACHE.exclusive_access();
    if flags == 0 {
        cache.remove(fs_path);
    } else {
        cache.insert(String::from(fs_path), flags);
    }
}

pub fn path_fs_flags(fs_path: &str) -> usize {
    PATH_FS_FLAGS_CACHE
        .exclusive_access()
        .get(fs_path)
        .copied()
        .unwrap_or(0)
}

// ==================== OSInode ====================

pub fn stat_dentry(dentry: &Arc<Dentry>) -> Kstat {
    // `main` may keep recently written data in the per-inode pending-write
    // cache.  Path-based stat must publish that data before asking lwext4 for
    // the on-disk length; otherwise tools such as GCC can see a zero-length
    // source even though read(2) already returns the complete cached file.
    let pending_flushed = dentry.flush_pending_writes();
    let info = if pending_flushed {
        dentry.refresh_inode_info().or_else(|| dentry.inode_info())
    } else {
        dentry.inode_info()
    };
    let mut st = match info {
        Some(info) => {
            let file_type = if dentry.is_dir || info.is_dir {
                S_IFDIR
            } else if dentry.is_link || info.is_link {
                S_IFLNK
            } else if let Some(mode_type) = path_special_mode(&dentry.fs_path) {
                mode_type
            } else {
                info.mode & 0o170000
            };
            let file_type = if file_type == 0 { S_IFREG } else { file_type };
            let mut st = Kstat::new(file_type | (info.mode & 0o7777));
            st.st_ino = info.inode_num as u64;
            st.st_size = info.size as i64;
            st.st_blksize = EXT4_BLOCK_SIZE;
            st.st_blocks = info.blocks_count;
            st.st_nlink = info.links_count.max(1);
            st.st_uid = info.uid;
            st.st_gid = info.gid;
            if file_type == S_IFDIR {
                st.st_nlink = st.st_nlink.max(2);
            }
            st.st_atime_sec = info.atime as i64;
            st.st_mtime_sec = info.mtime as i64;
            st.st_ctime_sec = info.ctime as i64;
            let cached_times = path_cached_times(&dentry.fs_path);
            let now_sec = crate::time::realtime_sec();
            st.st_atime_sec = cached_times
                .atime
                .unwrap_or_else(|| st.st_atime_sec.min(now_sec));
            st.st_mtime_sec = cached_times
                .mtime
                .unwrap_or_else(|| st.st_mtime_sec.min(now_sec));
            st.st_ctime_sec = cached_times
                .ctime
                .unwrap_or_else(|| st.st_ctime_sec.min(now_sec));
            st
        }
        None => {
            if dentry.is_dir {
                Kstat::new(S_IFDIR | 0o755)
            } else {
                Kstat::new(S_IFREG | 0o644)
            }
        }
    };
    if dentry.is_dir {
        st.st_nlink = st.st_nlink.max(2);
    }
    st
}

pub struct OSInode {
    readable: bool,
    writable: bool,
    dentry: Arc<Dentry>,
    /// Per-open-file offset and lwext4 cursors follow Linux open-file-description
    /// serialization. Slow I/O waiters sleep instead of occupying another CPU.
    inner: SleepLock<OSInodeInner>,
    open_key: Option<OpenInodeKey>,
}

unsafe impl Send for OSInode {}
unsafe impl Sync for OSInode {}

pub struct OSInodeInner {
    offset: usize,
    file_handle: Option<ext4_file>,
    dir_handle: Option<ext4_dir>,
    dir_cursor_index: usize,
    pending_dir_entry: Option<ext4_direntry>,
    pub lock_type: i16,
    append: bool,
    cached_stat: Option<Kstat>,
}

impl Drop for OSInode {
    fn drop(&mut self) {
        self.dentry.record_pending_close();
        {
            let mut inner = self.inner.exclusive_access();
            if let Some(ref mut file) = inner.file_handle {
                unsafe {
                    ext4_fclose(file);
                }
            }
            if let Some(ref mut dir) = inner.dir_handle {
                unsafe {
                    ext4_dir_close(dir);
                }
            }
        }
        if let Some(open_key) = self.open_key {
            if let Some(cleanup) = unregister_open_inode(open_key) {
                let runtime_ready = if cleanup.needs_flush {
                    flush_and_reset_inode_runtime(open_key.super_block, open_key.inode)
                } else {
                    // A final unlink has no remaining observer, so dirty data
                    // can be discarded without writing blocks that are about
                    // to be freed.
                    self.dentry.discard_pending_writes();
                    true
                };
                crate::mm::invalidate_file_page_cache(page_cache_key_from_open_key(open_key));
                // Preserve the hidden link if writeback failed. This keeps the
                // inode allocated until a later recovery path can flush it.
                if runtime_ready && self.dentry.super_block.unlink_file(&cleanup.orphan_path) {
                    if let (Some(parent), Some(orphan_name)) = (
                        dentry_parent(&self.dentry),
                        cleanup.orphan_path.rsplit('/').next(),
                    ) {
                        mark_cached_child_absent(&parent, orphan_name);
                    }
                }
            }
        }
    }
}

impl OSInode {
    fn file_page_cache_key(&self) -> Option<FilePageCacheKey> {
        self.open_key.map(page_cache_key_from_open_key)
    }

    fn begin_cached_write(&self) -> FilePageCacheWriteGuard {
        FilePageCacheWriteGuard::new(self.file_page_cache_key())
    }

    fn invalidate_page_cache_after_write(&self, offset: usize, len: usize, file_size: usize) {
        if let Some(key) = self.file_page_cache_key() {
            crate::mm::invalidate_file_page_cache_after_write(key, offset, len, file_size);
        }
    }

    fn refresh_handle_size(&self, file: &mut ext4_file) -> usize {
        let handle_size = unsafe { ext4_fsize(file) };
        let size = self.dentry.runtime_file_size().unwrap_or(handle_size);
        file.fsize = size;
        size as usize
    }

    pub fn get_dentry(&self) -> &Arc<Dentry> {
        &self.dentry
    }

    pub fn flush_pending_data(&self) -> bool {
        self.dentry.flush_pending_writes()
    }

    pub fn get_lock_type(&self) -> i16 {
        self.inner.exclusive_access().lock_type
    }

    pub fn set_lock_type(&self, t: i16) {
        self.inner.exclusive_access().lock_type = t;
    }

    pub fn new(readable: bool, writable: bool, append: bool, dentry: Arc<Dentry>) -> Self {
        Self::new_with_file_handle(readable, writable, append, dentry, None)
    }

    fn new_created(
        readable: bool,
        writable: bool,
        append: bool,
        dentry: Arc<Dentry>,
        mut file_handle: ext4_file,
    ) -> Self {
        file_handle.flags = match (readable, writable) {
            (true, true) => O_RDWR,
            (false, true) => O_WRONLY,
            _ => O_RDONLY,
        };
        if append {
            file_handle.flags |= O_APPEND;
            file_handle.fpos = file_handle.fsize;
        } else {
            file_handle.fpos = 0;
        }
        Self::new_with_file_handle(readable, writable, append, dentry, Some(file_handle))
    }

    fn new_with_file_handle(
        readable: bool,
        writable: bool,
        append: bool,
        dentry: Arc<Dentry>,
        file_handle: Option<ext4_file>,
    ) -> Self {
        if let Some(file) = file_handle {
            dentry.cache_file_handle(file);
        }
        let offset = append
            .then(|| dentry.runtime_file_size().unwrap_or(0) as usize)
            .unwrap_or(0);
        let key = open_inode_key(&dentry);
        let mut inode = Self {
            readable,
            writable,
            dentry,
            inner: SleepLock::new_named(
                OSInodeInner {
                    offset,
                    file_handle,
                    dir_handle: None,
                    dir_cursor_index: 0,
                    pending_dir_entry: None,
                    lock_type: 2,
                    append,
                    cached_stat: None,
                },
                "VfsOpenFileState",
            ),
            open_key: None,
        };
        if let Some(key) = key {
            if inode.open_handle() {
                register_open_inode(key);
                inode.open_key = Some(key);
            }
        }
        inode
    }
    pub fn dentry(&self) -> &Arc<Dentry> {
        &self.dentry
    }

    fn open_handle(&self) -> bool {
        let fs_path = &self.dentry.fs_path;
        let c_path = match CString::new(fs_path.as_bytes()) {
            Ok(s) => s,
            Err(_) => return false,
        };
        // File already exists (created by open_file_at/create_file).
        // Use "r+b" for writable files, "rb" for read-only.
        let flags = if self.writable { "r+b" } else { "rb" };
        let c_flags = match CString::new(flags) {
            Ok(s) => s,
            Err(_) => return false,
        };

        // The file-handle state is the outer VFS lock.  Keep it held while
        // opening so two harts cannot create two handles for one open file;
        // the superblock FsLock then serializes lwext4 itself.
        let mut inner = self.inner.exclusive_access();
        if inner.file_handle.is_some() {
            return true;
        }
        let offset = inner.offset;
        if let Some(mut file) = self.dentry.cached_file_handle() {
            file.flags = if self.writable { O_RDWR } else { O_RDONLY };
            if inner.append {
                file.flags |= O_APPEND;
            }
            file.fsize = self.dentry.runtime_file_size().unwrap_or(file.fsize);
            file.fpos = if file.flags & O_APPEND != 0 {
                file.fsize
            } else {
                offset as u64
            };
            inner.file_handle = Some(file);
            return true;
        }
        let mut file: ext4_file = unsafe { core::mem::zeroed() };
        let rc = {
            let _fs_lock = self.dentry.super_block.lock_fs();
            unsafe {
                let rc = ext4_fopen(
                    &mut file,
                    c_path.as_ptr() as *const core::ffi::c_char,
                    c_flags.as_ptr() as *const core::ffi::c_char,
                );
                if rc == EOK as i32 {
                    ext4_fseek(&mut file, offset as i64, SEEK_SET as u32);
                }
                rc
            }
        };
        if rc == EOK as i32 {
            self.dentry.cache_file_handle(file);
            inner.file_handle = Some(file);
            true
        } else {
            drop(inner);
            #[cfg(feature = "buildstorm-diagnostics")]
            crate::println!(
                "[buildstorm-fs] open-handle-failed rc={} writable={} path={}",
                rc,
                self.writable,
                fs_path,
            );
            false
        }
    }

    fn has_backing_handle(&self) -> bool {
        self.dentry.is_dir
            || self.dentry.is_link
            || self.inner.exclusive_access().file_handle.is_some()
    }

    pub fn read_all(&self) -> alloc::vec::Vec<u8> {
        if !self.dentry.flush_pending_writes() {
            return alloc::vec::Vec::new();
        }
        let mut inner = self.inner.exclusive_access();
        let size = self
            .dentry
            .inode_info()
            .map(|i| i.size as usize)
            .unwrap_or(0);
        let mut out = if size > 0 {
            alloc::vec::Vec::with_capacity(size)
        } else {
            alloc::vec::Vec::new()
        };
        let mut buffer = [0u8; 4096];
        loop {
            let len =
                self.dentry
                    .super_block
                    .read_at(&self.dentry.fs_path, inner.offset, &mut buffer);
            if len == 0 {
                break;
            }
            inner.offset += len;
            out.extend_from_slice(&buffer[..len]);
        }
        out
    }

    fn stat_inner(&self) -> Kstat {
        // Flush before taking the open-file lock so the global lock order stays
        // mutation/VFS -> file handle -> lwext4.  Once flushed, ext4_fsize is
        // the authoritative live length, including a Vim truncate.
        let pending_flushed = self.dentry.flush_pending_writes();
        let live_stat = {
            let mut inner = self.inner.exclusive_access();
            if let Some(mut st) = inner.cached_stat {
                if let Some(ref mut file) = inner.file_handle {
                    st.st_ino = file.inode as u64;
                    st.st_size = {
                        let _fs_lock = self.dentry.super_block.lock_fs();
                        let disk_size = unsafe { ext4_fsize(file) as i64 };
                        if pending_flushed {
                            disk_size
                        } else {
                            self.dentry
                                .runtime_file_size()
                                .map(|size| size as i64)
                                .unwrap_or(disk_size)
                        }
                    };
                    st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
                    inner.cached_stat = Some(st);
                    Some(st)
                } else if self.dentry.is_dir || (st.st_mode & S_IFREG) != S_IFREG {
                    return st;
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(st) = live_stat {
            self.dentry
                .update_local_cached_size(st.st_size.max(0) as u64);
            return st;
        }
        let mut st = stat_dentry(&self.dentry);
        if !self.dentry.is_dir {
            st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
        } else {
            st.st_nlink = st.st_nlink.max(2);
        }
        if !self.dentry.is_dir {
            self.inner.exclusive_access().cached_stat = Some(st);
        }
        st
    }

    pub fn update_local_cached_size(&self, size: i64) {
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut st) = inner.cached_stat {
            st.st_size = size;
            st.st_blocks = ((size.max(0) as u64) + 511) / 512;
        }
        self.dentry.update_local_cached_size(size.max(0) as u64);
    }

    pub fn refresh_after_truncate(&self, size: i64) {
        let _cache_write = self.begin_cached_write();
        let mut inner = self.inner.exclusive_access();
        if let Some(mut file) = inner.file_handle.take() {
            unsafe {
                ext4_fclose(&mut file);
            }
        }
        if let Some(ref mut st) = inner.cached_stat {
            st.st_size = size;
            st.st_blocks = ((size.max(0) as u64) + 511) / 512;
        }
        self.dentry.update_local_cached_size(size.max(0) as u64);
    }

    pub fn truncate(&self, size: u64) -> bool {
        if !self.writable || !self.open_handle() {
            return false;
        }
        let _file_mutation = self.dentry.lock_file_mutation();
        if !self.dentry.flush_pending_writes_during_mutation() {
            return false;
        }
        let _cache_write = self.begin_cached_write();
        let mut inner = self.inner.exclusive_access();
        let Some(ref mut file) = inner.file_handle else {
            return false;
        };
        let truncated = {
            let _fs_lock = self.dentry.super_block.lock_fs();
            let truncated = unsafe { ext4_ftruncate(file, size) == EOK as i32 };
            if truncated {
                self.dentry.commit_cached_size(size);
            }
            truncated
        };
        if !truncated {
            return false;
        }
        if let Some(ref mut st) = inner.cached_stat {
            st.st_size = size as i64;
            st.st_blocks = (size + 511) / 512;
        }
        true
    }

    pub fn update_cached_mode(&self, mode: u32) {
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut st) = inner.cached_stat {
            st.st_mode = mode;
        }
        self.dentry.update_cached_mode(mode);
    }

    pub fn update_cached_owner(&self, uid: u32, gid: u32, mode: u32, ctime: i64) {
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut st) = inner.cached_stat {
            st.st_uid = uid;
            st.st_gid = gid;
            st.st_mode = mode;
            st.st_ctime_sec = ctime;
        }
        self.dentry
            .update_cached_owner(uid, gid, mode, ctime.clamp(0, u32::MAX as i64) as u32);
    }

    pub fn update_cached_times(&self, atime: Option<i64>, mtime: Option<i64>, ctime: Option<i64>) {
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut st) = inner.cached_stat {
            if let Some(atime) = atime {
                st.st_atime_sec = atime;
            }
            if let Some(mtime) = mtime {
                st.st_mtime_sec = mtime;
            }
            if let Some(ctime) = ctime {
                st.st_ctime_sec = ctime;
            }
        }
        self.dentry.update_cached_times(
            atime.map(|v| v.clamp(0, u32::MAX as i64) as u32),
            mtime.map(|v| v.clamp(0, u32::MAX as i64) as u32),
            ctime.map(|v| v.clamp(0, u32::MAX as i64) as u32),
        );
    }

    fn now_sec() -> i64 {
        crate::time::realtime_sec()
    }

    fn update_cached_data_time(inner: &mut OSInodeInner, now_sec: i64) {
        if let Some(ref mut st) = inner.cached_stat {
            st.st_mtime_sec = now_sec;
            st.st_ctime_sec = now_sec;
        }
    }

    fn update_cached_path_data_time(&self, now_sec: i64) {
        let now = now_sec.clamp(0, u32::MAX as i64) as u32;
        update_path_cached_times(&self.dentry.fs_path, None, Some(now_sec), Some(now_sec));
        self.dentry.update_cached_times(None, Some(now), Some(now));
    }

    fn allocate_read_ahead(request_len: usize) -> Option<ReadAheadBuffer> {
        if request_len == 0 || request_len >= FILE_PAGE_READAHEAD_SIZE {
            return None;
        }
        let mut buffer = READ_AHEAD_POOL
            .exclusive_access()
            .take()
            .unwrap_or_default();
        buffer
            .try_reserve_exact(FILE_PAGE_READAHEAD_SIZE)
            .ok()?;
        buffer.resize(FILE_PAGE_READAHEAD_SIZE, 0);
        Some(ReadAheadBuffer(Some(buffer)))
    }

    fn read_ahead_offset(offset: usize, request_len: usize) -> usize {
        let aligned = offset & !(FILE_PAGE_READAHEAD_SIZE - 1);
        let within_window = offset - aligned;
        if request_len <= FILE_PAGE_READAHEAD_SIZE - within_window {
            aligned
        } else {
            offset
        }
    }

    fn read_handle_at(
        &self,
        file: &mut ext4_file,
        offset: usize,
        size: usize,
        buf: &mut [u8],
    ) -> usize {
        if offset >= size || buf.is_empty() {
            return 0;
        }
        let max_read = buf.len().min(size - offset);
        unsafe {
            ext4_fseek(file, offset as i64, SEEK_SET as u32);
        }
        let mut total = 0usize;
        while total < max_read {
            let mut rw_count: usize = 0;
            let chunk_len = max_read - total;
            #[cfg(feature = "smp-diagnostics")]
            let watched_before = crate::mm::watched_frame_is_intact();
            let rc = unsafe {
                ext4_fread(
                    file,
                    buf[total..max_read].as_mut_ptr() as *mut core::ffi::c_void,
                    chunk_len,
                    &mut rw_count,
                )
            };
            #[cfg(feature = "smp-diagnostics")]
            if watched_before && !crate::mm::watched_frame_is_intact() {
                crate::println!(
                    "[frame-diag] watched page changed during inode fread path={} off={:#x} requested={} returned={} rc={}",
                    self.dentry.fs_path,
                    offset + total,
                    chunk_len,
                    rw_count,
                    rc,
                );
            }
            if rc != EOK as i32 || rw_count == 0 || rw_count > chunk_len {
                break;
            }
            total += rw_count;
        }
        total
    }

    fn read_with_pending_overlay(
        &self,
        file: &mut ext4_file,
        offset: usize,
        buf: &mut [u8],
        read_len: usize,
        backing_size: usize,
    ) -> Option<usize> {
        if read_len == 0 {
            return Some(0);
        }
        let saved_pos = file.fpos;
        let saved_size = file.fsize;
        let backing_len = read_len.min(backing_size.saturating_sub(offset));
        buf[..read_len].fill(0);
        file.fsize = backing_size as u64;
        let loaded = if backing_len == 0 {
            0
        } else {
            let _fs_lock = self.dentry.super_block.lock_fs();
            self.read_handle_at(file, offset, backing_size, &mut buf[..backing_len])
        };
        file.fpos = saved_pos;
        file.fsize = saved_size;
        if loaded != backing_len
            || !self
                .dentry
                .overlay_pending_read(offset, &mut buf[..read_len])
        {
            return None;
        }
        Some(read_len)
    }

    fn next_dir_entry(inner: &mut OSInodeInner) -> Option<ext4_direntry> {
        let entry = {
            let dir = inner.dir_handle.as_mut()?;
            let ptr = unsafe { ext4_dir_entry_next(dir) };
            if ptr.is_null() {
                return None;
            }
            unsafe { *ptr }
        };
        inner.dir_cursor_index = inner.dir_cursor_index.saturating_add(1);
        Some(entry)
    }

    fn rewind_dir(inner: &mut OSInodeInner) {
        if let Some(dir) = inner.dir_handle.as_mut() {
            unsafe {
                ext4_dir_entry_rewind(dir);
            }
        }
        inner.dir_cursor_index = 0;
        inner.pending_dir_entry = None;
    }

    fn linux_dirent_type(de_type: u8) -> u8 {
        match de_type as u32 {
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

    pub fn align_up(value: usize, align: usize) -> usize {
        (value + align - 1) & !(align - 1)
    }

    fn extend_sparse(file: &mut ext4_file, size: usize) -> bool {
        unsafe { ext4_ftruncate(file, size as u64) == EOK as i32 }
    }
}

// ==================== OpenFlags ====================

bitflags! {
    pub struct OpenFlags: u32 {
        const RDONLY = 0;
        const WRONLY = 1 << 0;
        const RDWR = 1 << 1;
        const CREATE = 0x40;
        const EXCL = 0x80;
        const TRUNC = 0x200;
        const APPEND = 0x400;
        const DIRECTORY = 0x10000;
        const NOFOLLOW = 0x20000;
        const PATH = 0x200000;
    }
}

impl OpenFlags {
    pub fn from_raw(flags: u32) -> Self {
        Self::from_bits_truncate(flags)
    }

    pub fn create_requested(&self) -> bool {
        self.contains(Self::CREATE)
    }

    pub fn excl_requested(&self) -> bool {
        self.contains(Self::EXCL)
    }

    pub fn trunc_requested(&self) -> bool {
        self.contains(Self::TRUNC)
    }

    pub fn directory_requested(&self) -> bool {
        self.contains(Self::DIRECTORY)
    }

    pub fn nofollow_requested(&self) -> bool {
        self.contains(Self::NOFOLLOW)
    }

    pub fn path_requested(&self) -> bool {
        self.contains(Self::PATH)
    }

    pub fn read_write(&self) -> (bool, bool) {
        if self.contains(Self::PATH) {
            (false, false)
        } else if self.contains(Self::WRONLY) {
            (false, true)
        } else if self.contains(Self::RDWR) {
            (true, true)
        } else {
            (true, false)
        }
    }
}

// ==================== File Operations ====================

pub fn open_file(path: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    open_file_new(None, path, flags, 0o666, None)
}

pub fn open_file_at(
    base: Option<Arc<Dentry>>,
    path: &str,
    flags: OpenFlags,
    mode: u32,
) -> Option<Arc<OSInode>> {
    open_file_new(base, path, flags, mode, None)
}

pub(crate) fn open_file_at_with_status(
    base: Option<Arc<Dentry>>,
    path: &str,
    flags: OpenFlags,
    mode: u32,
) -> Option<(Arc<OSInode>, bool)> {
    let mut created = false;
    let inode = open_file_new(base, path, flags, mode, Some(&mut created))?;
    Some((inode, created))
}

fn open_file_new(
    base: Option<Arc<Dentry>>,
    path: &str,
    flags: OpenFlags,
    mode: u32,
    mut created_status: Option<&mut bool>,
) -> Option<Arc<OSInode>> {
    if let Some(status) = created_status.as_deref_mut() {
        *status = false;
    }
    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let want_dir = flags.directory_requested();
    #[cfg(feature = "buildstorm-diagnostics")]
    let diagnose_buildstorm =
        path.contains("/target/debug/") || start.fs_path.contains("/target/debug/");

    if flags.create_requested() {
        if want_dir {
            return None;
        }

        let (parent, name) = match vfs_resolve_parent_and_name(&start, path) {
            Some(parts) => parts,
            None => {
                #[cfg(feature = "buildstorm-diagnostics")]
                if diagnose_buildstorm {
                    crate::println!("[buildstorm-fs] create-parent-missing path={}", path);
                }
                return None;
            }
        };
        // Linux open creates or acquires the target inode atomically with
        // respect to rename/unlink in the parent directory. Keep the directory
        // mutation generation odd until the live lwext4 handle is registered.
        let _mutation = begin_cached_child_mutation(&parent);
        let mut existing = vfs_cached_lookup_during_mutation(&parent, &name);
        if existing.is_none() {
            if let Some((dentry, file_handle)) =
                vfs_create_open_file_during_mutation(&parent, &name, mode)
            {
                dentry.update_local_cached_size(0);
                invalidate_dentry_file_pages(&dentry);
                #[cfg(feature = "buildstorm-diagnostics")]
                if name == "invoked.timestamp" || name.ends_with("build-script-build-script-build") {
                    crate::println!(
                        "[buildstorm-fs] traced-open-created name={} inode={} dir={} path={}",
                        name,
                        dentry.inode_num,
                        dentry.is_dir,
                        dentry.fs_path,
                    );
                }
                let (readable, writable) = flags.read_write();
                let inode = Arc::new(OSInode::new_created(
                    readable,
                    writable,
                    flags.contains(OpenFlags::APPEND),
                    dentry,
                    file_handle,
                ));
                if let Some(status) = created_status.as_deref_mut() {
                    *status = true;
                }
                return inode.has_backing_handle().then_some(inode);
            }

            // A failed atomic create commonly means EEXIST. Resolve only on
            // this slow path so new build artifacts avoid a redundant inode
            // lookup while existing files preserve normal O_CREAT semantics.
            existing = vfs_lookup_during_mutation(&parent, &name);
        }
        #[cfg(feature = "buildstorm-diagnostics")]
        if name == "invoked.timestamp" || name.ends_with("build-script-build-script-build") {
            crate::println!(
                "[buildstorm-fs] traced-open-create name={} parent={} existing={:?}",
                name,
                parent.fs_path,
                existing
                    .as_ref()
                    .map(|dentry| (dentry.inode_num, dentry.is_dir, dentry.is_link)),
            );
        }
        if let Some(dentry) = existing.and_then(|d| {
            if flags.nofollow_requested() && d.is_link {
                if flags.path_requested() {
                    Some(d)
                } else {
                    None
                }
            } else {
                follow_symlink(&start, d, 0)
            }
        }) {
            if flags.excl_requested() || dentry.is_dir {
                #[cfg(feature = "buildstorm-diagnostics")]
                if diagnose_buildstorm {
                    crate::println!(
                        "[buildstorm-fs] create-existing-rejected excl={} dir={} inode={} path={}",
                        flags.excl_requested(),
                        dentry.is_dir,
                        dentry.inode_num,
                        dentry.fs_path,
                    );
                }
                return None;
            }
            if flags.trunc_requested() {
                let _file_mutation = dentry.lock_file_mutation();
                if !dentry.flush_pending_writes_during_mutation() {
                    return None;
                }
                let _cache_write = begin_dentry_cached_write(&dentry);
                let truncated = dentry.super_block.truncate_file(&dentry.fs_path, 0);
                if !truncated {
                    #[cfg(feature = "buildstorm-diagnostics")]
                    if diagnose_buildstorm {
                        crate::println!(
                            "[buildstorm-fs] create-existing-truncate-failed path={} resolved={}",
                            path,
                            dentry.fs_path,
                        );
                    }
                    return None;
                }
                dentry.update_local_cached_size(0);
            }
            let (readable, writable) = flags.read_write();
            let append = flags.contains(OpenFlags::APPEND);
            let inode = Arc::new(OSInode::new(readable, writable, append, dentry));
            return inode.has_backing_handle().then_some(inode);
        }
        #[cfg(feature = "buildstorm-diagnostics")]
        if diagnose_buildstorm {
            crate::println!("[buildstorm-fs] create-file-failed path={}", path);
        }
        return None;
    }

    let raw_dentry = vfs_resolve_with_fallback(&start, path)?;
    let dentry = if flags.nofollow_requested() && raw_dentry.is_link {
        if !flags.path_requested() {
            return None;
        }
        raw_dentry
    } else {
        follow_symlink(&start, raw_dentry, 0)?
    };
    if want_dir && !dentry.is_dir {
        return None;
    }

    if flags.trunc_requested() && !dentry.is_dir {
        let _file_mutation = dentry.lock_file_mutation();
        if !dentry.flush_pending_writes_during_mutation() {
            return None;
        }
        let _cache_write = begin_dentry_cached_write(&dentry);
        let truncated = dentry.super_block.truncate_file(&dentry.fs_path, 0);
        if !truncated {
            return None;
        }
        dentry.update_local_cached_size(0);
    }

    let (readable, writable) = if dentry.is_dir {
        (false, false)
    } else {
        flags.read_write()
    };
    let append = !dentry.is_dir && flags.contains(OpenFlags::APPEND);
    let inode = Arc::new(OSInode::new(readable, writable, append, dentry));
    inode.has_backing_handle().then_some(inode)
}

pub fn follow_symlink(
    start: &Arc<Dentry>,
    dentry: Arc<Dentry>,
    depth: usize,
) -> Option<Arc<Dentry>> {
    if depth >= 8 {
        return None;
    }
    if dentry.is_dir {
        return Some(dentry);
    }
    if !dentry.is_link {
        return Some(dentry);
    }
    let info = dentry.inode_info()?;
    if !info.is_link {
        return Some(dentry);
    }

    let target = read_dentry_symlink(&dentry)?;

    let base = if target.starts_with('/') {
        crate::task::current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone())
    } else {
        dentry_parent(&dentry).unwrap_or_else(|| start.clone())
    };
    let next = vfs_resolve_with_fallback(&base, target.as_str())?;
    follow_symlink(&base, next, depth + 1)
}

pub fn mkdir_at(base: Option<Arc<Dentry>>, path: &str, mode: u32) -> bool {
    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());

    let (parent, name) = match vfs_resolve_parent_and_name(&start, path) {
        Some(x) => x,
        None => return false,
    };

    vfs_create(&parent, &name, true, mode).is_some()
}

pub fn unlink_at_result(
    base: Option<Arc<Dentry>>,
    path: &str,
    is_dir: bool,
) -> Result<(), isize> {
    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let (parent, name) = match vfs_resolve_parent_and_name(&start, path) {
        Some(parts) => parts,
        None => return Err(-2),
    };

    // Resolve the target only after owning the parent mutation generation.
    // Two concurrent unlink calls then observe either the current inode or a
    // real ENOENT, rather than both operating on one stale positive dentry.
    let _mutation = begin_cached_child_mutation(&parent);
    let dentry = match vfs_lookup_during_mutation(&parent, &name) {
        Some(dentry) => dentry,
        None => return Err(-2),
    };
    if dentry.is_dir && !is_dir {
        return Err(-21);
    }
    if !dentry.is_dir && is_dir {
        return Err(-20);
    }

    let final_file_unlink = !is_dir
        && dentry
            .inode_info()
            .is_some_and(|info| info.links_count <= 1);
    let _file_mutation = final_file_unlink
        .then(|| dentry.lock_file_mutation())
        .flatten();
    let _cache_write = (!is_dir).then(|| begin_dentry_cached_write(&dentry));
    let mut discard_pending = false;
    let mut unlink_error = -1;
    let ok = if is_dir {
        let rc = dentry.super_block.unlink_child(
            parent.inode_num,
            dentry.inode_num,
            &dentry.name,
            true,
        );
        unlink_error = -(rc as isize);
        rc == EOK as i32
    } else if let Some(deferred) = defer_open_file_unlink(&dentry) {
        deferred
    } else {
        let rc = dentry.super_block.unlink_child(
            parent.inode_num,
            dentry.inode_num,
            &dentry.name,
            false,
        );
        unlink_error = -(rc as isize);
        let unlinked = rc == EOK as i32;
        discard_pending = unlinked && final_file_unlink;
        unlinked
    };

    if ok {
        if discard_pending {
            dentry.discard_pending_writes_during_mutation();
        }
        invalidate_cached_child_during_mutation(&parent, &dentry.name);
        Ok(())
    } else {
        Err(unlink_error)
    }
}

pub fn unlink_at(base: Option<Arc<Dentry>>, path: &str, is_dir: bool) -> bool {
    unlink_at_result(base, path, is_dir).is_ok()
}

// ==================== File Trait for OSInode ====================

impl File for OSInode {
    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read(&self, buf: &mut [u8]) -> isize {
        if !self.readable {
            return -1;
        }
        let file_mutation = self.dentry.lock_file_mutation();
        let pending_read = {
            let mut inner = self.inner.exclusive_access();
            let off = inner.offset;
            self.dentry.read_pending_fully(off, buf).map(|read_len| {
                inner.offset = off.saturating_add(read_len);
                read_len
            })
        };
        if let Some(read_len) = pending_read {
            return read_len as isize;
        }
        if self.dentry.has_pending_writes() {
            let need_open = self.inner.exclusive_access().file_handle.is_none();
            if need_open {
                self.open_handle();
            }
            let mut inner = self.inner.exclusive_access();
            let off = inner.offset;
            if let Some((read_len, backing_size)) =
                self.dentry.pending_read_plan(off, buf.len())
            {
                if let Some(mut file) = inner.file_handle {
                    if let Some(read_len) = self.read_with_pending_overlay(
                        &mut file,
                        off,
                        buf,
                        read_len,
                        backing_size,
                    ) {
                        inner.offset = off.saturating_add(read_len);
                        file.fpos = inner.offset as u64;
                        file.fsize = self.dentry.runtime_file_size().unwrap_or(file.fsize);
                        inner.file_handle = Some(file);
                        return read_len as isize;
                    }
                }
            }
        }
        if !self.dentry.flush_pending_writes_during_mutation() {
            return -5;
        }
        // Page-cache waits must not hold the inode mutation lock: another
        // task may own the in-flight load and need this lock to complete it.
        drop(file_mutation);
        let need_open = {
            let inner = self.inner.exclusive_access();
            inner.file_handle.is_none()
        };
        if need_open {
            self.open_handle();
        }
        let mut inner = self.inner.exclusive_access();
        let off = inner.offset;
        let mut cache_miss = None;
        let mut _cache_load = None;
        if let Some(key) = self.file_page_cache_key() {
            #[cfg(feature = "lock-stats")]
            let mut recorded_miss = false;
            loop {
                match read_file_page_cache(key, off, buf) {
                    FilePageCacheRead::Hit(read_len) => {
                        #[cfg(feature = "lock-stats")]
                        crate::mm::record_file_page_cache_hit(read_len);
                        inner.offset = off + read_len;
                        return read_len as isize;
                    }
                    FilePageCacheRead::Miss(_) => {
                        #[cfg(feature = "lock-stats")]
                        {
                            if !recorded_miss {
                                crate::mm::record_file_page_cache_miss(buf.len());
                            }
                            recorded_miss = true;
                        }
                    }
                }
                match reserve_file_page_cache_read(key, off, buf.len()) {
                    FilePageCacheReadLoad::Retry => continue,
                    FilePageCacheReadLoad::Bypass(generation) => {
                        cache_miss = Some((key, generation));
                        break;
                    }
                    FilePageCacheReadLoad::Wait(state) => {
                        wait_for_file_page_load(&state);
                    }
                    FilePageCacheReadLoad::Load(load) => {
                        cache_miss = Some((key, load.generation()));
                        _cache_load = Some(load);
                        break;
                    }
                }
            }
        }
        let mut read_ahead = cache_miss
            .is_some()
            .then(|| Self::allocate_read_ahead(buf.len()))
            .flatten();
        if let Some(ref mut file) = inner.file_handle {
            let _fs_lock = self.dentry.super_block.lock_fs();
            let size = self.refresh_handle_size(file);
            if off >= size {
                return 0;
            }
            let mut loaded_offset = off;
            let mut used_read_ahead = false;
            let (loaded_len, returned) = if let Some(read_ahead) = read_ahead.as_mut() {
                used_read_ahead = true;
                loaded_offset = Self::read_ahead_offset(off, buf.len());
                let loaded_len = self.read_handle_at(file, loaded_offset, size, read_ahead);
                let source_offset = off - loaded_offset;
                let returned = loaded_len
                    .saturating_sub(source_offset)
                    .min(buf.len())
                    .min(size - off);
                buf[..returned]
                    .copy_from_slice(&read_ahead[source_offset..source_offset + returned]);
                (loaded_len, returned)
            } else {
                let loaded_len = self.read_handle_at(file, off, size, buf);
                (loaded_len, loaded_len)
            };
            inner.offset = off + returned;
            drop(_fs_lock);
            drop(inner);
            if let Some((key, generation)) = cache_miss {
                if used_read_ahead {
                    cache_file_read(
                        key,
                        generation,
                        loaded_offset,
                        &read_ahead
                            .as_ref()
                            .expect("read-ahead source remains allocated")[..loaded_len],
                        size,
                    );
                } else {
                    cache_file_read(key, generation, off, &buf[..loaded_len], size);
                }
            }
            return returned as isize;
        }
        drop(inner);
        let mut inner = self.inner.exclusive_access();
        let rw_count = self
            .dentry
            .super_block
            .read_at(&self.dentry.fs_path, inner.offset, buf);
        inner.offset += rw_count;
        rw_count as isize
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        if !self.readable {
            return -1;
        }
        let file_mutation = self.dentry.lock_file_mutation();
        if let Some(read_len) = self.dentry.read_pending_fully(offset, buf) {
            return read_len as isize;
        }
        if self.dentry.has_pending_writes() {
            let need_open = self.inner.exclusive_access().file_handle.is_none();
            if need_open {
                self.open_handle();
            }
            if let Some((read_len, backing_size)) =
                self.dentry.pending_read_plan(offset, buf.len())
            {
                let mut file = self.inner.exclusive_access().file_handle;
                if let Some(ref mut file) = file {
                    if let Some(read_len) = self.read_with_pending_overlay(
                        file,
                        offset,
                        buf,
                        read_len,
                        backing_size,
                    ) {
                        return read_len as isize;
                    }
                }
            }
        }
        if !self.dentry.flush_pending_writes_during_mutation() {
            return -5;
        }
        drop(file_mutation);
        let need_open = self.inner.exclusive_access().file_handle.is_none();
        if need_open {
            self.open_handle();
        }
        let mut file = self.inner.exclusive_access().file_handle;
        let mut cache_miss = None;
        let mut _cache_load = None;
        if let Some(key) = self.file_page_cache_key() {
            #[cfg(feature = "lock-stats")]
            let mut recorded_miss = false;
            loop {
                match read_file_page_cache(key, offset, buf) {
                    FilePageCacheRead::Hit(read_len) => {
                        #[cfg(feature = "lock-stats")]
                        crate::mm::record_file_page_cache_hit(read_len);
                        return read_len as isize;
                    }
                    FilePageCacheRead::Miss(_) => {
                        #[cfg(feature = "lock-stats")]
                        {
                            if !recorded_miss {
                                crate::mm::record_file_page_cache_miss(buf.len());
                            }
                            recorded_miss = true;
                        }
                    }
                }
                match reserve_file_page_cache_read(key, offset, buf.len()) {
                    FilePageCacheReadLoad::Retry => continue,
                    FilePageCacheReadLoad::Bypass(generation) => {
                        cache_miss = Some((key, generation));
                        break;
                    }
                    FilePageCacheReadLoad::Wait(state) => {
                        wait_for_file_page_load(&state);
                    }
                    FilePageCacheReadLoad::Load(load) => {
                        cache_miss = Some((key, load.generation()));
                        _cache_load = Some(load);
                        break;
                    }
                }
            }
        }
        let mut read_ahead = cache_miss
            .is_some()
            .then(|| Self::allocate_read_ahead(buf.len()))
            .flatten();

        // ext4_file is a value-type cursor. pread must not hold the shared
        // open-file state while waiting for lwext4 or performing disk I/O;
        // each call can safely seek and advance its private cursor copy.
        if let Some(ref mut file) = file {
            let _fs_lock = self.dentry.super_block.lock_fs();
            let size = self.refresh_handle_size(file);
            if offset >= size {
                return 0;
            }
            let mut loaded_offset = offset;
            let mut used_read_ahead = false;
            let (loaded_len, returned) = if let Some(read_ahead) = read_ahead.as_mut() {
                used_read_ahead = true;
                loaded_offset = Self::read_ahead_offset(offset, buf.len());
                let loaded_len = self.read_handle_at(file, loaded_offset, size, read_ahead);
                let source_offset = offset - loaded_offset;
                let returned = loaded_len
                    .saturating_sub(source_offset)
                    .min(buf.len())
                    .min(size - offset);
                buf[..returned]
                    .copy_from_slice(&read_ahead[source_offset..source_offset + returned]);
                (loaded_len, returned)
            } else {
                let loaded_len = self.read_handle_at(file, offset, size, buf);
                (loaded_len, loaded_len)
            };
            drop(_fs_lock);
            if let Some((key, generation)) = cache_miss {
                if used_read_ahead {
                    cache_file_read(
                        key,
                        generation,
                        loaded_offset,
                        &read_ahead
                            .as_ref()
                            .expect("read-ahead source remains allocated")[..loaded_len],
                        size,
                    );
                } else {
                    cache_file_read(key, generation, offset, &buf[..loaded_len], size);
                }
            }
            return returned as isize;
        }
        self.dentry
            .super_block
            .read_at(&self.dentry.fs_path, offset, buf) as isize
    }

    fn write_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        if !self.writable {
            return -1;
        }
        if buf.is_empty() {
            return 0;
        }
        let file_mutation = self.dentry.lock_file_mutation();
        let need_open = {
            let inner = self.inner.exclusive_access();
            inner.file_handle.is_none()
        };
        if need_open {
            self.open_handle();
        }
        let mut inner = self.inner.exclusive_access();
        let saved_offset = inner.offset;
        let now_sec = Self::now_sec();
        let write_time = now_sec.clamp(0, u32::MAX as i64) as u32;
        if let Some(ref mut file) = inner.file_handle {
            let size = self.refresh_handle_size(file);
            let Some(requested_new_size) = offset.checked_add(buf.len()) else {
                return -27;
            };
            let requested_new_size = requested_new_size.max(size);
            if !vfs_tmpfs_can_resize(
                &self.dentry.path,
                &self.dentry.fs_path,
                size as u64,
                requested_new_size as u64,
            ) {
                return -28;
            }
            if offset <= size {
                if let Some((flush_inode, apply_pressure)) = self.dentry.buffer_pending_write(
                    *file,
                    offset,
                    buf,
                    write_time,
                ) {
                    file.fsize = requested_new_size as u64;
                    self.dentry.commit_cached_size(requested_new_size as u64);
                    if let Some(ref mut st) = inner.cached_stat {
                        st.st_size = requested_new_size as i64;
                        st.st_blocks = (requested_new_size as u64 + 511) / 512;
                    }
                    Self::update_cached_data_time(&mut inner, now_sec);
                    drop(inner);
                    self.invalidate_page_cache_after_write(
                        offset,
                        buf.len(),
                        requested_new_size,
                    );
                    self.update_cached_path_data_time(now_sec);
                    crate::fs::record_block_write_bytes(buf.len());
                    vfs_tmpfs_commit_resize(
                        &self.dentry.path,
                        &self.dentry.fs_path,
                        size as u64,
                        requested_new_size as u64,
                    );
                    if flush_inode {
                        let _ = self.dentry.flush_pending_writes_during_mutation();
                    }
                    drop(file_mutation);
                    if apply_pressure {
                        let _ = self.dentry.balance_pending_writes();
                    }
                    return buf.len() as isize;
                }
            }
            if !self.dentry.flush_pending_writes_during_mutation() {
                return -5;
            }
            let mut fs_lock = Some(self.dentry.super_block.lock_fs());
            let size = self.refresh_handle_size(file);
            if offset > size && !Self::extend_sparse(file, offset) {
                return -1;
            }
            unsafe {
                ext4_fseek(file, offset as i64, SEEK_SET as u32);
            }
            let mut total = 0usize;
            while total < buf.len() {
                if total != 0 {
                    drop(fs_lock.take());
                    fs_lock = Some(self.dentry.super_block.lock_fs());
                }
                let chunk_limit = if self.dentry.super_block.fs_has_waiters() {
                    EXT4_WRITE_LOCK_CHUNK_SIZE
                } else {
                    EXT4_WRITE_UNCONTENDED_CHUNK_SIZE
                };
                let chunk_len = (buf.len() - total).min(chunk_limit);
                let mut rw_count: usize = 0;
                let rc = unsafe {
                    ext4_fwrite_with_times(
                        file,
                        buf[total..total + chunk_len].as_ptr()
                            as *const core::ffi::c_void,
                        chunk_len,
                        &mut rw_count,
                        write_time,
                        write_time,
                    )
                };
                if rc != EOK as i32 || rw_count == 0 {
                    #[cfg(feature = "buildstorm-diagnostics")]
                    if self.dentry.fs_path.contains("/target/debug/") {
                        crate::println!(
                            "[buildstorm-fs] pwrite-failed rc={} off={} len={} written={} path={}",
                            rc,
                            offset + total,
                            chunk_len,
                            rw_count,
                            self.dentry.fs_path,
                        );
                    }
                    break;
                }
                total += rw_count;
            }
            unsafe {
                ext4_fseek(file, saved_offset as i64, SEEK_SET as u32);
            }
            let new_size = unsafe { ext4_fsize(file) as i64 };
            self.dentry.commit_cached_size(new_size.max(0) as u64);
            drop(fs_lock);
            if let Some(ref mut st) = inner.cached_stat {
                st.st_size = new_size;
                st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
            }
            if total > 0 {
                Self::update_cached_data_time(&mut inner, now_sec);
                drop(inner);
                self.invalidate_page_cache_after_write(
                    offset,
                    total,
                    new_size.max(0) as usize,
                );
                self.update_cached_path_data_time(now_sec);
                crate::fs::record_block_write_bytes(total);
                vfs_tmpfs_commit_resize(
                    &self.dentry.path,
                    &self.dentry.fs_path,
                    size as u64,
                    new_size.max(0) as u64,
                );
                return total as isize;
            }
            if total == 0 && !buf.is_empty() {
                inner.file_handle = None;
                drop(inner);
                let written = self
                    .dentry
                    .super_block
                    .write_at(&self.dentry.fs_path, offset, buf, write_time);
                if written > 0 {
                    let committed_size = (offset + written).max(new_size.max(0) as usize);
                    self.invalidate_page_cache_after_write(
                        offset,
                        written,
                        committed_size,
                    );
                    crate::fs::record_block_write_bytes(written);
                    self.update_local_cached_size(committed_size as i64);
                    self.update_cached_times(None, Some(now_sec), Some(now_sec));
                    self.update_cached_path_data_time(now_sec);
                    vfs_tmpfs_commit_resize(
                        &self.dentry.path,
                        &self.dentry.fs_path,
                        size as u64,
                        committed_size as u64,
                    );
                }
                return written as isize;
            }
            return total as isize;
        }
        if !self.dentry.flush_pending_writes_during_mutation() {
            return -5;
        }
        let old_size = self
            .dentry
            .inode_info()
            .map(|info| info.size as usize)
            .unwrap_or(0);
        let Some(requested_new_size) = offset.checked_add(buf.len()) else {
            return -27;
        };
        let requested_new_size = requested_new_size.max(old_size);
        if !vfs_tmpfs_can_resize(
            &self.dentry.path,
            &self.dentry.fs_path,
            old_size as u64,
            requested_new_size as u64,
        ) {
            return -28;
        }
        drop(inner);
        let written = self
            .dentry
            .super_block
            .write_at(&self.dentry.fs_path, offset, buf, write_time);
        if written > 0 {
            crate::fs::record_block_write_bytes(written);
            let new_size = old_size.max(offset + written);
            self.update_local_cached_size(new_size as i64);
            self.invalidate_page_cache_after_write(offset, written, new_size);
            self.update_cached_times(None, Some(now_sec), Some(now_sec));
            self.update_cached_path_data_time(now_sec);
            vfs_tmpfs_commit_resize(
                &self.dentry.path,
                &self.dentry.fs_path,
                old_size as u64,
                new_size as u64,
            );
        }
        written as isize
    }

    fn write(&self, buf: &mut [u8]) -> isize {
        if !self.writable {
            return -1;
        }
        if buf.is_empty() {
            return 0;
        }
        let file_mutation = self.dentry.lock_file_mutation();
        let need_open = {
            let inner = self.inner.exclusive_access();
            inner.file_handle.is_none()
        };
        if need_open {
            self.open_handle();
        }
        let mut inner = self.inner.exclusive_access();
        let append = inner.append;
        let cur_offset = inner.offset;
        let now_sec = Self::now_sec();
        let write_time = now_sec.clamp(0, u32::MAX as i64) as u32;
        if let Some(ref mut file) = inner.file_handle {
            let size = self.refresh_handle_size(file);
            let off = if append { size } else { cur_offset };
            let Some(requested_new_size) = off.checked_add(buf.len()) else {
                return -27;
            };
            let requested_new_size = requested_new_size.max(size);
            if !vfs_tmpfs_can_resize(
                &self.dentry.path,
                &self.dentry.fs_path,
                size as u64,
                requested_new_size as u64,
            ) {
                return -28;
            }
            if off <= size {
                if let Some((flush_inode, apply_pressure)) = self.dentry.buffer_pending_write(
                    *file,
                    off,
                    buf,
                    write_time,
                ) {
                    file.fsize = requested_new_size as u64;
                    file.fpos = off.saturating_add(buf.len()) as u64;
                    self.dentry.commit_cached_size(requested_new_size as u64);
                    inner.offset = off + buf.len();
                    if let Some(ref mut st) = inner.cached_stat {
                        st.st_size = requested_new_size as i64;
                        st.st_blocks = (requested_new_size as u64 + 511) / 512;
                    }
                    Self::update_cached_data_time(&mut inner, now_sec);
                    drop(inner);
                    self.invalidate_page_cache_after_write(
                        off,
                        buf.len(),
                        requested_new_size,
                    );
                    self.update_cached_path_data_time(now_sec);
                    crate::fs::record_block_write_bytes(buf.len());
                    vfs_tmpfs_commit_resize(
                        &self.dentry.path,
                        &self.dentry.fs_path,
                        size as u64,
                        requested_new_size as u64,
                    );
                    if flush_inode {
                        let _ = self.dentry.flush_pending_writes_during_mutation();
                    }
                    drop(file_mutation);
                    if apply_pressure {
                        let _ = self.dentry.balance_pending_writes();
                    }
                    return buf.len() as isize;
                }
            }
            if !self.dentry.flush_pending_writes_during_mutation() {
                return -5;
            }
            let mut fs_lock = Some(self.dentry.super_block.lock_fs());
            let size = self.refresh_handle_size(file);
            let off = if append { size } else { cur_offset };
            if off > size && !Self::extend_sparse(file, off) {
                inner.offset = unsafe { ext4_fsize(file) as usize };
                return -1;
            }
            unsafe {
                ext4_fseek(file, off as i64, SEEK_SET as u32);
            }
            let mut total = 0usize;
            while total < buf.len() {
                if total != 0 {
                    drop(fs_lock.take());
                    fs_lock = Some(self.dentry.super_block.lock_fs());
                }
                let chunk_limit = if self.dentry.super_block.fs_has_waiters() {
                    EXT4_WRITE_LOCK_CHUNK_SIZE
                } else {
                    EXT4_WRITE_UNCONTENDED_CHUNK_SIZE
                };
                let chunk_len = (buf.len() - total).min(chunk_limit);
                let mut rw_count: usize = 0;
                let rc = unsafe {
                    ext4_fwrite_with_times(
                        file,
                        buf[total..total + chunk_len].as_ptr()
                            as *const core::ffi::c_void,
                        chunk_len,
                        &mut rw_count,
                        write_time,
                        write_time,
                    )
                };
                if rc != EOK as i32 || rw_count == 0 {
                    #[cfg(feature = "buildstorm-diagnostics")]
                    if self.dentry.fs_path.contains("/target/debug/") {
                        crate::println!(
                            "[buildstorm-fs] write-failed rc={} off={} len={} written={} path={}",
                            rc,
                            off + total,
                            chunk_len,
                            rw_count,
                            self.dentry.fs_path,
                        );
                    }
                    break;
                }
                total += rw_count;
            }
            let new_size = unsafe { ext4_fsize(file) as i64 };
            self.dentry.commit_cached_size(new_size.max(0) as u64);
            drop(fs_lock);
            inner.offset = off + total;
            if let Some(ref mut st) = inner.cached_stat {
                st.st_size = new_size;
                st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
            }
            if total > 0 {
                Self::update_cached_data_time(&mut inner, now_sec);
                drop(inner);
                self.invalidate_page_cache_after_write(
                    off,
                    total,
                    new_size.max(0) as usize,
                );
                self.update_cached_path_data_time(now_sec);
                crate::fs::record_block_write_bytes(total);
                vfs_tmpfs_commit_resize(
                    &self.dentry.path,
                    &self.dentry.fs_path,
                    size as u64,
                    new_size.max(0) as u64,
                );
                return total as isize;
            }
            if total == 0 && !buf.is_empty() {
                inner.file_handle = None;
                drop(inner);
                let rw_count = self
                    .dentry
                    .super_block
                    .write_at(&self.dentry.fs_path, off, buf, write_time);
                let committed_size = size.max(off.saturating_add(rw_count));
                let mut inner = self.inner.exclusive_access();
                inner.offset = off + rw_count;
                if let Some(ref mut st) = inner.cached_stat {
                    st.st_size = committed_size as i64;
                    st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
                }
                self.dentry
                    .update_local_cached_size(committed_size as u64);
                if rw_count > 0 {
                    Self::update_cached_data_time(&mut inner, now_sec);
                    drop(inner);
                    self.invalidate_page_cache_after_write(
                        off,
                        rw_count,
                        committed_size,
                    );
                    self.update_cached_path_data_time(now_sec);
                    crate::fs::record_block_write_bytes(rw_count);
                    vfs_tmpfs_commit_resize(
                        &self.dentry.path,
                        &self.dentry.fs_path,
                        size as u64,
                        committed_size as u64,
                    );
                }
                return rw_count as isize;
            }
            return total as isize;
        }
        if !self.dentry.flush_pending_writes_during_mutation() {
            return -5;
        }
        let off = if inner.append {
            self.dentry
                .inode_info()
                .map(|i| i.size as usize)
                .unwrap_or(inner.offset)
        } else {
            inner.offset
        };
        let old_size = self
            .dentry
            .inode_info()
            .map(|info| info.size as usize)
            .unwrap_or(0);
        let Some(requested_new_size) = off.checked_add(buf.len()) else {
            return -27;
        };
        let requested_new_size = requested_new_size.max(old_size);
        if !vfs_tmpfs_can_resize(
            &self.dentry.path,
            &self.dentry.fs_path,
            old_size as u64,
            requested_new_size as u64,
        ) {
            return -28;
        }
        drop(inner);
        let rw_count = self
            .dentry
            .super_block
            .write_at(&self.dentry.fs_path, off, buf, write_time);
        let committed_size = old_size.max(off.saturating_add(rw_count));
        let mut inner = self.inner.exclusive_access();
        inner.offset = off + rw_count;
        if let Some(ref mut st) = inner.cached_stat {
            st.st_size = committed_size as i64;
            st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
        }
        self.dentry
            .update_local_cached_size(committed_size as u64);
        if rw_count > 0 {
            Self::update_cached_data_time(&mut inner, now_sec);
            drop(inner);
            self.invalidate_page_cache_after_write(
                off,
                rw_count,
                committed_size,
            );
            self.update_cached_path_data_time(now_sec);
            crate::fs::record_block_write_bytes(rw_count);
            vfs_tmpfs_commit_resize(
                &self.dentry.path,
                &self.dentry.fs_path,
                old_size as u64,
                committed_size as u64,
            );
        }
        rw_count as isize
    }

    fn stat(&self) -> Kstat {
        self.stat_inner()
    }

    fn is_dir(&self) -> bool {
        self.dentry.is_dir
    }

    fn path(&self) -> Option<String> {
        Some(self.dentry.path.clone())
    }

    fn page_cache_key(&self) -> Option<FilePageCacheKey> {
        self.file_page_cache_key()
    }

    fn getdents64(&self, buf: &mut [u8]) -> isize {
        const DIRENT64_FIXED_SIZE: usize = 8 + 8 + 2 + 1;
        if !self.dentry.is_dir {
            return -20;
        }
        let c_path = match CString::new(self.dentry.fs_path.as_bytes()) {
            Ok(path) => path,
            Err(_) => return -22,
        };
        let cache_epoch = self.dentry.child_cache_snapshot_epoch();
        let mut inner = self.inner.exclusive_access();
        let fs_lock = self.dentry.super_block.lock_fs();

        if inner.dir_handle.is_none() {
            let mut dir: ext4_dir = unsafe { core::mem::zeroed() };
            let rc = unsafe {
                ext4_dir_open(
                    &mut dir,
                    c_path.as_ptr() as *const core::ffi::c_char,
                )
            };
            if rc != EOK as i32 {
                return -(rc as isize);
            }
            inner.dir_handle = Some(dir);
            inner.dir_cursor_index = 0;
            inner.pending_dir_entry = None;
        }

        let expected_cursor = inner
            .offset
            .saturating_add(usize::from(inner.pending_dir_entry.is_some()));
        if inner.dir_cursor_index != expected_cursor {
            Self::rewind_dir(&mut inner);
        }
        while inner.dir_cursor_index < inner.offset {
            if Self::next_dir_entry(&mut inner).is_none() {
                return 0;
            }
        }

        let mut written = 0usize;
        loop {
            let entry = match inner
                .pending_dir_entry
                .take()
                .or_else(|| Self::next_dir_entry(&mut inner))
            {
                Some(entry) => entry,
                None => break,
            };
            let name_len = (entry.name_length as usize).min(entry.name.len());
            let name_bytes = &entry.name[..name_len];
            if name_bytes.is_empty()
                || name_bytes.starts_with(b".tmpfile.")
                || name_bytes.starts_with(UNLINKED_FILE_PREFIX.as_bytes())
            {
                inner.offset = inner.offset.saturating_add(1);
                continue;
            }
            let record_size = Self::align_up(DIRENT64_FIXED_SIZE + name_bytes.len() + 1, 8);
            if written + record_size > buf.len() {
                inner.pending_dir_entry = Some(entry);
                if written == 0 {
                    return -1;
                }
                break;
            }

            let record = &mut buf[written..written + record_size];
            record.fill(0);

            record[0..8].copy_from_slice(&(entry.inode as u64).to_le_bytes());
            let d_off = (inner.offset as i64) + 1;
            record[8..16].copy_from_slice(&d_off.to_le_bytes());
            record[16..18].copy_from_slice(&(record_size as u16).to_le_bytes());
            record[18] = Self::linux_dirent_type(entry.inode_type);

            let name_start = DIRENT64_FIXED_SIZE;
            record[name_start..name_start + name_bytes.len()].copy_from_slice(name_bytes);
            record[name_start + name_bytes.len()] = 0;

            written += record_size;
            inner.offset = inner.offset.saturating_add(1);
        }
        drop(fs_lock);
        drop(inner);
        if let Some(epoch) = cache_epoch {
            cache_readdir_entries(&self.dentry, &buf[..written], epoch);
        }
        written as isize
    }

    fn seek(&self, offset: usize) {
        let mut inner = self.inner.exclusive_access();
        inner.offset = offset;
        if self.dentry.is_dir {
            let _fs_lock = self.dentry.super_block.lock_fs();
            Self::rewind_dir(&mut inner);
        }
        // Regular-file I/O seeks its private lwext4 cursor immediately before
        // each read or write. The logical file offset is authoritative, so an
        // eager ext4_fseek here only serializes lseek-heavy workloads on FsLock.
    }

    fn get_offset(&self) -> usize {
        self.inner.exclusive_access().offset
    }

    fn get_flags(&self) -> usize {
        let inner = self.inner.exclusive_access();
        let mut flags = 0x8000usize;
        if self.readable && self.writable {
            flags |= OpenFlags::RDWR.bits() as usize;
        } else if self.writable {
            flags |= OpenFlags::WRONLY.bits() as usize;
        }
        if inner.append {
            flags |= OpenFlags::APPEND.bits() as usize;
        }
        flags
    }

    fn set_flags(&self, flags: usize) -> bool {
        let mut inner = self.inner.exclusive_access();
        inner.append = (flags & OpenFlags::APPEND.bits() as usize) != 0;
        true
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ==================== rename_at ====================

pub fn rename_at(
    old_base: Option<Arc<Dentry>>,
    old_path: &str,
    new_base: Option<Arc<Dentry>>,
    new_path: &str,
    flags: u32,
) -> Result<(), isize> {
    use lwext4_rust::bindings::{EOK, EXT4_RENAME_NOREPLACE};

    const RENAME_NOREPLACE: u32 = 1;

    if flags & !RENAME_NOREPLACE != 0 {
        return Err(-22);
    }

    let start = old_base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    #[cfg(feature = "buildstorm-diagnostics")]
    let diagnose_buildstorm = old_path.contains("/target/debug/")
        || new_path.contains("/target/debug/")
        || start.fs_path.contains("/target/debug/");
    // Keep the source parent alive independently of the dentry cache.  A child
    // stores only a Weak parent link, so relying on dentry_parent() can turn a
    // cache eviction into a spurious EBUSY during a perfectly valid rename.
    let (old_parent, old_name) = match vfs_resolve_parent_and_name(&start, old_path) {
        Some(parts) => parts,
        None => return Err(-2),
    };
    let old_dentry = match vfs_resolve_path(&start, old_path) {
        Some(d) => d,
        None => {
            #[cfg(feature = "buildstorm-diagnostics")]
            if diagnose_buildstorm {
                crate::println!(
                    "[buildstorm-fs] rename-source-missing old={} new={}",
                    old_path,
                    new_path,
                );
            }
            return Err(-2);
        }
    };

    let new_start = new_base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let (new_parent, new_name) = match vfs_resolve_parent_and_name(&new_start, new_path) {
        Some(x) => x,
        None => {
            #[cfg(feature = "buildstorm-diagnostics")]
            if diagnose_buildstorm || new_start.fs_path.contains("/target/debug/") {
                crate::println!(
                    "[buildstorm-fs] rename-parent-missing old={} new={}",
                    old_path,
                    new_path,
                );
            }
            return Err(-2);
        }
    };

    if !Arc::ptr_eq(&old_dentry.super_block, &new_parent.super_block) {
        return Err(-18);
    }

    #[cfg(feature = "buildstorm-diagnostics")]
    let new_fs_path = if new_parent.fs_path == "/" {
        alloc::format!("/{}", new_name)
    } else if new_parent.fs_path.ends_with('/') {
        alloc::format!("{}{}", new_parent.fs_path, new_name)
    } else {
        alloc::format!("{}/{}", new_parent.fs_path, new_name)
    };

    let orphan_fs_path = replacement_orphan_path(&new_parent);
    let orphan_name = match orphan_fs_path.rsplit('/').next() {
        Some(name) if !name.is_empty() => name,
        _ => return Err(-22),
    };

    let source_inode = old_dentry.inode_num;
    if old_dentry.is_dir {
        let mut ancestor = Some(new_parent.clone());
        while let Some(candidate) = ancestor {
            let candidate_inode = candidate.inode_num;
            if Arc::ptr_eq(&candidate.super_block, &old_dentry.super_block)
                && candidate_inode == source_inode
            {
                #[cfg(feature = "buildstorm-diagnostics")]
                if diagnose_buildstorm || new_fs_path.contains("/target/debug/") {
                    crate::println!(
                        "[buildstorm-fs] rename-directory-cycle source={} inode={} parent={} parent_inode={}",
                        old_dentry.fs_path,
                        source_inode,
                        candidate.fs_path,
                        candidate_inode,
                    );
                }
                return Err(-22);
            }
            ancestor = dentry_parent(&candidate);
        }
    }
    let same_parent = Arc::ptr_eq(&old_parent, &new_parent);
    let old_key = Arc::as_ptr(&old_parent) as usize;
    let new_key = Arc::as_ptr(&new_parent) as usize;
    let (first_parent, second_parent) = if same_parent || old_key < new_key {
        (old_parent.clone(), (!same_parent).then(|| new_parent.clone()))
    } else {
        (new_parent.clone(), Some(old_parent.clone()))
    };
    let _first_mutation = begin_cached_child_mutation(&first_parent);
    let _second_mutation = second_parent
        .as_ref()
        .map(begin_cached_child_mutation);
    let _source_file_mutation = (!old_dentry.is_dir)
        .then(|| old_dentry.lock_file_mutation())
        .flatten();
    if !old_dentry.is_dir && !old_dentry.flush_pending_writes_during_mutation() {
        return Err(-5);
    }
    // ext4_rename_at2 revalidates both leaf entries and atomically reports a
    // replaced inode without repeating the VFS path walk.
    let replacement_dentry = vfs_cached_lookup_during_mutation(&new_parent, &new_name);
    let _replacement_cache_write = replacement_dentry
        .as_ref()
        .map(|dentry| begin_dentry_cached_write(dentry));

    let lwext4_flags = if flags & RENAME_NOREPLACE != 0 {
        EXT4_RENAME_NOREPLACE
    } else {
        0
    };
    let (rc, replaced_inode, replaced_is_dir) = old_dentry.super_block.rename_child(
        old_parent.inode_num,
        source_inode,
        &old_name,
        new_parent.inode_num,
        &new_name,
        orphan_name,
        lwext4_flags,
    );

    #[cfg(feature = "buildstorm-diagnostics")]
    if old_dentry.fs_path.contains("build-script-build")
        || new_fs_path.contains("build-script-build")
    {
        crate::println!(
            "[buildstorm-link] rename rc={} inode={} replaced={} old={} new={}",
            rc,
            old_dentry.inode_num,
            replaced_inode,
            old_dentry.fs_path,
            new_fs_path,
        );
    }

    if rc == EOK as i32 {
        invalidate_cached_child_during_mutation(&old_parent, &old_name);
        invalidate_cached_child_during_mutation(&new_parent, &new_name);
        if !old_dentry.is_dir {
            let _ = cache_renamed_leaf_during_mutation(
                &new_parent,
                &new_name,
                &old_dentry,
            );
        }
        if replaced_inode != 0 {
            let super_block_key = Arc::as_ptr(&old_dentry.super_block) as usize;
            crate::mm::invalidate_file_page_cache(FilePageCacheKey {
                super_block: super_block_key,
                inode: replaced_inode,
            });
            let retained = !replaced_is_dir
                && retain_replaced_open_inode(
                    super_block_key,
                    replaced_inode,
                    &orphan_fs_path,
                    true,
                );
            if !retained {
                // A closed target may still have delayed writes, including
                // through another hard link. Persist them before removing the
                // orphan name, then drop stale handle/size state so inode reuse
                // cannot inherit the replaced file's cached identity.
                let runtime_ready = replaced_is_dir
                    || flush_and_reset_inode_runtime(super_block_key, replaced_inode);
                let _cleaned = runtime_ready
                    && if replaced_is_dir {
                        old_dentry.super_block.unlink_dir(&orphan_fs_path)
                    } else {
                        old_dentry.super_block.unlink_file(&orphan_fs_path)
                    };
                #[cfg(feature = "buildstorm-diagnostics")]
                if !_cleaned {
                    crate::println!(
                        "[buildstorm-fs] rename-orphan-cleanup-failed inode={} path={}",
                        replaced_inode,
                        orphan_fs_path,
                    );
                }
            }
        }
        Ok(())
    } else {
        #[cfg(feature = "buildstorm-diagnostics")]
        if diagnose_buildstorm || new_fs_path.contains("/target/debug/") {
            crate::println!(
                "[buildstorm-fs] rename-failed rc={} old={} new={}",
                rc,
                old_dentry.fs_path,
                new_fs_path,
            );
        }
        Err(-(rc as isize))
    }
}

// ==================== symlink_at ====================

pub fn symlink_at(base: Option<Arc<Dentry>>, target: &str, linkpath: &str) -> bool {
    use alloc::ffi::CString;
    use lwext4_rust::bindings::{ext4_fsymlink, EOK};

    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let (parent, name) = match vfs_resolve_parent_and_name(&start, linkpath) {
        Some(x) => x,
        None => return false,
    };

    let link_fs_path = if parent.fs_path == "/" {
        alloc::format!("/{}", name)
    } else if parent.fs_path.ends_with('/') {
        alloc::format!("{}{}", parent.fs_path, name)
    } else {
        alloc::format!("{}/{}", parent.fs_path, name)
    };

    let link_cpath = match CString::new(link_fs_path.as_bytes()) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let target_cpath = match CString::new(target) {
        Ok(s) => s,
        Err(_) => return false,
    };

    let rc = {
        let _fs_lock = parent.super_block.lock_fs();
        unsafe {
            ext4_fsymlink(
                target_cpath.as_ptr() as *const core::ffi::c_char,
                link_cpath.as_ptr() as *const core::ffi::c_char,
            )
        }
    };
    if rc == EOK as i32 {
        invalidate_cached_child(&parent, &name);
        true
    } else {
        false
    }
}

// ==================== link_at ====================

pub fn link_at(
    old_base: Option<Arc<Dentry>>,
    old_path: &str,
    new_base: Option<Arc<Dentry>>,
    new_path: &str,
) -> bool {
    use alloc::ffi::CString;
    use lwext4_rust::bindings::{ext4_flink, EOK};

    let old_start = old_base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let old_dentry = match vfs_resolve_path(&old_start, old_path) {
        Some(d) if !d.is_dir => d,
        _ => return false,
    };
    let old_cpath = match CString::new(old_dentry.fs_path.as_bytes()) {
        Ok(s) => s,
        Err(_) => return false,
    };

    let new_start = new_base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let (new_parent, new_name) = match vfs_resolve_parent_and_name(&new_start, new_path) {
        Some(x) => x,
        None => return false,
    };
    let new_fs_path = if new_parent.fs_path == "/" {
        alloc::format!("/{}", new_name)
    } else if new_parent.fs_path.ends_with('/') {
        alloc::format!("{}{}", new_parent.fs_path, new_name)
    } else {
        alloc::format!("{}/{}", new_parent.fs_path, new_name)
    };
    let new_cpath = match CString::new(new_fs_path.as_bytes()) {
        Ok(s) => s,
        Err(_) => return false,
    };

    // Git finalizes a loose object with link(2) followed by unlink(2).  The
    // source may still live only in main's pending-write cache, so publish it
    // before creating the durable hard link.  Serialize the destination name
    // with other namespace mutations and avoid recursively taking that lock
    // during cache invalidation below.
    let _new_mutation = begin_cached_child_mutation(&new_parent);
    if vfs_lookup_during_mutation(&new_parent, &new_name).is_some() {
        return false;
    }
    let _source_file_mutation = old_dentry.lock_file_mutation();
    if !old_dentry.flush_pending_writes_during_mutation() {
        return false;
    }

    let rc = {
        let _fs_lock = old_dentry.super_block.lock_fs();
        unsafe {
            ext4_flink(
                old_cpath.as_ptr() as *const core::ffi::c_char,
                new_cpath.as_ptr() as *const core::ffi::c_char,
            )
        }
    };
    #[cfg(feature = "buildstorm-diagnostics")]
    if old_dentry.fs_path.contains("build-script-build")
        || new_fs_path.contains("build-script-build")
    {
        crate::println!(
            "[buildstorm-link] link rc={} inode={} old={} new={}",
            rc,
            old_dentry.inode_num,
            old_dentry.fs_path,
            new_fs_path,
        );
    }
    if rc == EOK as i32 {
        invalidate_cached_child_during_mutation(&new_parent, &new_name);
        true
    } else {
        false
    }
}
