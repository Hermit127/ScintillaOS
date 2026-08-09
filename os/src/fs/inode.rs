use super::vfs::{
    begin_cached_child_mutation, dentry_parent, invalidate_cached_child,
    invalidate_cached_child_during_mutation, mark_cached_child_absent, read_dentry_symlink,
    vfs_create, vfs_create_during_mutation, vfs_lookup, vfs_resolve_parent_and_name,
    vfs_resolve_path, vfs_resolve_with_fallback, vfs_tmpfs_can_resize,
    vfs_tmpfs_commit_resize, Dentry, DirEntry,
};
use super::{File, FilePageCacheKey, Kstat, S_IFDIR, S_IFLNK, S_IFREG};
use crate::sync::VfsLock;
use alloc::collections::BTreeMap;
use alloc::ffi::CString;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::*;
use lwext4_rust::bindings::*;

const EXT4_BLOCK_SIZE: u32 = 4096;
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
    unlink_in_progress: bool,
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
}

static NEXT_UNLINKED_FILE: AtomicUsize = AtomicUsize::new(0);

fn open_inode_key(dentry: &Arc<Dentry>) -> Option<OpenInodeKey> {
    if dentry.is_dir || dentry.is_link {
        return None;
    }
    let inode = dentry.inode_info()?.inode_num;
    Some(OpenInodeKey {
        super_block: Arc::as_ptr(&dentry.super_block) as usize,
        inode,
    })
}

fn register_open_inode(key: OpenInodeKey) {
    let mut open = OPEN_INODES.exclusive_access();
    let state = open.entry(key).or_insert(OpenInodeState {
        count: 0,
        orphan_path: None,
        unlink_in_progress: false,
    });
    state.count += 1;
}

fn unregister_open_inode(key: OpenInodeKey) -> Option<String> {
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
    open.remove(&key);
    orphan_path
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
    let info = dentry.inode_info();
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
    /// Per-open-file offset, cached stat and lwext4 handle.  It is acquired
    /// before the owning superblock's FsLock whenever the handle is used.
    inner: VfsLock<OSInodeInner>,
    open_key: Option<OpenInodeKey>,
}

unsafe impl Send for OSInode {}
unsafe impl Sync for OSInode {}

pub struct OSInodeInner {
    offset: usize,
    file_handle: Option<ext4_file>,
    pub lock_type: i16,
    append: bool,
    cached_stat: Option<Kstat>,
}

impl Drop for OSInode {
    fn drop(&mut self) {
        {
            let mut inner = self.inner.exclusive_access();
            if let Some(ref mut f) = inner.file_handle {
                let _fs_lock = self.dentry.super_block.lock_fs();
                unsafe {
                    ext4_fclose(f);
                }
            }
        }
        if let Some(orphan_path) = self.open_key.and_then(unregister_open_inode) {
            if self.dentry.super_block.unlink_file(&orphan_path) {
                if let (Some(parent), Some(orphan_name)) = (
                    dentry_parent(&self.dentry),
                    orphan_path.rsplit('/').next(),
                ) {
                    mark_cached_child_absent(&parent, orphan_name);
                }
            }
        }
    }
}

impl OSInode {
    fn invalidate_cached_file_pages(&self) {
        if let Some(key) = self.open_key {
            crate::mm::invalidate_file_page_cache(FilePageCacheKey {
                super_block: key.super_block,
                inode: key.inode,
            });
        }
    }

    pub fn get_dentry(&self) -> &Arc<Dentry> {
        &self.dentry
    }

    pub fn get_lock_type(&self) -> i16 {
        self.inner.exclusive_access().lock_type
    }

    pub fn set_lock_type(&self, t: i16) {
        self.inner.exclusive_access().lock_type = t;
    }

    pub fn new(readable: bool, writable: bool, append: bool, dentry: Arc<Dentry>) -> Self {
        let offset = if append {
            dentry.inode_info().map(|i| i.size as usize).unwrap_or(0)
        } else {
            0
        };
        let key = open_inode_key(&dentry);
        let mut inode = Self {
            readable,
            writable,
            dentry,
            inner: VfsLock::new_named(
                OSInodeInner {
                    offset,
                    file_handle: None,
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
                let _ = inode.stat_inner();
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
            inner.file_handle = Some(file);
            true
        } else {
            drop(inner);
            #[cfg(feature = "buildstorm-diagnostics")]
            if fs_path.contains("/target/debug/") {
                crate::println!(
                    "[buildstorm-fs] open-handle-failed rc={} writable={} path={}",
                    rc,
                    self.writable,
                    fs_path,
                );
            }
            false
        }
    }

    fn has_backing_handle(&self) -> bool {
        self.dentry.is_dir
            || self.dentry.is_link
            || self.inner.exclusive_access().file_handle.is_some()
    }

    pub fn read_all(&self) -> alloc::vec::Vec<u8> {
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
        let dentry_size = self
            .dentry
            .inode_info()
            .map(|info| info.size as i64)
            .unwrap_or(0);
        {
            let mut inner = self.inner.exclusive_access();
            if let Some(mut st) = inner.cached_stat {
                if let Some(ref mut file) = inner.file_handle {
                    st.st_ino = file.inode as u64;
                    st.st_size = (unsafe { ext4_fsize(file) as i64 })
                        .max(dentry_size);
                    st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
                    inner.cached_stat = Some(st);
                    return st;
                }
                if self.dentry.is_dir || (st.st_mode & S_IFREG) != S_IFREG {
                    return st;
                }
            }
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

    pub fn update_cached_size(&self, size: i64) {
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut st) = inner.cached_stat {
            st.st_size = size;
            st.st_blocks = ((size.max(0) as u64) + 511) / 512;
        }
        self.dentry.update_cached_size(size.max(0) as u64);
    }

    pub fn refresh_after_truncate(&self, size: i64) {
        self.invalidate_cached_file_pages();
        let mut inner = self.inner.exclusive_access();
        if let Some(mut file) = inner.file_handle.take() {
            let _fs_lock = self.dentry.super_block.lock_fs();
            unsafe {
                ext4_fclose(&mut file);
            }
        }
        if let Some(ref mut st) = inner.cached_stat {
            st.st_size = size;
            st.st_blocks = ((size.max(0) as u64) + 511) / 512;
        }
        self.dentry.update_cached_size(size.max(0) as u64);
    }

    pub fn truncate(&self, size: u64) -> bool {
        if !self.writable || !self.open_handle() {
            return false;
        }
        self.invalidate_cached_file_pages();
        let mut inner = self.inner.exclusive_access();
        let Some(ref mut file) = inner.file_handle else {
            return false;
        };
        let truncated = {
            let _fs_lock = self.dentry.super_block.lock_fs();
            unsafe { ext4_ftruncate(file, size) == EOK as i32 }
        };
        if !truncated {
            return false;
        }
        if let Some(ref mut st) = inner.cached_stat {
            st.st_size = size as i64;
            st.st_blocks = (size + 511) / 512;
        }
        self.dentry.update_cached_size(size);
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

    pub fn align_up(value: usize, align: usize) -> usize {
        (value + align - 1) & !(align - 1)
    }

    fn fill_gap_with_zero(file: &mut ext4_file, from: usize, to: usize) -> bool {
        if from >= to {
            return true;
        }
        let mut pos = from;
        let zeros = [0u8; 4096];
        unsafe {
            ext4_fseek(file, from as i64, SEEK_SET as u32);
        }
        while pos < to {
            let chunk = (to - pos).min(zeros.len());
            let mut written: usize = 0;
            let rc = unsafe {
                ext4_fwrite(
                    file,
                    zeros[..chunk].as_ptr() as *const core::ffi::c_void,
                    chunk,
                    &mut written,
                )
            };
            if rc != EOK as i32 || written == 0 {
                return false;
            }
            pos += written;
        }
        true
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

fn current_euid_is_root() -> bool {
    crate::task::current_task()
        .map(|task| task.inner_exclusive_access().euid == 0)
        .unwrap_or(false)
}

fn root_make_accessible(dentry: &Arc<Dentry>, bits: u32) {
    if !current_euid_is_root() {
        return;
    }
    if let Some(info) = dentry.inode_info() {
        let new_mode = info.mode | (bits & 0o777);
        if new_mode != info.mode {
            dentry.super_block.set_mode(&dentry.fs_path, new_mode);
            dentry.update_cached_mode(new_mode);
        }
    }
}

pub fn open_file(path: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    open_file_new(None, path, flags, 0o666)
}

pub fn open_file_at(
    base: Option<Arc<Dentry>>,
    path: &str,
    flags: OpenFlags,
    mode: u32,
) -> Option<Arc<OSInode>> {
    open_file_new(base, path, flags, mode)
}

fn open_file_new(
    base: Option<Arc<Dentry>>,
    path: &str,
    flags: OpenFlags,
    mode: u32,
) -> Option<Arc<OSInode>> {
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
        let existing = vfs_lookup(&parent, &name);
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
                dentry.update_cached_size(0);
            }
            let (readable, writable) = flags.read_write();
            let append = flags.contains(OpenFlags::APPEND);
            let inode = Arc::new(OSInode::new(readable, writable, append, dentry));
            return inode.has_backing_handle().then_some(inode);
        }

        let dentry = match vfs_create_during_mutation(&parent, &name, false, mode) {
            Some(dentry) => dentry,
            None => {
                #[cfg(feature = "buildstorm-diagnostics")]
                if diagnose_buildstorm {
                    crate::println!("[buildstorm-fs] create-file-failed path={}", path);
                }
                return None;
            }
        };
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
        let inode = Arc::new(OSInode::new(
            readable,
            writable,
            flags.contains(OpenFlags::APPEND),
            dentry,
        ));
        return inode.has_backing_handle().then_some(inode);
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
        let truncated = dentry.super_block.truncate_file(&dentry.fs_path, 0);
        if !truncated {
            return None;
        }
        dentry.update_cached_size(0);
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

    if vfs_resolve_path(&start, path).is_some() {
        return false;
    }

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
    let dentry = match vfs_lookup(&parent, &name) {
        Some(dentry) => dentry,
        None => return Err(-2),
    };
    if dentry.is_dir && !is_dir {
        return Err(-21);
    }
    if !dentry.is_dir && is_dir {
        return Err(-20);
    }

    root_make_accessible(&parent, 0o777);
    root_make_accessible(&dentry, 0o777);

    let ok = if is_dir {
        dentry.super_block.unlink_dir(&dentry.fs_path)
    } else if let Some(deferred) = defer_open_file_unlink(&dentry) {
        deferred
    } else {
        dentry.super_block.unlink_file(&dentry.fs_path)
    };

    if ok {
        invalidate_cached_child_during_mutation(&parent, &dentry.name);
        Ok(())
    } else {
        Err(-1)
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
        let need_open = {
            let inner = self.inner.exclusive_access();
            inner.file_handle.is_none()
        };
        if need_open {
            self.open_handle();
        }
        let mut inner = self.inner.exclusive_access();
        let off = inner.offset;
        if let Some(ref mut file) = inner.file_handle {
            let _fs_lock = self.dentry.super_block.lock_fs();
            let size = unsafe { ext4_fsize(file) as usize };
            if off >= size {
                return 0;
            }
            let max_read = buf.len().min(size - off);
            unsafe {
                ext4_fseek(file, off as i64, SEEK_SET as u32);
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
                        off + total,
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
            inner.offset = off + total;
            return total as isize;
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
        let need_open = {
            let inner = self.inner.exclusive_access();
            inner.file_handle.is_none()
        };
        if need_open {
            self.open_handle();
        }
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut file) = inner.file_handle {
            let _fs_lock = self.dentry.super_block.lock_fs();
            let size = unsafe { ext4_fsize(file) as usize };
            if offset >= size {
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
                        "[frame-diag] watched page changed during inode pread path={} off={:#x} requested={} returned={} rc={}",
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
            return total as isize;
        }
        drop(inner);
        self.dentry
            .super_block
            .read_at(&self.dentry.fs_path, offset, buf) as isize
    }

    fn write_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        if !self.writable {
            return -1;
        }
        if !buf.is_empty() {
            self.invalidate_cached_file_pages();
        }
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
            let _fs_lock = self.dentry.super_block.lock_fs();
            let size = unsafe { ext4_fsize(file) as usize };
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
            if offset > size && !Self::fill_gap_with_zero(file, size, offset) {
                return -1;
            }
            unsafe {
                ext4_fseek(file, offset as i64, SEEK_SET as u32);
            }
            let mut total = 0usize;
            while total < buf.len() {
                let mut rw_count: usize = 0;
                let rc = unsafe {
                    ext4_fwrite_with_times(
                        file,
                        buf[total..].as_ptr() as *const core::ffi::c_void,
                        buf.len() - total,
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
                            buf.len() - total,
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
            // Cache publication and tmpfs accounting use independent VFS
            // state. Release FsLock before entering those domains.
            drop(_fs_lock);
            if let Some(ref mut st) = inner.cached_stat {
                st.st_size = new_size;
                st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
            }
            self.dentry.update_cached_size(new_size.max(0) as u64);
            if total > 0 {
                Self::update_cached_data_time(&mut inner, now_sec);
                drop(inner);
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
                    crate::fs::record_block_write_bytes(written);
                    let new_size = (offset + written) as i64;
                    self.update_cached_size(new_size);
                    self.update_cached_times(None, Some(now_sec), Some(now_sec));
                    self.update_cached_path_data_time(now_sec);
                    vfs_tmpfs_commit_resize(
                        &self.dentry.path,
                        &self.dentry.fs_path,
                        size as u64,
                        new_size.max(0) as u64,
                    );
                }
                return written as isize;
            }
            return total as isize;
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
            let new_size = (offset + written) as i64;
            self.update_cached_size(new_size);
            self.update_cached_times(None, Some(now_sec), Some(now_sec));
            self.update_cached_path_data_time(now_sec);
            vfs_tmpfs_commit_resize(
                &self.dentry.path,
                &self.dentry.fs_path,
                old_size as u64,
                new_size.max(0) as u64,
            );
        }
        written as isize
    }

    fn write(&self, buf: &mut [u8]) -> isize {
        if !buf.is_empty() {
            self.invalidate_cached_file_pages();
        }
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
            let _fs_lock = self.dentry.super_block.lock_fs();
            let off = if append {
                unsafe { ext4_fsize(file) as usize }
            } else {
                cur_offset
            };
            let size = unsafe { ext4_fsize(file) as usize };
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
            if off > size && !Self::fill_gap_with_zero(file, size, off) {
                inner.offset = unsafe { ext4_fsize(file) as usize };
                return -1;
            }
            unsafe {
                ext4_fseek(file, off as i64, SEEK_SET as u32);
            }
            let mut total = 0usize;
            while total < buf.len() {
                let mut rw_count: usize = 0;
                let rc = unsafe {
                    ext4_fwrite_with_times(
                        file,
                        buf[total..].as_ptr() as *const core::ffi::c_void,
                        buf.len() - total,
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
                            buf.len() - total,
                            rw_count,
                            self.dentry.fs_path,
                        );
                    }
                    break;
                }
                total += rw_count;
            }
            let new_size = unsafe { ext4_fsize(file) as i64 };
            // Cache publication and tmpfs accounting must happen after the
            // direct-handle transaction releases FsLock.
            drop(_fs_lock);
            inner.offset = off + total;
            if let Some(ref mut st) = inner.cached_stat {
                st.st_size = new_size;
                st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
            }
            self.dentry.update_cached_size(new_size.max(0) as u64);
            if total > 0 {
                Self::update_cached_data_time(&mut inner, now_sec);
                drop(inner);
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
                let mut inner = self.inner.exclusive_access();
                inner.offset = off + rw_count;
                if let Some(ref mut st) = inner.cached_stat {
                    st.st_size = (off + rw_count) as i64;
                    st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
                }
                self.dentry.update_cached_size((off + rw_count) as u64);
                if rw_count > 0 {
                    Self::update_cached_data_time(&mut inner, now_sec);
                    drop(inner);
                    self.update_cached_path_data_time(now_sec);
                    crate::fs::record_block_write_bytes(rw_count);
                    vfs_tmpfs_commit_resize(
                        &self.dentry.path,
                        &self.dentry.fs_path,
                        size as u64,
                        (off + rw_count) as u64,
                    );
                }
                return rw_count as isize;
            }
            return total as isize;
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
        let mut inner = self.inner.exclusive_access();
        inner.offset = off + rw_count;
        if let Some(ref mut st) = inner.cached_stat {
            st.st_size = (off + rw_count) as i64;
            st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
        }
        self.dentry.update_cached_size((off + rw_count) as u64);
        if rw_count > 0 {
            Self::update_cached_data_time(&mut inner, now_sec);
            drop(inner);
            self.update_cached_path_data_time(now_sec);
            crate::fs::record_block_write_bytes(rw_count);
            vfs_tmpfs_commit_resize(
                &self.dentry.path,
                &self.dentry.fs_path,
                old_size as u64,
                (off + rw_count) as u64,
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
        self.open_key.map(|key| FilePageCacheKey {
            super_block: key.super_block,
            inode: key.inode,
        })
    }

    fn getdents64(&self, buf: &mut [u8]) -> isize {
        const DIRENT64_FIXED_SIZE: usize = 8 + 8 + 2 + 1;
        let mut inner = self.inner.exclusive_access();

        let entries: Vec<DirEntry> = self
            .dentry
            .super_block
            .dir_get_entries(&self.dentry.fs_path);

        let mut written = 0usize;
        let mut entry_index = inner.offset;

        if inner.offset >= entries.len() {
            return 0;
        }

        while entry_index < entries.len() {
            let entry = &entries[entry_index];
            let name = entry.get_name();
            let name_bytes = name.as_bytes();
            if name_bytes.is_empty()
                || name.starts_with(".tmpfile.")
                || name.starts_with(UNLINKED_FILE_PREFIX)
            {
                entry_index += 1;
                continue;
            }
            let record_size = Self::align_up(DIRENT64_FIXED_SIZE + name_bytes.len() + 1, 8);
            if written + record_size > buf.len() {
                if written == 0 {
                    return -1;
                }
                break;
            }

            let record = &mut buf[written..written + record_size];
            record.fill(0);

            record[0..8].copy_from_slice(&(entry.inode as u64).to_le_bytes());
            let d_off = (entry_index as i64) + 1;
            record[8..16].copy_from_slice(&d_off.to_le_bytes());
            record[16..18].copy_from_slice(&(record_size as u16).to_le_bytes());
            record[18] = entry.get_de_type();

            let name_start = DIRENT64_FIXED_SIZE;
            record[name_start..name_start + name_bytes.len()].copy_from_slice(name_bytes);
            record[name_start + name_bytes.len()] = 0;

            written += record_size;
            entry_index += 1;
        }

        inner.offset = entry_index;
        written as isize
    }

    fn seek(&self, offset: usize) {
        let mut inner = self.inner.exclusive_access();
        inner.offset = offset;
        if let Some(ref mut file) = inner.file_handle {
            let _fs_lock = self.dentry.super_block.lock_fs();
            unsafe {
                ext4_fseek(file, offset as i64, SEEK_SET as u32);
            }
        }
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
    use alloc::ffi::CString;
    use lwext4_rust::bindings::{ext4_frename2, EOK, EXT4_RENAME_NOREPLACE};

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

    let old_cpath = match CString::new(old_dentry.fs_path.as_bytes()) {
        Ok(s) => s,
        Err(_) => return Err(-22),
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

    let new_fs_path = if new_parent.fs_path == "/" {
        alloc::format!("/{}", new_name)
    } else if new_parent.fs_path.ends_with('/') {
        alloc::format!("{}{}", new_parent.fs_path, new_name)
    } else {
        alloc::format!("{}/{}", new_parent.fs_path, new_name)
    };

    let new_cpath = match CString::new(new_fs_path.as_bytes()) {
        Ok(s) => s,
        Err(_) => return Err(-22),
    };

    let orphan_fs_path = replacement_orphan_path(&new_parent);
    let orphan_cpath = match CString::new(orphan_fs_path.as_bytes()) {
        Ok(s) => s,
        Err(_) => return Err(-22),
    };

    if old_dentry.is_dir {
        let source_inode = old_dentry
            .inode_info()
            .map(|info| info.inode_num)
            .unwrap_or(old_dentry.inode_num);
        let mut ancestor = Some(new_parent.clone());
        while let Some(candidate) = ancestor {
            let candidate_inode = candidate
                .inode_info()
                .map(|info| info.inode_num)
                .unwrap_or(candidate.inode_num);
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

    let mut replaced_inode = 0u32;
    let mut replaced_is_dir = false;
    let lwext4_flags = if flags & RENAME_NOREPLACE != 0 {
        EXT4_RENAME_NOREPLACE
    } else {
        0
    };
    let rc = {
        let _fs_lock = old_dentry.super_block.lock_fs();
        unsafe {
            ext4_frename2(
                old_cpath.as_ptr() as *const core::ffi::c_char,
                new_cpath.as_ptr() as *const core::ffi::c_char,
                orphan_cpath.as_ptr() as *const core::ffi::c_char,
                lwext4_flags,
                &mut replaced_inode,
                &mut replaced_is_dir,
            )
        }
    };

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
        if replaced_inode != 0 {
            let super_block_key = Arc::as_ptr(&old_dentry.super_block) as usize;
            let retained = !replaced_is_dir
                && retain_replaced_open_inode(
                    super_block_key,
                    replaced_inode,
                    &orphan_fs_path,
                );
            if !retained {
                let cleaned = if replaced_is_dir {
                    old_dentry.super_block.unlink_dir(&orphan_fs_path)
                } else {
                    old_dentry.super_block.unlink_file(&orphan_fs_path)
                };
                #[cfg(feature = "buildstorm-diagnostics")]
                if !cleaned {
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
        invalidate_cached_child(&new_parent, &new_name);
        true
    } else {
        false
    }
}
