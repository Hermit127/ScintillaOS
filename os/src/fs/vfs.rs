use crate::sync::UPSafeCell;
use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_char;
use lazy_static::*;

use super::block_device_stream::{BlockDeviceStream, BlockDeviceStreamOp, Ext4Wrapper};
use lwext4_rust::bindings::*;
use lwext4_rust::Ext4BlockWrapper;

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
    pub fs_data: UPSafeCell<FsInner>,
}

impl VfsSuperBlock {
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
        const DISK_SIZE_512M: u64 = 512 * 1024 * 1024;
        let stream = BlockDeviceStream {
            inner: blk_device,
            pos: 0,
            total_size: DISK_SIZE_512M,
        };
        let ext4 = Ext4BlockWrapper::<BlockDeviceStreamOp>::new_with_name(
            stream,
            "ext4_fs",
            mount_point,
        ).ok()?;
        Some(Arc::new(VfsSuperBlock {
            fs_type,
            fs_data: unsafe { UPSafeCell::new_named(FsInner::Ext4(Ext4Wrapper(ext4)), "VfsFsData") },
        }))
    }

    fn with_fs<R>(&self, f: impl FnOnce(&Ext4Wrapper) -> R) -> R {
        let data = self.fs_data.exclusive_access();
        match &*data {
            FsInner::Ext4(wrapper) => f(wrapper),
        }
    }

    pub fn force_umount(&self) {
        let mut data = self.fs_data.exclusive_access();
        if let FsInner::Ext4(ref mut wrapper) = *data {
            unsafe {
                wrapper.0.lwext4_umount().ok();
            }
        }
    }

    // ---------- path-based ext4 operations ----------

    pub fn dir_get_entries(&self, fs_path: &str) -> Vec<DirEntry> {
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
                    let de = ext4_dir_entry_next(&mut d);
                    if de.is_null() {
                        break;
                    }
                    let dentry = &*de;
                    let name_len = dentry.name_length as usize;
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
        self.with_fs(|_fs| {
            let c_path = CString::new(fs_path).ok()?;
            let c_path = c_path.into_raw();

            let mut file: ext4_file = unsafe { core::mem::zeroed() };
            let flags = CString::new("rb").ok()?;
            let flags_ptr = flags.into_raw();

            let rc = unsafe { ext4_fopen(&mut file, c_path, flags_ptr) };
            unsafe {
                drop(CString::from_raw(c_path));
                drop(CString::from_raw(flags_ptr));
            }

            if rc != EOK as i32 {
                return None;
            }

            let inode_num = file.inode;
            let size = unsafe { ext4_fsize(&mut file) };

            let mut mode: u32 = 0;
            let c_path2 = CString::new(fs_path).ok()?;
            let _mode_rc = unsafe { ext4_mode_get(c_path2.as_ptr() as *const c_char, &mut mode) };

            let mut uid: u32 = 0;
            let mut gid: u32 = 0;
            let c_path_owner = CString::new(fs_path).ok()?;
            unsafe {
                ext4_owner_get(c_path_owner.as_ptr() as *const c_char, &mut uid, &mut gid);
            }

            let is_link = mode & 0xA000 == 0xA000;

            let mut atime: u32 = 0;
            let mut mtime: u32 = 0;
            let mut ctime: u32 = 0;
            let c_path3 = CString::new(fs_path).ok()?;
            unsafe {
                ext4_atime_get(c_path3.as_ptr() as *const c_char, &mut atime);
                ext4_mtime_get(c_path3.as_ptr() as *const c_char, &mut mtime);
                ext4_ctime_get(c_path3.as_ptr() as *const c_char, &mut ctime);
            }

            unsafe {
                ext4_fclose(&mut file);
            }

            let mut is_dir = (mode & 0x4000) != 0;
            if !is_dir {
                let mut dir: ext4_dir = unsafe { core::mem::zeroed() };
                let c_path_dir = CString::new(fs_path).ok()?;
                let rc = unsafe { ext4_dir_open(&mut dir, c_path_dir.as_ptr() as *const c_char) };
                if rc == EOK as i32 {
                    is_dir = true;
                    mode = (mode & 0o7777) | 0o040000;
                    unsafe {
                        ext4_dir_close(&mut dir);
                    }
                }
            }
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
                links_count: 0,
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
                let chunk_len = core::cmp::min(128, max_read - total);
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

            let ok = unsafe { ext4_ftruncate(&mut file, size) } == EOK as i32;
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
    pub parent: Option<Arc<Dentry>>,
    pub super_block: Arc<VfsSuperBlock>,
    pub children: UPSafeCell<Option<Vec<Arc<Dentry>>>>,
}

impl Dentry {
    fn new_root(sb: Arc<VfsSuperBlock>) -> Arc<Self> {
        Arc::new(Dentry {
            name: String::from("/"),
            path: String::from("/"),
            fs_path: String::from("/"),
            inode_num: 2,
            is_dir: true,
            parent: None,
            super_block: sb,
            children: unsafe { UPSafeCell::new(None) },
        })
    }

    fn new_mount_root(sb: Arc<VfsSuperBlock>, mnt_path: String, lwext4_mount: &str) -> Arc<Self> {
        Arc::new(Dentry {
            name: String::from("/"),
            path: mnt_path,
            fs_path: String::from(lwext4_mount),
            inode_num: 2,
            is_dir: true,
            parent: None,
            super_block: sb,
            children: unsafe { UPSafeCell::new(None) },
        })
    }

    fn new_child(parent: &Arc<Dentry>, name: &str, inode_num: u32, is_dir: bool) -> Arc<Self> {
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
            parent: Some(parent.clone()),
            super_block: parent.super_block.clone(),
            children: unsafe { UPSafeCell::new(None) },
        })
    }
}

// ==================== Mount ====================

pub struct Mount {
    pub mnt_point: Arc<Dentry>,
    pub mnt_root: Arc<Dentry>,
}

lazy_static! {
    pub static ref MOUNT_TABLE: UPSafeCell<Vec<Mount>> = unsafe { UPSafeCell::new(Vec::new()) };
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

fn vfs_parent(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    if let Some(mnt_point) = is_mount_root(dentry) {
        return mnt_point.parent.clone();
    }
    dentry.parent.clone()
}

pub fn vfs_lookup(parent: &Arc<Dentry>, name: &str) -> Option<Arc<Dentry>> {
    match name {
        "." => return Some(parent.clone()),
        ".." => return vfs_parent(parent).or_else(|| Some(parent.clone())),
        _ => {}
    }

    {
        let children = parent.children.exclusive_access();
        if let Some(list) = children.as_ref() {
            if let Some(cached) = list.iter().find(|c| c.name == name) {
                if let Some(mnt_root) = lookup_mount(cached) {
                    return Some(mnt_root);
                }
                return Some(cached.clone());
            }
        }
    }

    let child_fs_path = if parent.fs_path == "/" {
        format!("/{}", name)
    } else {
        format!("{}/{}", parent.fs_path, name)
    };

    let (child_inode, is_dir) = {
        let entries = parent.super_block.dir_get_entries(&parent.fs_path);
        let found = entries.iter().find(|e| e.get_name() == name)?;
        let inode_num = found.inode;
        let info = parent.super_block.get_inode_info(&child_fs_path);
        let is_dir = match &info {
            Some(info) => info.is_dir,
            None => found.de_type == 2,
        };
        (inode_num, is_dir)
    };

    let child = Dentry::new_child(parent, name, child_inode, is_dir);

    {
        let mut children = parent.children.exclusive_access();
        if let Some(list) = children.as_mut() {
            list.push(child.clone());
        } else {
            *children = Some(vec![child.clone()]);
        }
    }

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
        current = vfs_lookup(&current, components[i])?;
    }

    Some((current, filename))
}

// ==================== File Creation ====================

pub fn vfs_create(parent: &Arc<Dentry>, name: &str, is_dir: bool, mode: u32) -> Option<Arc<Dentry>> {
    let inode_num = parent
        .super_block
        .create_file(&parent.fs_path, name, is_dir)?;

    let child = Dentry::new_child(parent, name, inode_num, is_dir);

    let umask = crate::task::current_task()
        .map(|t| t.inner_exclusive_access().umask)
        .unwrap_or(0o22);
    let file_type = if is_dir { 0o040000 } else { 0o100000 };
    let effective_mode = file_type | (mode & !umask & 0o7777);
    let fs_path = child.fs_path.clone();
    child.super_block.set_mode(&fs_path, effective_mode);

    {
        let mut children = parent.children.exclusive_access();
        if let Some(list) = children.as_mut() {
            list.push(child.clone());
        } else {
            *children = Some(vec![child.clone()]);
        }
    }

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
        {
            return false;
        }
    }

    let mnt_root = Dentry::new_mount_root(sb, mnt_point.path.clone(), fs_path);

    MOUNT_TABLE.exclusive_access().push(Mount {
        mnt_point: mnt_point.clone(),
        mnt_root: mnt_root.clone(),
    });

    true
}

pub fn vfs_umount(mnt_point: &Arc<Dentry>) -> bool {
    let path = &mnt_point.path;
    let mut table = MOUNT_TABLE.exclusive_access();
    if let Some(idx) = table.iter().position(|m| &m.mnt_point.path == path) {
        table.remove(idx);
        true
    } else {
        false
    }
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
    vfs_mount(mnt_point, test_sb, "/mp/")
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
