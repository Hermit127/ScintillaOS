use crate::sync::UPSafeCell;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ext4_rs::{Ext4, InodeFileType};
use lazy_static::*;

// ==================== VfsSuperBlock ====================

pub enum FsInner {
    Ext4(Ext4),
}

pub struct VfsSuperBlock {
    pub fs_type: &'static str,
    pub fs_data: UPSafeCell<FsInner>,
}

impl VfsSuperBlock {
    pub fn new_ext4(
        blk_device: Arc<dyn crate::drivers::BlockDevice>,
        fs_type: &'static str,
    ) -> Arc<Self> {
        let adapter = Arc::new(super::inode::Ext4BlockDeviceAdapter {
            inner: blk_device,
        });
        let cache = Arc::new(super::block_cache::BlockCache::new(adapter));
        let ext4 = Ext4::open(cache);
        Arc::new(VfsSuperBlock {
            fs_type,
            fs_data: unsafe { UPSafeCell::new(FsInner::Ext4(ext4)) },
        })
    }

    pub fn with_ext4<R>(&self, f: impl FnOnce(&Ext4) -> R) -> R {
        let data = self.fs_data.exclusive_access();
        match &*data {
            FsInner::Ext4(ext4) => f(ext4),
        }
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

    fn new_mount_root(sb: Arc<VfsSuperBlock>, mnt_path: String) -> Arc<Self> {
        Arc::new(Dentry {
            name: String::from("/"),
            path: mnt_path,
            fs_path: String::from("/"),
            inode_num: 2,
            is_dir: true,
            parent: None,
            super_block: sb,
            children: unsafe { UPSafeCell::new(None) },
        })
    }

    fn new_child(
        parent: &Arc<Dentry>,
        name: &str,
        inode_num: u32,
        is_dir: bool,
    ) -> Arc<Self> {
        let path = if parent.path == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", parent.path, name)
        };
        let fs_path = if parent.fs_path == "/" {
            format!("/{}", name)
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
    pub static ref MOUNT_TABLE: UPSafeCell<Vec<Mount>> = unsafe {
        UPSafeCell::new(Vec::new())
    };
}

fn lookup_mount(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    let table = MOUNT_TABLE.exclusive_access();
    for m in table.iter() {
        if Arc::ptr_eq(&m.mnt_point, dentry) {
            return Some(m.mnt_root.clone());
        }
    }
    None
}

fn is_mount_root(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    let table = MOUNT_TABLE.exclusive_access();
    for m in table.iter() {
        if Arc::ptr_eq(&m.mnt_root, dentry) {
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

    let (child_inode, is_dir) = parent.super_block.with_ext4(|ext4| {
        let entries = ext4.ext4_dir_get_entries(parent.inode_num);
        let found = entries.iter().find(|e| e.get_name() == name)?;
        let inode_ref = ext4.get_inode_ref(found.inode as u32);
        Some((found.inode as u32, inode_ref.inode.is_dir()))
    })?;

    let child = Dentry::new_child(parent, name, child_inode, is_dir);

    {
        let mut children = parent.children.exclusive_access();
        if let Some(list) = children.as_mut() {
            list.push(child.clone());
        } else {
            *children = Some(alloc::vec![child.clone()]);
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

    Some(current)
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

    let components: Vec<&str> =
        path.split('/')
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

pub fn vfs_create(parent: &Arc<Dentry>, name: &str, is_dir: bool, uid: u32, gid: u32) -> Option<Arc<Dentry>> {
    let file_type = if is_dir {
        InodeFileType::S_IFDIR
    } else {
        InodeFileType::S_IFREG
    };

    let inode_num = parent.super_block.with_ext4(|ext4| {
        ext4
            .create_with_attr(
                parent.inode_num,
                name,
                file_type.bits(),
                uid as u16,
                gid as u16,
            )
            .ok()
            .map(|r| r.inode_num)
    })?;

    let child = Dentry::new_child(parent, name, inode_num, is_dir);

    {
        let mut children = parent.children.exclusive_access();
        if let Some(list) = children.as_mut() {
            list.push(child.clone());
        } else {
            *children = Some(alloc::vec![child.clone()]);
        }
    }

    Some(child)
}

// ==================== Mount Operations ====================

pub fn vfs_mount(mnt_point: Arc<Dentry>, sb: Arc<VfsSuperBlock>) -> bool {
    let path = mnt_point.path.clone();
    {
        let table = MOUNT_TABLE.exclusive_access();
        if table.iter().any(|m| m.mnt_point.path == path && path != "/") {
            return false;
        }
    }

    let mnt_root = Dentry::new_mount_root(sb, mnt_point.path.clone());

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
    let test_sb = VfsSuperBlock::new_ext4(BLOCK_DEVICE.clone(), "ext4");
    vfs_mount(mnt_point, test_sb)
}

pub fn list_apps() {
    println!("/**** APPS ****");
    ROOT_SB.with_ext4(|ext4| {
        if let Ok(bin_ino) = ext4.ext4_dir_open("/bin") {
            for entry in ext4.ext4_dir_get_entries(bin_ino) {
                let name = entry.get_name();
                if name != "." && name != ".." && !name.is_empty() {
                    println!("{}", name);
                }
            }
        }
    });
    println!("**************/");
}
