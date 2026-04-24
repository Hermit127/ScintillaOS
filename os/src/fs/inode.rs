use super::{File, Kstat};
use crate::drivers::{BlockDevice, BLOCK_DEVICE};
use crate::sync::UPSafeCell;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bitflags::*;
use ext4_rs::{Ext4, InodeFileType};
use lazy_static::*;

struct Ext4BlockDeviceAdapter {
    inner: Arc<dyn BlockDevice>,
}

impl ext4_rs::BlockDevice for Ext4BlockDeviceAdapter {
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        const SECTOR_SIZE: usize = 512;
        const EXT4_BLOCK_SIZE: usize = ext4_rs::BLOCK_SIZE;
        let sector_start = offset / SECTOR_SIZE;
        let sector_offset = offset % SECTOR_SIZE;
        let raw_len = sector_offset + EXT4_BLOCK_SIZE;
        let sector_count = (raw_len + SECTOR_SIZE - 1) / SECTOR_SIZE;
        let mut raw = vec![0u8; sector_count * SECTOR_SIZE];
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
        let mut raw = vec![0u8; sector_count * SECTOR_SIZE];
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

lazy_static! {
    static ref EXT4_FS: UPSafeCell<Ext4> = {
        let device = Arc::new(Ext4BlockDeviceAdapter {
            inner: BLOCK_DEVICE.clone(),
        });
        unsafe { UPSafeCell::new(Ext4::open(device)) }
    };
}

fn with_ext4<R>(f: impl FnOnce(&Ext4) -> R) -> R {
    let fs = EXT4_FS.exclusive_access();
    f(&fs)
}

fn trim_cstr_suffix(path: &str) -> &str {
    path.trim_end_matches('\0')
}

fn normalize_path(base_dir: Option<&str>, path: &str) -> Option<String> {
    let path = trim_cstr_suffix(path);
    if path.is_empty() {
        return None;
    }

    let mut parts: Vec<&str> = Vec::new();
    let is_abs = path.starts_with('/');
    if !is_abs {
        for comp in base_dir.unwrap_or("/").split('/') {
            match comp {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                _ => parts.push(comp),
            }
        }
    }

    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(comp),
        }
    }

    if parts.is_empty() {
        return Some("/".to_string());
    }
    Some(format!("/{}", parts.join("/")))
}

fn candidate_paths(base_dir: Option<&str>, path: &str, want_dir: bool) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(primary) = normalize_path(base_dir, path) {
        candidates.push(primary.clone());
        let path = trim_cstr_suffix(path).trim_start_matches("./");
        if base_dir.is_none()
            && !path.starts_with('/')
            && !path.contains('/')
            && path != "."
            && path != ".."
            && !want_dir
        {
            let bin_fallback = format!("/bin/{}", path);
            if bin_fallback != primary {
                candidates.push(bin_fallback);
            }
        }
    }
    candidates
}

fn lookup_existing(base_dir: Option<&str>, path: &str, want_dir: bool) -> Option<(u32, String, bool)> {
    for candidate in candidate_paths(base_dir, path, want_dir) {
        // ext4_rs::generic_open("/") may return an invalid inode number.
        // Handle root path explicitly to preserve UNIX-like "."/"/" behavior.
        if candidate == "/" {
            return Some((2, candidate, true));
        }
        let inode = match with_ext4(|fs| {
            let mut parent = 2u32;
            let mut off = 0u32;
            fs.generic_open(
                candidate.as_str(),
                &mut parent,
                false,
                InodeFileType::S_IFREG.bits(),
                &mut off,
            )
            .ok()
        }) {
            Some(inode) => inode,
            None => continue,
        };

        let inode_ref = with_ext4(|fs| fs.get_inode_ref(inode));
        let is_dir = inode_ref.inode.is_dir();
        if want_dir && !is_dir {
            continue;
        }
        return Some((inode, candidate, is_dir));
    }
    None
}

pub struct OSInode {
    readable: bool,
    writable: bool,
    inner: UPSafeCell<OSInodeInner>,
}

pub struct OSInodeInner {
    offset: usize,
    inode_num: u32,
    path: String,
    is_dir: bool,
}

impl OSInode {
    pub fn new(readable: bool, writable: bool, inode_num: u32, path: String, is_dir: bool) -> Self {
        Self {
            readable,
            writable,
            inner: unsafe {
                UPSafeCell::new(OSInodeInner {
                    offset: 0,
                    inode_num,
                    path,
                    is_dir,
                })
            },
        }
    }

    pub fn read_all(&self) -> Vec<u8> {
        let mut inner = self.inner.exclusive_access();
        let mut buffer = [0u8; 512];
        let mut out = Vec::new();
        loop {
            let len = with_ext4(|fs| fs.read_at(inner.inode_num, inner.offset, &mut buffer).ok())
                .unwrap_or(0);
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
        let inode_ref = with_ext4(|fs| fs.get_inode_ref(inner.inode_num));
        let inode = inode_ref.inode;
        let mut st = Kstat::new(inode.mode() as u32);
        st.st_ino = inode_ref.inode_num as u64;
        st.st_size = inode.size() as i64;
        st.st_blksize = ext4_rs::BLOCK_SIZE as u32;
        st.st_blocks = inode.blocks_count();
        st.st_atime_sec = inode.atime() as i64;
        st.st_mtime_sec = inode.mtime() as i64;
        st.st_ctime_sec = inode.ctime() as i64;
        st
    }

    fn align_up(value: usize, align: usize) -> usize {
        (value + align - 1) & !(align - 1)
    }
}

pub fn list_apps() {
    println!("/**** APPS ****");
    with_ext4(|fs| {
        if let Ok(bin_ino) = fs.ext4_dir_open("/bin") {
            for entry in fs.ext4_dir_get_entries(bin_ino) {
                let name = entry.get_name();
                if name != "." && name != ".." && !name.is_empty() {
                    println!("{}", name);
                }
            }
        }
    });
    println!("**************/");
}

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

    /// Return (readable, writable).
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

pub fn open_file(path: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    open_file_at(None, path, flags)
}

pub fn mkdir_at(base_dir: Option<&str>, path: &str) -> bool {
    let normalized = match normalize_path(base_dir, path) {
        Some(path) if path != "/" => path,
        _ => return false,
    };
    if lookup_existing(None, normalized.as_str(), false).is_some() {
        return false;
    }
    with_ext4(|fs| {
        let mut parent = 2u32;
        let mut off = 0u32;
        fs.generic_open(
            normalized.as_str(),
            &mut parent,
            true,
            InodeFileType::S_IFDIR.bits(),
            &mut off,
        )
        .is_ok()
    })
}

pub fn unlink_at(base_dir: Option<&str>, path: &str) -> bool {
    let normalized = match normalize_path(base_dir, path) {
        Some(path) if path != "/" => path,
        _ => return false,
    };
    with_ext4(|fs| {
        let mut parent_inode_num = 2u32;
        let mut nameoff = 0u32;
        let child_inode = match fs.generic_open(
            normalized.as_str(),
            &mut parent_inode_num,
            false,
            0,
            &mut nameoff,
        ) {
            Ok(inode) => inode,
            Err(_) => return false,
        };

        let mut child_inode_ref = fs.get_inode_ref(child_inode);
        if child_inode_ref.inode.is_dir() {
            return false;
        }

        let child_link_cnt = child_inode_ref.inode.links_count();
        let child_size = child_inode_ref.inode.size();
        if child_link_cnt == 1 && child_size > 0 {
            if fs.truncate_inode(&mut child_inode_ref, 0).is_err() {
                return false;
            }
        }

        let name = match normalized.rsplit('/').next() {
            Some(name) if !name.is_empty() => name,
            _ => return false,
        };
        let mut parent_inode_ref = fs.get_inode_ref(parent_inode_num);
        fs.unlink(&mut parent_inode_ref, &mut child_inode_ref, name)
            .is_ok()
    })
}

pub fn open_file_at(base_dir: Option<&str>, path: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    let want_dir = flags.directory_requested();

    if flags.create_requested() {
        let normalized = normalize_path(base_dir, path)?;
        if want_dir {
            return None;
        }
        if let Some((inode_num, full_path, is_dir)) = lookup_existing(base_dir, path, false) {
            if flags.trunc_requested() && !is_dir {
                let truncated = with_ext4(|fs| {
                    let mut inode_ref = fs.get_inode_ref(inode_num);
                    fs.truncate_inode(&mut inode_ref, 0).is_ok()
                });
                if !truncated {
                    return None;
                }
            }
            let (readable, writable) = if is_dir { (false, false) } else { flags.read_write() };
            return Some(Arc::new(OSInode::new(readable, writable, inode_num, full_path, is_dir)));
        }

        let inode_num = with_ext4(|fs| {
            let mut parent = 2u32;
            let mut off = 0u32;
            fs.generic_open(
                normalized.as_str(),
                &mut parent,
                true,
                InodeFileType::S_IFREG.bits(),
                &mut off,
            )
            .ok()
        })?;
        let (readable, writable) = flags.read_write();
        return Some(Arc::new(OSInode::new(
            readable,
            writable,
            inode_num,
            normalized,
            false,
        )));
    }

    let (inode_num, full_path, is_dir) = lookup_existing(base_dir, path, want_dir)?;

    if flags.trunc_requested() && !is_dir {
        let truncated = with_ext4(|fs| {
            let mut inode_ref = fs.get_inode_ref(inode_num);
            fs.truncate_inode(&mut inode_ref, 0).is_ok()
        });
        if !truncated {
            return None;
        }
    }

    let (readable, writable) = if is_dir { (false, false) } else { flags.read_write() };
    Some(Arc::new(OSInode::new(
        readable,
        writable,
        inode_num,
        full_path,
        is_dir,
    )))
}

impl File for OSInode {
    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read(&self, buf: &mut [u8]) -> usize {
        let mut inner = self.inner.exclusive_access();
        let read_size = with_ext4(|fs| fs.read_at(inner.inode_num, inner.offset, buf).ok())
            .unwrap_or(0);
        inner.offset += read_size;
        read_size
    }

    fn write(&self, buf: &mut [u8]) -> usize {
        let mut inner = self.inner.exclusive_access();
        let write_size = with_ext4(|fs| fs.write_at(inner.inode_num, inner.offset, buf).ok())
            .unwrap_or(0);
        inner.offset += write_size;
        write_size
    }

    fn stat(&self) -> Kstat {
        self.stat_inner()
    }

    fn is_dir(&self) -> bool {
        self.inner.exclusive_access().is_dir
    }

    fn path(&self) -> Option<String> {
        Some(self.inner.exclusive_access().path.clone())
    }

    fn getdents64(&self, buf: &mut [u8]) -> isize {
        const DIRENT64_FIXED_SIZE: usize = 8 + 8 + 2 + 1;
        let mut inner = self.inner.exclusive_access();
        let inode_is_dir = with_ext4(|fs| fs.get_inode_ref(inner.inode_num).inode.is_dir());
        if !inode_is_dir {
            return -1;
        }
        let entries = with_ext4(|fs| fs.ext4_dir_get_entries(inner.inode_num));

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
}
