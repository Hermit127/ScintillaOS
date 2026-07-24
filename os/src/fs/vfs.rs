use crate::sync::{FsGuard, FsLock, VfsLock};
use alloc::collections::BTreeMap;
use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ffi::c_char;
use core::ops::{Deref, DerefMut};
use lazy_static::*;

use super::block_device_stream::{BlockDeviceStream, BlockDeviceStreamOp, Ext4Wrapper};
use lwext4_rust::bindings::*;
use lwext4_rust::Ext4BlockWrapper;

const PATH_OWNER_CACHE_MAX_ENTRIES: usize = 1024;
// The first SMP release has no mount hotplug requirement.  Reserving the
// small mount table up front keeps Vec growth (and therefore allocation) out
// of VfsLock critical sections.
const MOUNT_TABLE_CAPACITY: usize = 16;

lazy_static! {
    // lwext4 keeps process-global mount/cache state internally.  Per-volume
    // locks are insufficient in the dual-disk SMP configuration: operations
    // on rootfs and the test disk must not enter lwext4 concurrently.
    static ref LWEXT4_GLOBAL: FsLock<()> =
        FsLock::new_named((), "LwExt4Global");
    static ref PATH_OWNER_CACHE: VfsLock<BTreeMap<String, (u32, u32)>> =
        VfsLock::new_named(BTreeMap::new(), "VfsPathOwnerCache");
}

pub(crate) struct VfsFsGuard<'a> {
    _global: FsGuard<'a, ()>,
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
        self.de_type
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

// ==================== VfsSuperBlock ====================

pub enum FsInner {
    Ext4(Ext4Wrapper),
}

pub struct VfsSuperBlock {
    pub fs_type: &'static str,
    /// Ext4/lwext4 is not reentrant.  The first SMP revision serializes each
    /// mounted superblock explicitly, rather than relying on a global
    /// single-core cell.
    pub fs_data: FsLock<FsInner>,
}

impl VfsSuperBlock {
    /// Serialize direct lwext4 handle operations.  Callers that also protect
    /// dentry or open-file metadata must acquire that VFS lock first, then
    /// this guard, following `VfsLock -> FsLock`.
    pub(crate) fn lock_fs(&self) -> VfsFsGuard<'_> {
        let global = LWEXT4_GLOBAL.exclusive_access();
        let local = self.fs_data.exclusive_access();
        VfsFsGuard {
            _global: global,
            local,
        }
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
        let total_size = blk_device.capacity_bytes().unwrap_or(FALLBACK_DISK_SIZE);
        let stream = BlockDeviceStream {
            inner: blk_device,
            pos: 0,
            total_size,
            scratch: alloc::vec![0u8; 128 * 1024],
        };
        let ext4 =
            Ext4BlockWrapper::<BlockDeviceStreamOp>::new_with_name(stream, "ext4_fs", mount_point)
                .ok()?;
        Some(Arc::new(VfsSuperBlock {
            fs_type,
            fs_data: FsLock::new_named(FsInner::Ext4(Ext4Wrapper(ext4)), "VfsFsData"),
        }))
    }

    fn with_fs<R>(&self, f: impl FnOnce(&Ext4Wrapper) -> R) -> R {
        let data = self.lock_fs();
        match &*data {
            FsInner::Ext4(wrapper) => f(wrapper),
        }
    }

    pub fn force_umount(&self) {
        let mut data = self.lock_fs();
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

    pub fn get_inode_info(&self, fs_path: &str) -> Option<InodeInfo> {
        // The owner cache is VFS metadata. Take this snapshot before
        // `with_fs` acquires the superblock FsLock, preserving the documented
        // `VfsLock -> FsLock` order.
        let cached_owner = cached_path_owner(fs_path);
        self.with_fs(|_fs| {
            let mut mode: u32 = 0;
            let c_path2 = CString::new(fs_path).ok()?;
            let _mode_rc = unsafe { ext4_mode_get(c_path2.as_ptr() as *const c_char, &mut mode) };
            let mut raw_ino: u32 = 0;
            let mut raw_inode: ext4_inode = unsafe { core::mem::zeroed() };
            let raw_rc = unsafe {
                ext4_raw_inode_fill(
                    c_path2.as_ptr() as *const c_char,
                    &mut raw_ino,
                    &mut raw_inode,
                )
            };
            let links_count = if raw_rc == EOK as i32 {
                raw_inode.links_count as u32
            } else {
                1
            };

            let mut uid: u32 = 0;
            let mut gid: u32 = 0;
            let c_path_owner = CString::new(fs_path).ok()?;
            unsafe {
                ext4_owner_get(c_path_owner.as_ptr() as *const c_char, &mut uid, &mut gid);
            }
            if let Some((cached_uid, cached_gid)) = cached_owner {
                uid = cached_uid;
                gid = cached_gid;
            }

            let mut atime: u32 = 0;
            let mut mtime: u32 = 0;
            let mut ctime: u32 = 0;
            let c_path3 = CString::new(fs_path).ok()?;
            unsafe {
                ext4_atime_get(c_path3.as_ptr() as *const c_char, &mut atime);
                ext4_mtime_get(c_path3.as_ptr() as *const c_char, &mut mtime);
                ext4_ctime_get(c_path3.as_ptr() as *const c_char, &mut ctime);
            }

            let file_type = mode & 0o170000;
            let is_link = file_type == 0o120000;
            let is_dir_mode = file_type == 0o040000;
            let is_regular_mode = file_type == 0 || file_type == 0o100000;

            let (inode_num, size, is_dir) = if is_dir_mode {
                let mut dir: ext4_dir = unsafe { core::mem::zeroed() };
                let c_path_dir = CString::new(fs_path).ok()?;
                let dir_rc =
                    unsafe { ext4_dir_open(&mut dir, c_path_dir.as_ptr() as *const c_char) };
                if dir_rc == EOK as i32 {
                    let inode_num = dir.f.inode;
                    let size = dir.f.fsize;
                    unsafe {
                        ext4_dir_close(&mut dir);
                    }
                    mode = (mode & 0o7777) | 0o040000;
                    (inode_num, size, true)
                } else {
                    return None;
                }
            } else if !is_regular_mode && raw_rc == EOK as i32 {
                let size = ((raw_inode.size_hi as u64) << 32) | raw_inode.size_lo as u64;
                (raw_ino, size, false)
            } else {
                let mut file: ext4_file = unsafe { core::mem::zeroed() };
                let c_path_file = CString::new(fs_path).ok()?;
                let flags = CString::new("rb").ok()?;
                let file_rc = unsafe {
                    ext4_fopen(
                        &mut file,
                        c_path_file.as_ptr() as *const c_char,
                        flags.as_ptr() as *const c_char,
                    )
                };

                if file_rc == EOK as i32 {
                    let inode_num = file.inode;
                    let size = unsafe { ext4_fsize(&mut file) };
                    unsafe {
                        ext4_fclose(&mut file);
                    }
                    (inode_num, size, false)
                } else if is_link {
                    // Symlink lookup only needs the link flag and path.
                    (0, 0, false)
                } else {
                    let mut dir: ext4_dir = unsafe { core::mem::zeroed() };
                    let c_path_dir = CString::new(fs_path).ok()?;
                    let dir_rc =
                        unsafe { ext4_dir_open(&mut dir, c_path_dir.as_ptr() as *const c_char) };
                    if dir_rc != EOK as i32 {
                        return None;
                    }
                    let inode_num = dir.f.inode;
                    let size = dir.f.fsize;
                    unsafe {
                        ext4_dir_close(&mut dir);
                    }
                    mode = (mode & 0o7777) | 0o040000;
                    (inode_num, size, true)
                }
            };

            Some(InodeInfo {
                inode_num,
                is_dir,
                is_link,
                mode,
                size,
                atime,
                mtime,
                ctime,
                blocks_count: 0,
                links_count,
                uid,
                gid,
            })
        })
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
                let read_rc = unsafe {
                    ext4_fread(
                        &mut file,
                        buf[total..total + chunk_len].as_mut_ptr() as *mut core::ffi::c_void,
                        chunk_len,
                        &mut rw_count,
                    )
                };
                if read_rc != EOK as i32 || rw_count == 0 {
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

    pub fn write_at(&self, fs_path: &str, offset: usize, buf: &[u8]) -> usize {
        self.write_at_raw(fs_path, offset, buf)
    }

    fn write_at_raw(&self, fs_path: &str, offset: usize, buf: &[u8]) -> usize {
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

            unsafe {
                ext4_fseek(&mut file, offset as i64, SEEK_SET as u32);
            }

            let mut rw_count: usize = 0;
            let write_rc = unsafe {
                ext4_fwrite(
                    &mut file,
                    buf.as_ptr() as *const core::ffi::c_void,
                    buf.len(),
                    &mut rw_count,
                )
            };
            unsafe {
                ext4_fclose(&mut file);
            }

            if write_rc != EOK as i32 {
                return 0;
            }
            rw_count
        })
    }

    pub fn create_file(&self, parent_path: &str, name: &str, is_dir: bool) -> Option<u32> {
        let full_path = if parent_path == "/" {
            format!("/{}", name)
        } else if parent_path.ends_with('/') {
            format!("{}{}", parent_path, name)
        } else {
            format!("{}/{}", parent_path, name)
        };

        self.with_fs(|_fs| {
            if is_dir {
                let c_path = CString::new(full_path.as_bytes()).ok()?;
                let rc = unsafe { ext4_dir_mk(c_path.as_ptr() as *const c_char) };
                if rc == EOK as i32 {
                    return Some(0);
                }
                None
            } else {
                let c_path = CString::new(full_path.as_bytes()).ok()?;
                let c_path = c_path.into_raw();
                let flags = CString::new("wb").ok()?;
                let flags_ptr = flags.into_raw();

                let mut file: ext4_file = unsafe { core::mem::zeroed() };
                let rc = unsafe { ext4_fopen(&mut file, c_path, flags_ptr) };
                unsafe {
                    drop(CString::from_raw(c_path));
                    drop(CString::from_raw(flags_ptr));
                }
                if rc == EOK as i32 {
                    let inode = file.inode;
                    unsafe {
                        ext4_fclose(&mut file);
                    }
                    Some(inode)
                } else {
                    None
                }
            }
        })
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
                return false;
            }

            let old_size = unsafe { ext4_fsize(&mut file) };
            let ok = if size <= old_size {
                (unsafe { ext4_ftruncate(&mut file, size) }) == EOK as i32
            } else {
                let zeros = [0u8; 4096];
                let mut pos = old_size as usize;
                let target = size as usize;
                unsafe {
                    ext4_fseek(&mut file, old_size as i64, SEEK_SET as u32);
                }
                let mut ok = true;
                while pos < target {
                    let chunk = (target - pos).min(zeros.len());
                    let mut written: usize = 0;
                    let rc = unsafe {
                        ext4_fwrite(
                            &mut file,
                            zeros[..chunk].as_ptr() as *const core::ffi::c_void,
                            chunk,
                            &mut written,
                        )
                    };
                    if rc != EOK as i32 || written == 0 {
                        ok = false;
                        break;
                    }
                    pos += written;
                }
                ok
            };
            unsafe {
                ext4_fclose(&mut file);
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
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                let r = unsafe { ext4_owner_set(c_path.as_ptr() as *const c_char, uid, gid) };
                if r == EOK as i32 {
                    record_path_owner(fs_path, uid, gid);
                }
                r
            } else {
                -1
            }
        })
    }

    pub fn fsync(&self, fs_path: &str) -> i32 {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe { ext4_cache_flush(c_path.as_ptr() as *const c_char) }
            } else {
                -1
            }
        })
    }

    pub fn sync_all(&self) -> i32 {
        self.with_fs(|_fs| {
            let root = CString::new("/").unwrap();
            unsafe { ext4_cache_flush(root.as_ptr() as *const c_char) }
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
    /// Cache updates use a two-phase protocol: create/reserve a replacement
    /// list before acquiring this lock, and never call Ext4 while it is held.
    pub children: VfsLock<Option<Vec<Arc<Dentry>>>>,
    /// This is only an in-memory metadata cache.  Ext4 lookup happens after
    /// dropping the lock, then the result is stored in a second short phase.
    inode_info: VfsLock<Option<InodeInfo>>,
}

impl Dentry {
    fn new_root(sb: Arc<VfsSuperBlock>) -> Arc<Self> {
        Arc::new(Dentry {
            name: String::from("/"),
            path: String::from("/"),
            fs_path: String::from("/"),
            inode_num: 2,
            is_dir: true,
            is_link: false,
            parent: None,
            super_block: sb,
            children: VfsLock::new_named(None, "DentryChildren"),
            inode_info: VfsLock::new_named(None, "DentryInodeInfo"),
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
            children: VfsLock::new_named(None, "DentryChildren"),
            inode_info: VfsLock::new_named(None, "DentryInodeInfo"),
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
        Arc::new(Dentry {
            name: String::from(name),
            path,
            fs_path,
            inode_num,
            is_dir,
            is_link,
            parent: Some(Arc::downgrade(parent)),
            super_block: parent.super_block.clone(),
            children: VfsLock::new_named(None, "DentryChildren"),
            inode_info: VfsLock::new_named(None, "DentryInodeInfo"),
        })
    }

    pub fn inode_info(&self) -> Option<InodeInfo> {
        let cached = { *self.inode_info.exclusive_access() };
        if let Some(info) = cached {
            return Some(info);
        }
        let info = self.super_block.get_inode_info(&self.fs_path)?;
        *self.inode_info.exclusive_access() = Some(info);
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

    pub fn invalidate_inode_info(&self) {
        *self.inode_info.exclusive_access() = None;
    }

    pub fn update_cached_size(&self, size: u64) {
        if let Some(ref mut info) = *self.inode_info.exclusive_access() {
            info.size = size;
            info.blocks_count = (size + 511) / 512;
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
    static ref TMPFS_MOUNTS: VfsLock<Vec<TmpfsMount>> =
        VfsLock::new_named(Vec::with_capacity(MOUNT_TABLE_CAPACITY), "VfsTmpfsMounts");
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

const DENTRY_CHILD_CACHE_LIMIT: usize = 128;

pub fn dentry_parent(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    dentry.parent.as_ref().and_then(Weak::upgrade)
}

fn vfs_parent(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    if let Some(mnt_point) = is_mount_root(dentry) {
        return dentry_parent(&mnt_point);
    }
    dentry_parent(dentry)
}

fn cache_child(parent: &Arc<Dentry>, child: Arc<Dentry>) {
    // Reserve a complete cache line before taking the Dentry lock.  A second
    // lookup below handles a racing initializer without calling the allocator
    // while holding a VfsLock.
    if parent.children.exclusive_access().is_none() {
        let mut initial = Vec::with_capacity(DENTRY_CHILD_CACHE_LIMIT);
        initial.push(child.clone());
        let mut children = parent.children.exclusive_access();
        if children.is_none() {
            *children = Some(initial);
            return;
        }
    }

    let mut children = parent.children.exclusive_access();
    let list = children
        .as_mut()
        .expect("Dentry child cache initialized before use");
    debug_assert!(list.capacity() >= DENTRY_CHILD_CACHE_LIMIT);

    if let Some(slot) = list.iter_mut().find(|cached| cached.name == child.name) {
        *slot = child;
    } else {
        if list.len() >= DENTRY_CHILD_CACHE_LIMIT {
            if let Some(idx) = list
                .iter()
                .position(|cached| Arc::strong_count(cached) == 1)
            {
                list.remove(idx);
            }
        }
        if list.len() < DENTRY_CHILD_CACHE_LIMIT {
            list.push(child);
        }
    }
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

    let cached = {
        let children = parent.children.exclusive_access();
        children
            .as_ref()
            .and_then(|list| list.iter().find(|cached| cached.name == name).cloned())
    };
    if let Some(cached) = cached {
        if let Some(mnt_root) = lookup_mount(&cached) {
            return Some(mnt_root);
        }
        return Some(cached);
    }

    let child_fs_path = if parent.fs_path == "/" {
        format!("/{}", name)
    } else if parent.fs_path.ends_with('/') {
        format!("{}{}", parent.fs_path, name)
    } else {
        format!("{}/{}", parent.fs_path, name)
    };

    let info = parent.super_block.get_inode_info(&child_fs_path)?;
    let (child_inode, is_dir, is_link) = (info.inode_num, info.is_dir, info.is_link);

    let child = Dentry::new_child(parent, name, child_inode, is_dir, is_link);
    child.set_cached_inode_info(info);

    cache_child(parent, child.clone());

    if let Some(mnt_root) = lookup_mount(&child) {
        return Some(mnt_root);
    }

    Some(child)
}

pub fn vfs_resolve_path(start: &Arc<Dentry>, path: &str) -> Option<Arc<Dentry>> {
    let path = path.trim_matches('\0');
    if path.is_empty() {
        return None;
    }

    let is_abs = path.starts_with('/');
    let mut current = if is_abs {
        crate::task::current_task()
            .map(|t| t.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| ROOT_DENTRY.clone())
    } else {
        start.clone()
    };

    for component in path.split('/') {
        match component {
            "" | "." => continue,
            _ => {
                current = vfs_lookup(&current, component)?;
            }
        }
    }

    lookup_mount(&current).or(Some(current))
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

    let is_abs = path.starts_with('/');
    let mut current = if is_abs {
        crate::task::current_task()
            .map(|t| t.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| ROOT_DENTRY.clone())
    } else {
        start.clone()
    };

    let components: Vec<&str> = path
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    if components.is_empty() {
        return None;
    }

    let filename = String::from(*components.last().unwrap());
    for i in 0..components.len() - 1 {
        if !current.is_dir {
            return None;
        }
        current = vfs_lookup(&current, components[i])?;
    }
    if !current.is_dir {
        return None;
    }

    Some((current, filename))
}

// ==================== File Creation ====================

pub fn vfs_create(
    parent: &Arc<Dentry>,
    name: &str,
    is_dir: bool,
    mode: u32,
) -> Option<Arc<Dentry>> {
    let inode_num = parent
        .super_block
        .create_file(&parent.fs_path, name, is_dir)?;

    let child = Dentry::new_child(parent, name, inode_num, is_dir, false);

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
    let fs_path = child.fs_path.clone();
    child.super_block.set_owner(&fs_path, uid, gid);
    child.super_block.set_mode(&fs_path, effective_mode);
    if let Some(info) = child.super_block.get_inode_info(&fs_path) {
        child.set_cached_inode_info(info);
    }

    cache_child(parent, child.clone());

    Some(child)
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
        VfsSuperBlock::new_ext4(ROOT_BLOCK_DEVICE.clone(), "ext4")
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

/// Mount the test disk (bus 0) at the specified mount point dentry.
pub fn mount_test_disk_at(mnt_point: Arc<Dentry>) -> bool {
    let test_sb = match VfsSuperBlock::new_ext4(BLOCK_DEVICE.clone(), "ext4") {
        Some(sb) => sb,
        None => return false,
    };
    vfs_mount(mnt_point, test_sb, "/__mount_vda/")
}

pub fn list_apps() {
    println!("/**** APPS ****");
    ROOT_SB
        .dir_get_entries("/bin")
        .iter()
        .filter(|e| {
            let name = e.get_name();
            name != "." && name != ".." && !name.is_empty()
        })
        .for_each(|e| println!("{}", e.get_name()));
    println!("**************/");
}
