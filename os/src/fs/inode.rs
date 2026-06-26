use super::vfs::{
    dentry_parent, vfs_create, vfs_resolve_parent_and_name, vfs_resolve_path,
    vfs_resolve_with_fallback, Dentry, DirEntry,
};
use super::{File, Kstat, S_IFDIR, S_IFIFO, S_IFREG};
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::ffi::CString;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;
use lazy_static::*;
use lwext4_rust::bindings::*;

const EXT4_BLOCK_SIZE: u32 = 4096;
const PATH_TIME_CACHE_MAX_ENTRIES: usize = 1024;

#[derive(Clone, Copy, Default)]
struct CachedTimes {
    atime: Option<i64>,
    mtime: Option<i64>,
    ctime: Option<i64>,
}

lazy_static! {
    static ref PATH_TIME_CACHE: UPSafeCell<BTreeMap<String, CachedTimes>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
    static ref PATH_SPECIAL_MODE_CACHE: UPSafeCell<BTreeMap<String, u32>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
}

pub fn record_special_path_mode(fs_path: &str, mode_type: u32) {
    if mode_type == S_IFIFO {
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

// ==================== OSInode ====================

pub struct OSInode {
    readable: bool,
    writable: bool,
    dentry: Arc<Dentry>,
    inner: UPSafeCell<OSInodeInner>,
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
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut f) = inner.file_handle {
            unsafe {
                ext4_fclose(f);
            }
        }
    }
}

impl OSInode {
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
            dentry
                .super_block
                .get_inode_info(&dentry.fs_path)
                .map(|i| i.size as usize)
                .unwrap_or(0)
        } else {
            0
        };
        let inode = Self {
            readable,
            writable,
            dentry,
            inner: unsafe {
                UPSafeCell::new_named(
                    OSInodeInner {
                        offset,
                        file_handle: None,
                        lock_type: 2,
                        append,
                        cached_stat: None,
                    },
                    "OSInodeInner",
                )
            },
        };
        if !inode.dentry.is_dir {
            inode.open_handle();
        }
        inode
    }
    pub fn dentry(&self) -> &Arc<Dentry> {
        &self.dentry
    }

    fn open_handle(&self) {
        let fs_path = &self.dentry.fs_path;
        let c_path = match CString::new(fs_path.as_bytes()) {
            Ok(s) => s,
            Err(_) => return,
        };
        // File already exists (created by open_file_at/create_file).
        // Use "r+b" for writable files, "rb" for read-only.
        let flags = if self.writable { "r+b" } else { "rb" };
        let c_flags = match CString::new(flags) {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut file: ext4_file = unsafe { core::mem::zeroed() };
        let rc = unsafe {
            ext4_fopen(
                &mut file,
                c_path.as_ptr() as *const core::ffi::c_char,
                c_flags.as_ptr() as *const core::ffi::c_char,
            )
        };
        if rc == EOK as i32 {
            let mut inner = self.inner.exclusive_access();
            // seek to current offset
            let offset = inner.offset;
            unsafe {
                ext4_fseek(&mut file, offset as i64, SEEK_SET as u32);
            }
            inner.file_handle = Some(file);
        }
    }

    pub fn read_all(&self) -> alloc::vec::Vec<u8> {
        let mut inner = self.inner.exclusive_access();
        let size = self
            .dentry
            .super_block
            .get_inode_info(&self.dentry.fs_path)
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
        {
            let mut inner = self.inner.exclusive_access();
            if let Some(mut st) = inner.cached_stat {
                if let Some(ref mut file) = inner.file_handle {
                    st.st_ino = file.inode as u64;
                    st.st_size = (unsafe { ext4_fsize(file) as i64 }).max(inner.offset as i64);
                    st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
                    inner.cached_stat = Some(st);
                }
                return st;
            }
        }
        let info = self.dentry.super_block.get_inode_info(&self.dentry.fs_path);
        let mut st = match info {
            Some(info) => {
                let file_type = if self.dentry.is_dir || info.is_dir {
                    S_IFDIR
                } else if let Some(mode_type) = path_special_mode(&self.dentry.fs_path) {
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
                if file_type == S_IFDIR {
                    st.st_nlink = st.st_nlink.max(2);
                }
                st.st_atime_sec = info.atime as i64;
                st.st_mtime_sec = info.mtime as i64;
                st.st_ctime_sec = info.ctime as i64;
                let cached_times = path_cached_times(&self.dentry.fs_path);
                let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as i64;
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
                if self.dentry.is_dir {
                    Kstat::new(S_IFDIR | 0o755)
                } else {
                    Kstat::new(S_IFREG | 0o644)
                }
            }
        };
        if !self.dentry.is_dir {
            let inner = self.inner.exclusive_access();
            st.st_size = st.st_size.max(inner.offset as i64);
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
    }

    pub fn update_cached_mode(&self, mode: u32) {
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut st) = inner.cached_stat {
            st.st_mode = mode;
        }
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

    pub fn read_write(&self) -> (bool, bool) {
        if self.contains(Self::WRONLY) {
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
    if let Some(info) = dentry.super_block.get_inode_info(&dentry.fs_path) {
        let new_mode = info.mode | (bits & 0o777);
        if new_mode != info.mode {
            dentry.super_block.set_mode(&dentry.fs_path, new_mode);
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

    if flags.create_requested() {
        if want_dir {
            return None;
        }

        if let Some(dentry) =
            vfs_resolve_with_fallback(&start, path).and_then(|d| follow_symlink(&start, d, 0))
        {
            if !dentry.is_dir {
                if flags.excl_requested() {
                    return None;
                }
                if flags.trunc_requested() {
                    let truncated = dentry.super_block.truncate_file(&dentry.fs_path, 0);
                    if !truncated {
                        return None;
                    }
                }
                let (readable, writable) = flags.read_write();
                let append = flags.contains(OpenFlags::APPEND);
                return Some(Arc::new(OSInode::new(readable, writable, append, dentry)));
            }
        }

        let (parent, name) = vfs_resolve_parent_and_name(&start, path)?;
        let dentry = vfs_create(&parent, &name, false, mode)?;
        let (readable, writable) = flags.read_write();
        return Some(Arc::new(OSInode::new(
            readable,
            writable,
            flags.contains(OpenFlags::APPEND),
            dentry,
        )));
    }

    let dentry = follow_symlink(&start, vfs_resolve_with_fallback(&start, path)?, 0)?;
    if want_dir && !dentry.is_dir {
        return None;
    }

    if !dentry.is_dir
        && (flags.trunc_requested()
            || flags.contains(OpenFlags::WRONLY)
            || flags.contains(OpenFlags::RDWR))
    {
        root_make_accessible(&dentry, 0o666);
    }

    if flags.trunc_requested() && !dentry.is_dir {
        let truncated = dentry.super_block.truncate_file(&dentry.fs_path, 0);
        if !truncated {
            return None;
        }
    }

    let (readable, writable) = if dentry.is_dir {
        (false, false)
    } else {
        flags.read_write()
    };
    let append = !dentry.is_dir && flags.contains(OpenFlags::APPEND);
    Some(Arc::new(OSInode::new(readable, writable, append, dentry)))
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
    let info = dentry.super_block.get_inode_info(&dentry.fs_path)?;
    if !info.is_link {
        return Some(dentry);
    }

    let mut target_buf = [0u8; 512];
    let mut rcnt: usize = 0;
    let c_path = alloc::ffi::CString::new(dentry.fs_path.as_bytes()).ok()?;
    let rc = unsafe {
        lwext4_rust::bindings::ext4_readlink(
            c_path.as_ptr() as *const core::ffi::c_char,
            target_buf.as_mut_ptr() as *mut core::ffi::c_char,
            target_buf.len(),
            &mut rcnt,
        )
    };
    if rc != 0 || rcnt == 0 {
        return None;
    }
    let target = core::str::from_utf8(&target_buf[..rcnt])
        .ok()?
        .trim_matches('\0');
    if target.is_empty() {
        return None;
    }

    let base = if target.starts_with('/') {
        crate::task::current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone())
    } else {
        dentry_parent(&dentry).unwrap_or_else(|| start.clone())
    };
    let next = vfs_resolve_with_fallback(&base, target)?;
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

pub fn unlink_at(base: Option<Arc<Dentry>>, path: &str, is_dir: bool) -> bool {
    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let dentry = match vfs_resolve_path(&start, path) {
        Some(d) => d,
        None => return false,
    };

    if dentry.is_dir != is_dir {
        return false;
    }

    let parent = match dentry_parent(&dentry) {
        Some(p) => p,
        None => return false,
    };

    root_make_accessible(&parent, 0o777);
    root_make_accessible(&dentry, 0o777);

    let ok = if is_dir {
        dentry.super_block.unlink_dir(&dentry.fs_path)
    } else {
        dentry.super_block.unlink_file(&dentry.fs_path)
    };

    if ok {
        let mut children = parent.children.exclusive_access();
        if let Some(list) = children.as_mut() {
            list.retain(|c| c.name != dentry.name);
        }
    }
    ok
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
        if !self.writable {
            let mut inner = self.inner.exclusive_access();
            let rw_count = self
                .dentry
                .super_block
                .read_at(&self.dentry.fs_path, inner.offset, buf);
            inner.offset += rw_count;
            return rw_count as isize;
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
                let rc = unsafe {
                    ext4_fread(
                        file,
                        buf[total..max_read].as_mut_ptr() as *mut core::ffi::c_void,
                        max_read - total,
                        &mut rw_count,
                    )
                };
                if rc != EOK as i32 || rw_count == 0 {
                    break;
                }
                total += rw_count;
            }
            inner.offset = off + total;
            return total as isize;
        }
        -1
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        if !self.writable {
            return self
                .dentry
                .super_block
                .read_at(&self.dentry.fs_path, offset, buf) as isize;
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
                let rc = unsafe {
                    ext4_fread(
                        file,
                        buf[total..max_read].as_mut_ptr() as *mut core::ffi::c_void,
                        max_read - total,
                        &mut rw_count,
                    )
                };
                if rc != EOK as i32 || rw_count == 0 {
                    break;
                }
                total += rw_count;
            }
            return total as isize;
        }
        -1
    }

    fn write_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        if !self.writable {
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
        let saved_offset = inner.offset;
        if let Some(ref mut file) = inner.file_handle {
            let size = unsafe { ext4_fsize(file) as usize };
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
                    ext4_fwrite(
                        file,
                        buf[total..].as_ptr() as *const core::ffi::c_void,
                        buf.len() - total,
                        &mut rw_count,
                    )
                };
                if rc != EOK as i32 || rw_count == 0 {
                    break;
                }
                total += rw_count;
            }
            unsafe {
                ext4_fseek(file, saved_offset as i64, SEEK_SET as u32);
            }
            let new_size = unsafe { ext4_fsize(file) as i64 };
            if let Some(ref mut st) = inner.cached_stat {
                st.st_size = new_size;
                st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
            }
            if total == 0 && !buf.is_empty() {
                inner.file_handle = None;
                drop(inner);
                let written = self
                    .dentry
                    .super_block
                    .write_at(&self.dentry.fs_path, offset, buf);
                if written > 0 {
                    let new_size = (offset + written) as i64;
                    self.update_cached_size(new_size);
                }
                return written as isize;
            }
            return total as isize;
        }
        drop(inner);
        let written = self
            .dentry
            .super_block
            .write_at(&self.dentry.fs_path, offset, buf);
        if written > 0 {
            let new_size = (offset + written) as i64;
            self.update_cached_size(new_size);
        }
        written as isize
    }

    fn write(&self, buf: &mut [u8]) -> isize {
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
        if let Some(ref mut file) = inner.file_handle {
            let off = if append {
                unsafe { ext4_fsize(file) as usize }
            } else {
                cur_offset
            };
            let size = unsafe { ext4_fsize(file) as usize };
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
                    ext4_fwrite(
                        file,
                        buf[total..].as_ptr() as *const core::ffi::c_void,
                        buf.len() - total,
                        &mut rw_count,
                    )
                };
                if rc != EOK as i32 || rw_count == 0 {
                    break;
                }
                total += rw_count;
            }
            let new_size = unsafe { ext4_fsize(file) as i64 };
            inner.offset = off + total;
            if let Some(ref mut st) = inner.cached_stat {
                st.st_size = new_size;
                st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
            }
            if total == 0 && !buf.is_empty() {
                inner.file_handle = None;
                drop(inner);
                let rw_count = self
                    .dentry
                    .super_block
                    .write_at(&self.dentry.fs_path, off, buf);
                let mut inner = self.inner.exclusive_access();
                inner.offset = off + rw_count;
                if let Some(ref mut st) = inner.cached_stat {
                    st.st_size = (off + rw_count) as i64;
                    st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
                }
                return rw_count as isize;
            }
            return total as isize;
        }
        let off = if inner.append {
            self.dentry
                .super_block
                .get_inode_info(&self.dentry.fs_path)
                .map(|i| i.size as usize)
                .unwrap_or(inner.offset)
        } else {
            inner.offset
        };
        drop(inner);
        let rw_count = self
            .dentry
            .super_block
            .write_at(&self.dentry.fs_path, off, buf);
        let mut inner = self.inner.exclusive_access();
        inner.offset = off + rw_count;
        if let Some(ref mut st) = inner.cached_stat {
            st.st_size = (off + rw_count) as i64;
            st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
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
            if name_bytes.is_empty() {
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
) -> bool {
    use alloc::ffi::CString;
    use lwext4_rust::bindings::{ext4_frename, EOK};

    let start = old_base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let old_dentry = match vfs_resolve_path(&start, old_path) {
        Some(d) => d,
        None => return false,
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

    let rc = unsafe {
        ext4_frename(
            old_cpath.as_ptr() as *const core::ffi::c_char,
            new_cpath.as_ptr() as *const core::ffi::c_char,
        )
    };

    if rc == EOK as i32 {
        if let Some(old_parent) = dentry_parent(&old_dentry) {
            let mut children = old_parent.children.exclusive_access();
            if let Some(list) = children.as_mut() {
                list.retain(|c| c.name != old_dentry.name);
            }
        }
        let mut children = new_parent.children.exclusive_access();
        if let Some(list) = children.as_mut() {
            list.retain(|c| c.name != new_name);
        }
        true
    } else {
        false
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

    let rc = unsafe {
        ext4_fsymlink(
            target_cpath.as_ptr() as *const core::ffi::c_char,
            link_cpath.as_ptr() as *const core::ffi::c_char,
        )
    };

    rc == EOK as i32
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

    let rc = unsafe {
        ext4_flink(
            old_cpath.as_ptr() as *const core::ffi::c_char,
            new_cpath.as_ptr() as *const core::ffi::c_char,
        )
    };
    if rc == EOK as i32 {
        let mut children = new_parent.children.exclusive_access();
        if let Some(list) = children.as_mut() {
            list.retain(|c| c.name != new_name);
        }
        true
    } else {
        false
    }
}
