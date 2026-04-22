use super::{File, Kstat, S_IFREG};
use crate::drivers::BLOCK_DEVICE;
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;
use easyfs::{EasyFileSystem, Inode};
use lazy_static::*;

pub struct OSInode {
    readable: bool,
    writable: bool,
    inner: UPSafeCell<OSInodeInner>,
}

pub struct OSInodeInner {
    offset: usize,
    inode: Arc<Inode>,
}

impl OSInode {
    pub fn new(readable: bool, writable: bool, inode: Arc<Inode>) -> Self {
        Self {
            readable,
            writable,
            inner: unsafe { UPSafeCell::new(OSInodeInner { offset: 0, inode }) },
        }
    }
    pub fn read_all(&self) -> Vec<u8> {
        let mut inner = self.inner.exclusive_access();
        let mut buffer = [0u8; 512];
        let mut v: Vec<u8> = Vec::new();
        loop {
            let len = inner.inode.read_at(inner.offset, &mut buffer);
            if len == 0 {
                break;
            }
            inner.offset += len;
            v.extend_from_slice(&buffer[..len]);
        }
        v
    }

    fn stat_inner(&self) -> Kstat {
        let inner = self.inner.exclusive_access();
        let mut st = Kstat::new(S_IFREG);
        st.st_ino = Arc::as_ptr(&inner.inode) as usize as u64;
        let mut offset = 0usize;
        let mut block = [0u8; 512];
        loop {
            let len = inner.inode.read_at(offset, &mut block);
            if len == 0 {
                break;
            }
            offset += len;
        }
        st.st_size = offset as i64;
        st.st_blocks = (offset as u64 + 511) / 512;
        st
    }

    fn align_up(value: usize, align: usize) -> usize {
        (value + align - 1) & !(align - 1)
    }
}

lazy_static! {
    pub static ref ROOT_INODE: Arc<Inode> = {
        let efs = EasyFileSystem::open(BLOCK_DEVICE.clone());
        Arc::new(EasyFileSystem::root_inode(&efs))
    };
}

pub fn list_apps() {
    println!("/**** APPS ****");
    for app in ROOT_INODE.ls() {
        println!("{}", app);
    }
    println!("**************/");
}

bitflags! {
    pub struct OpenFlags: u32 {
        const RDONLY = 0;
        const WRONLY = 1 << 0;
        const RDWR = 1 << 1;
        const CREATE = 1 << 9;
        const TRUNC = 1 << 10;
    }
}

impl OpenFlags {
    /// Do not check validity for simplicity
    /// Return (readable, writable)
    pub fn read_write(&self) -> (bool, bool) {
        if self.is_empty() {
            (true, false)
        } else if self.contains(Self::WRONLY) {
            (false, true)
        } else {
            (true, true)
        }
    }
}

pub fn open_file(name: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    let (readable, writable) = flags.read_write();
    if name == "." || name == "/" {
        return Some(Arc::new(OSInode::new(readable, writable, ROOT_INODE.clone())));
    }
    if flags.contains(OpenFlags::CREATE) {
        if let Some(inode) = ROOT_INODE.find(name) {
            // clear size
            inode.clear();
            Some(Arc::new(OSInode::new(readable, writable, inode)))
        } else {
            // create file
            ROOT_INODE
                .create(name)
                .map(|inode| Arc::new(OSInode::new(readable, writable, inode)))
        }
    } else {
        ROOT_INODE.find(name).map(|inode| {
            if flags.contains(OpenFlags::TRUNC) {
                inode.clear();
            }
            Arc::new(OSInode::new(readable, writable, inode))
        })
    }
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
        let rsize = inner.inode.read_at(inner.offset, buf);
        inner.offset += rsize;
        rsize
    }
    fn write(&self, buf: &mut [u8]) -> usize {
        let mut inner = self.inner.exclusive_access();
        let wsize = inner.inode.write_at(inner.offset, buf);
        inner.offset += wsize;
        wsize
    }
    fn stat(&self) -> Kstat {
        self.stat_inner()
    }

    fn getdents64(&self, buf: &mut [u8]) -> isize {
        const DIRENT64_FIXED_SIZE: usize = 8 + 8 + 2 + 1;
        let mut inner = self.inner.exclusive_access();
        let entries = inner.inode.ls();
        if inner.offset >= entries.len() {
            return 0;
        }

        let mut written = 0usize;
        let mut entry_index = inner.offset;
        while entry_index < entries.len() {
            let name = entries[entry_index].as_bytes();
            let record_size = Self::align_up(DIRENT64_FIXED_SIZE + name.len() + 1, 8);
            if written + record_size > buf.len() {
                if written == 0 {
                    return -1;
                }
                break;
            }

            let record = &mut buf[written..written + record_size];
            record.fill(0);

            let d_ino = (entry_index as u64) + 1;
            record[0..8].copy_from_slice(&d_ino.to_le_bytes());

            let d_off = (entry_index as i64) + 1;
            record[8..16].copy_from_slice(&d_off.to_le_bytes());

            let d_reclen = record_size as u16;
            record[16..18].copy_from_slice(&d_reclen.to_le_bytes());

            // easyfs currently does not expose a stable per-entry type here.
            record[18] = 0;

            let name_start = DIRENT64_FIXED_SIZE;
            record[name_start..name_start + name.len()].copy_from_slice(name);
            record[name_start + name.len()] = 0;

            written += record_size;
            entry_index += 1;
        }

        inner.offset = entry_index;
        written as isize
    }
}
