use super::vfs::{
    vfs_create, vfs_rename, vfs_resolve_parent_and_name, vfs_resolve_path,
    vfs_resolve_with_fallback, vfs_symlink, Dentry,
};
use super::{File, Kstat};
use crate::drivers::BlockDevice;
use crate::sync::UPSafeCell;
use crate::task::current_task;
use alloc::string::String;
use alloc::sync::Arc;
use bitflags::*;

pub struct Ext4BlockDeviceAdapter {
    pub inner: Arc<dyn BlockDevice>,
}

impl ext4_rs::BlockDevice for Ext4BlockDeviceAdapter {
    fn read_offset(&self, offset: usize) -> alloc::vec::Vec<u8> {
        const SECTOR_SIZE: usize = 512;
        const EXT4_BLOCK_SIZE: usize = ext4_rs::BLOCK_SIZE;
        let sector_start = offset / SECTOR_SIZE;
        let sector_offset = offset % SECTOR_SIZE;
        let raw_len = sector_offset + EXT4_BLOCK_SIZE;
        let sector_count = (raw_len + SECTOR_SIZE - 1) / SECTOR_SIZE;
        let mut raw = alloc::vec![0u8; sector_count * SECTOR_SIZE];
        for i in 0..sector_count {
            let begin = i * SECTOR_SIZE;
            self.inner
                .read_block(sector_start + i, &mut raw[begin..begin + SECTOR_SIZE]);
        }
        raw[sector_offset..sector_offset + EXT4_BLOCK_SIZE].to_vec()
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        const SECTOR_SIZE: usize = 512;
        let sector_start = offset / SECTOR_SIZE;
        let sector_offset = offset % SECTOR_SIZE;
        let raw_len = sector_offset + data.len();
        let sector_count = (raw_len + SECTOR_SIZE - 1) / SECTOR_SIZE;
        let mut raw = alloc::vec![0u8; sector_count * SECTOR_SIZE];
        for i in 0..sector_count {
            let begin = i * SECTOR_SIZE;
            self.inner
                .read_block(sector_start + i, &mut raw[begin..begin + SECTOR_SIZE]);
        }
        raw[sector_offset..sector_offset + data.len()].copy_from_slice(data);
        for i in 0..sector_count {
            let begin = i * SECTOR_SIZE;
            self.inner
                .write_block(sector_start + i, &raw[begin..begin + SECTOR_SIZE]);
        }
    }
}

// ==================== OSInode ====================

pub struct OSInode {
    readable: bool,
    writable: bool,
    dentry: Arc<Dentry>,
    inner: UPSafeCell<OSInodeInner>,
}

pub struct OSInodeInner {
    offset: usize,
}

impl OSInode {
    pub fn new(readable: bool, writable: bool, dentry: Arc<Dentry>) -> Self {
        Self {
            readable,
            writable,
            dentry,
            inner: unsafe { UPSafeCell::new(OSInodeInner { offset: 0 }) },
        }
    }

    pub fn read_all(&self) -> alloc::vec::Vec<u8> {
        let mut inner = self.inner.exclusive_access();
        let mut buffer = [0u8; 512];
        let mut out = alloc::vec::Vec::new();
        loop {
            let len = self.dentry.super_block.with_ext4(|ext4| {
                ext4
                    .read_at(self.dentry.inode_num, inner.offset, &mut buffer)
                    .ok()
                    .unwrap_or(0)
            });
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
        let inode_ref = self
            .dentry
            .super_block
            .with_ext4(|ext4| ext4.get_inode_ref(self.dentry.inode_num));
        let inode = inode_ref.inode;
        let mut st = Kstat::new(inode.mode() as u32);
        st.st_ino = inode_ref.inode_num as u64;
        st.st_size = inode.size() as i64;
        st.st_blksize = ext4_rs::BLOCK_SIZE as u32;
        st.st_blocks = inode.blocks_count();
        st.st_atime_sec = inode.atime() as i64;
        st.st_mtime_sec = inode.mtime() as i64;
        st.st_ctime_sec = inode.ctime() as i64;
        st.st_uid = inode.uid() as u32;
        st.st_gid = inode.gid() as u32;
        drop(inner);
        st
    }

    fn align_up(value: usize, align: usize) -> usize {
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
        const DIRECTORY = 0x10000;
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

fn check_file_permission(
    dentry: &Arc<Dentry>,
    euid: u32,
    egid: u32,
    want_read: bool,
    want_write: bool,
) -> bool {
    if euid == 0 {
        return true;
    }
    dentry.super_block.with_ext4(|ext4| {
        let inode_ref = ext4.get_inode_ref(dentry.inode_num);
        let mode = inode_ref.inode.mode();
        let owner_perm = (mode >> 6) & 0o7;
        let group_perm = (mode >> 3) & 0o7;
        let other_perm = mode & 0o7;
        let perm = if inode_ref.inode.uid() as u32 == euid {
            owner_perm
        } else if inode_ref.inode.gid() as u32 == egid {
            group_perm
        } else {
            other_perm
        };
        if want_read && (perm & 0o4) == 0 {
            return false;
        }
        if want_write && (perm & 0o2) == 0 {
            return false;
        }
        true
    })
}

pub fn open_file(path: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    open_file_at(None, path, flags)
}

pub fn open_file_at(
    base: Option<Arc<Dentry>>,
    path: &str,
    flags: OpenFlags,
) -> Option<Arc<OSInode>> {
    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let want_dir = flags.directory_requested();

    if flags.create_requested() {
        if want_dir {
            return None;
        }

        // 文件已存在？
        if let Some(dentry) = vfs_resolve_with_fallback(&start, path) {
            if !dentry.is_dir {
                if flags.trunc_requested() {
                    let truncated = dentry.super_block.with_ext4(|ext4| {
                        let mut inode_ref = ext4.get_inode_ref(dentry.inode_num);
                        ext4.truncate_inode(&mut inode_ref, 0).is_ok()
                    });
                    if !truncated {
                        return None;
                    }
                }
                let (readable, writable) = flags.read_write();
                let (euid, egid) = current_task()
                    .map(|t| {
                        let inner = t.inner_exclusive_access();
                        (inner.euid, inner.egid)
                    })
                    .unwrap_or((0, 0));
                if !check_file_permission(&dentry, euid, egid, readable, writable) {
                    return None;
                }
                return Some(Arc::new(OSInode::new(readable, writable, dentry)));
            }
        }

        // 新建文件
        let (parent, name) =
            vfs_resolve_parent_and_name(&start, path)?;
        let (euid, egid) = current_task()
            .map(|t| {
                let inner = t.inner_exclusive_access();
                (inner.euid, inner.egid)
            })
            .unwrap_or((0, 0));
        let dentry = vfs_create(&parent, &name, false, euid, egid)?;
        let (readable, writable) = flags.read_write();
        return Some(Arc::new(OSInode::new(readable, writable, dentry)));
    }

    // 普通打开
    let dentry = vfs_resolve_with_fallback(&start, path)?;
    if want_dir && !dentry.is_dir {
        return None;
    }

    if flags.trunc_requested() && !dentry.is_dir {
        let truncated = dentry.super_block.with_ext4(|ext4| {
            let mut inode_ref = ext4.get_inode_ref(dentry.inode_num);
            ext4.truncate_inode(&mut inode_ref, 0).is_ok()
        });
        if !truncated {
            return None;
        }
    }

    let (readable, writable) = if dentry.is_dir {
        (false, false)
    } else {
        flags.read_write()
    };
    let (euid, egid) = current_task()
        .map(|t| {
            let inner = t.inner_exclusive_access();
            (inner.euid, inner.egid)
        })
        .unwrap_or((0, 0));
    if !check_file_permission(&dentry, euid, egid, readable, writable) {
        return None;
    }
    Some(Arc::new(OSInode::new(readable, writable, dentry)))
}

pub fn mkdir_at(base: Option<Arc<Dentry>>, path: &str) -> bool {
    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());

    // 已经存在且是目录 → 幂等成功
    if let Some(existing) = vfs_resolve_path(&start, path) {
        return existing.is_dir;
    }

    let (parent, name) = match vfs_resolve_parent_and_name(&start, path) {
        Some(x) => x,
        None => return false,
    };

    let (euid, egid) = current_task()
        .map(|t| {
            let inner = t.inner_exclusive_access();
            (inner.euid, inner.egid)
        })
        .unwrap_or((0, 0));
    vfs_create(&parent, &name, true, euid, egid).is_some()
}

pub fn symlink_at(base: Option<Arc<Dentry>>, target: &str, linkpath: &str) -> bool {
    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let (parent, name) = match vfs_resolve_parent_and_name(&start, linkpath) {
        Some(x) => x,
        None => return false,
    };
    let (euid, egid) = current_task()
        .map(|t| {
            let inner = t.inner_exclusive_access();
            (inner.euid, inner.egid)
        })
        .unwrap_or((0, 0));
    vfs_symlink(&parent, &name, target, euid, egid).is_some()
}

pub fn rename_at(
    old_base: Option<Arc<Dentry>>,
    oldpath: &str,
    new_base: Option<Arc<Dentry>>,
    newpath: &str,
) -> bool {
    let start_old = old_base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let start_new = new_base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());

    let old_dentry = match vfs_resolve_path(&start_old, oldpath) {
        Some(d) => d,
        None => return false,
    };

    let (new_parent, new_name) = match vfs_resolve_parent_and_name(&start_new, newpath) {
        Some(x) => x,
        None => return false,
    };

    vfs_rename(&old_dentry, &new_parent, &new_name)
}

pub fn unlink_at(base: Option<Arc<Dentry>>, path: &str) -> bool {
    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let dentry = match vfs_resolve_path(&start, path) {
        Some(d) => d,
        None => return false,
    };

    if dentry.is_dir {
        return false;
    }

    let parent = match dentry.parent.clone() {
        Some(p) => p,
        None => return false,
    };

    let ok = dentry.super_block.with_ext4(|ext4| {
        let mut child_inode_ref = ext4.get_inode_ref(dentry.inode_num);
        let child_link_cnt = child_inode_ref.inode.links_count();
        let child_size = child_inode_ref.inode.size();
        if child_link_cnt == 1 && child_size > 0 {
            if ext4.truncate_inode(&mut child_inode_ref, 0).is_err() {
                return false;
            }
        }

        let mut parent_inode_ref = ext4.get_inode_ref(parent.inode_num);
        ext4
            .unlink(&mut parent_inode_ref, &mut child_inode_ref, &dentry.name)
            .is_ok()
    });

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

    fn read(&self, buf: &mut [u8]) -> usize {
        let mut inner = self.inner.exclusive_access();
        let read_size = self.dentry.super_block.with_ext4(|ext4| {
            ext4
                .read_at(self.dentry.inode_num, inner.offset, buf)
                .ok()
                .unwrap_or(0)
        });
        inner.offset += read_size;
        read_size
    }

    fn write(&self, buf: &mut [u8]) -> usize {
        let mut inner = self.inner.exclusive_access();
        let write_size = self.dentry.super_block.with_ext4(|ext4| {
            ext4
                .write_at(self.dentry.inode_num, inner.offset, buf)
                .ok()
                .unwrap_or(0)
        });
        inner.offset += write_size;
        write_size
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

        let inode_is_dir = self.dentry.super_block.with_ext4(|ext4| {
            ext4.get_inode_ref(self.dentry.inode_num).inode.is_dir()
        });
        if !inode_is_dir {
            return -1;
        }

        let entries = self.dentry.super_block.with_ext4(|ext4| {
            ext4.ext4_dir_get_entries(self.dentry.inode_num)
        });

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
        self.inner.exclusive_access().offset = offset;
    }

    fn get_offset(&self) -> usize {
        self.inner.exclusive_access().offset
    }
}
