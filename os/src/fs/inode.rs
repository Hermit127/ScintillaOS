use super::vfs::{
    vfs_create, vfs_resolve_parent_and_name, vfs_resolve_path, vfs_resolve_with_fallback, Dentry,
    DirEntry,
};
use super::{File, Kstat, S_IFDIR, S_IFREG};
use crate::sync::UPSafeCell;
use alloc::ffi::CString;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;
use lwext4_rust::bindings::*;

const EXT4_BLOCK_SIZE: u32 = 4096;

// ==================== OSInode ====================

pub struct OSInode {
    readable: bool,
    writable: bool,
    append: bool,
    dentry: Arc<Dentry>,
    inner: UPSafeCell<OSInodeInner>,
}

unsafe impl Send for OSInode {}
unsafe impl Sync for OSInode {}

pub struct OSInodeInner {
    offset: usize,
    file_handle: Option<ext4_file>,
    pub lock_type: i16,
}

impl Drop for OSInode {
    fn drop(&mut self) {
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut f) = inner.file_handle {
            unsafe { ext4_fclose(f); }
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
        Self {
            readable,
            writable,
            append,
            dentry,
            inner: unsafe { UPSafeCell::new_named(OSInodeInner { offset, file_handle: None, lock_type: 2 }, "OSInodeInner") },
        }
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
            unsafe { ext4_fseek(&mut file, offset as i64, SEEK_SET as u32); }
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
        let inner = self.inner.exclusive_access();
        let info = self.dentry.super_block.get_inode_info(&self.dentry.fs_path);
        match info {
            Some(info) => {
                let mut st = Kstat::new(info.mode);
                st.st_ino = info.inode_num as u64;
                st.st_size = info.size as i64;
                st.st_blksize = EXT4_BLOCK_SIZE;
                st.st_blocks = info.blocks_count;
                st.st_atime_sec = info.atime as i64;
                st.st_mtime_sec = info.mtime as i64;
                st.st_ctime_sec = info.ctime as i64;
                drop(inner);
                st
            }
            None => {
                let mode = if self.dentry.is_dir {
                    S_IFDIR | 0o755
                } else {
                    S_IFREG | 0o644
                };
                let st = Kstat::new(mode);
                drop(inner);
                st
            }
        }
    }

    pub fn align_up(value: usize, align: usize) -> usize {
        (value + align - 1) & !(align - 1)
    }
}

// ==================== OpenFlags ====================

bitflags! {
    pub struct OpenFlags: u32 {
        const RDONLY = 0;
        const WRONLY = 1 << 0;
        const RDWR = 1 << 1;
        const CREATE = 0x40;
        const TRUNC = 0x200;
        const APPEND = 0x400;
        const DIRECTORY = 0x10000;
        const TMPFILE = 0x400000;
    }
}

impl OpenFlags {
    const TRUNC_LEGACY_BITS: u32 = 1 << 10;

    pub fn from_raw(flags: u32) -> Self {
        Self::from_bits_truncate(flags)
    }

    pub fn create_requested(&self) -> bool {
        self.contains(Self::CREATE)
    }

    pub fn trunc_requested(&self) -> bool {
        (self.bits() & (Self::TRUNC.bits() | Self::TRUNC_LEGACY_BITS)) != 0
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

    if flags.contains(OpenFlags::TMPFILE) {
        let parent = vfs_resolve_with_fallback(&start, path)?;
        if !parent.is_dir {
            return None;
        }
        let name = alloc::format!("tmp_{}", polyhal::time::Time::now().to_nsec());
        let dentry = vfs_create(&parent, &name, false, mode)?;
        return Some(Arc::new(OSInode::new(true, true, false, dentry)));
    }

    if flags.create_requested() {
        if want_dir {
            return None;
        }

        if let Some(dentry) = vfs_resolve_with_fallback(&start, path) {
            if !dentry.is_dir {
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

    let dentry = vfs_resolve_with_fallback(&start, path)?;
    if want_dir && !dentry.is_dir {
        return None;
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

pub fn mkdir_at(base: Option<Arc<Dentry>>, path: &str) -> bool {
    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());

    if let Some(existing) = vfs_resolve_path(&start, path) {
        return existing.is_dir;
    }

    let (parent, name) = match vfs_resolve_parent_and_name(&start, path) {
        Some(x) => x,
        None => return false,
    };

    vfs_create(&parent, &name, true, 0o777).is_some()
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

    let parent = match dentry.parent.clone() {
        Some(p) => p,
        None => return false,
    };

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
        let mut inner = self.inner.exclusive_access();
        let rw_count = self
            .dentry
            .super_block
            .read_at(&self.dentry.fs_path, inner.offset, buf);
        inner.offset += rw_count;
        rw_count as isize
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        let need_open = {
            let inner = self.inner.exclusive_access();
            inner.file_handle.is_none()
        };
        if need_open {
            self.open_handle();
        }
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut file) = inner.file_handle {
            unsafe {
                ext4_fseek(file, offset as i64, SEEK_SET as u32);
            }
            let mut total = 0usize;
            while total < buf.len() {
                let mut rw_count: usize = 0;
                let rc = unsafe {
                    ext4_fread(
                        file,
                        buf[total..].as_mut_ptr() as *mut core::ffi::c_void,
                        buf.len() - total,
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
        let need_open = {
            let inner = self.inner.exclusive_access();
            inner.file_handle.is_none()
        };
        if need_open {
            self.open_handle();
        }
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut file) = inner.file_handle {
            unsafe {
                ext4_fseek(file, offset as i64, SEEK_SET as u32);
            }
            let mut rw_count: usize = 0;
            let rc = unsafe {
                ext4_fwrite(
                    file,
                    buf.as_ptr() as *const core::ffi::c_void,
                    buf.len(),
                    &mut rw_count,
                )
            };
            if rc == EOK as i32 {
                return rw_count as isize;
            }
        }
        -1
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
        if self.append {
            inner.offset = self
                .dentry
                .super_block
                .get_inode_info(&self.dentry.fs_path)
                .map(|i| i.size as usize)
                .unwrap_or(inner.offset);
        }
        let off = inner.offset;
        if let Some(ref mut file) = inner.file_handle {
            unsafe {
                ext4_fseek(file, off as i64, SEEK_SET as u32);
            }
            let mut rw_count: usize = 0;
            let rc = unsafe {
                ext4_fwrite(
                    file,
                    buf.as_ptr() as *const core::ffi::c_void,
                    buf.len(),
                    &mut rw_count,
                )
            };
            if rc == EOK as i32 {
                inner.offset = off + rw_count;
                return rw_count as isize;
            }
        }
        -1
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
        if let Some(old_parent) = old_dentry.parent.clone() {
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
            link_cpath.as_ptr() as *const core::ffi::c_char,
            target_cpath.as_ptr() as *const core::ffi::c_char,
        )
    };

    rc == EOK as i32
}
