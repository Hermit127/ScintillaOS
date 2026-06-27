pub mod block_device_stream;
pub mod inode;
mod pipe;
pub mod proc;
pub mod stdio;
pub mod vfs;

use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

static BLOCK_WRITTEN_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn record_block_write_bytes(bytes: usize) {
    if bytes > 0 {
        BLOCK_WRITTEN_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

pub fn block_written_sectors() -> u64 {
    let bytes = BLOCK_WRITTEN_BYTES.load(Ordering::Relaxed);
    ((bytes + 511) / 512).max(1)
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Kstat {
    pub st_dev: u64,        // Device ID containing the file
    pub st_ino: u64,        // Inode number
    pub st_mode: u32,       // File type and mode bits
    pub st_nlink: u32,      // Number of hard links
    pub st_uid: u32,        // Owner user ID
    pub st_gid: u32,        // Owner group ID
    pub st_rdev: u64,       // Device ID for special files
    pub __pad: u64,         // ABI padding for layout compatibility
    pub st_size: i64,       // Total file size in bytes
    pub st_blksize: u32,    // Preferred I/O block size
    pub __pad2: i32,        // ABI padding for layout compatibility
    pub st_blocks: u64,     // Number of 512-byte blocks allocated
    pub st_atime_sec: i64,  // Last access time: seconds
    pub st_atime_nsec: i64, // Last access time: nanoseconds
    pub st_mtime_sec: i64,  // Last modification time: seconds
    pub st_mtime_nsec: i64, // Last modification time: nanoseconds
    pub st_ctime_sec: i64,  // Last status change time: seconds
    pub st_ctime_nsec: i64, // Last status change time: nanoseconds
    pub __unused: [u32; 2], // Reserved for future extension
}

impl Kstat {
    pub const fn new(st_mode: u32) -> Self {
        Self {
            st_dev: 0,
            st_ino: 0,
            st_mode,
            st_nlink: 1,
            st_uid: 0,
            st_gid: 0,
            st_rdev: 0,
            __pad: 0,
            st_size: 0,
            st_blksize: 512,
            __pad2: 0,
            st_blocks: 0,
            st_atime_sec: 0,
            st_atime_nsec: 0,
            st_mtime_sec: 0,
            st_mtime_nsec: 0,
            st_ctime_sec: 0,
            st_ctime_nsec: 0,
            __unused: [0; 2],
        }
    }
}

pub const S_IFIFO: u32 = 0o010000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFSOCK: u32 = 0o140000;

pub trait File: Send + Sync {
    fn readable(&self) -> bool;
    fn writable(&self) -> bool;
    fn read(&self, buf: &mut [u8]) -> isize;
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        self.seek(offset);
        self.read(buf)
    }
    fn write(&self, buf: &mut [u8]) -> isize;
    fn write_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        self.seek(offset);
        self.write(buf)
    }
    fn stat(&self) -> Kstat;
    fn is_dir(&self) -> bool {
        false
    }
    fn path(&self) -> Option<String> {
        None
    }
    fn getdents64(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    fn seek(&self, _offset: usize) {}
    fn get_offset(&self) -> usize {
        0
    }
    fn get_flags(&self) -> usize {
        let mut flags = 0usize;
        if self.readable() && self.writable() {
            flags |= 2;
        } else if self.writable() {
            flags |= 1;
        }
        0x8000 | flags
    }
    fn set_flags(&self, _flags: usize) -> bool {
        false
    }
    fn poll(&self) -> (bool, bool, bool) {
        (true, true, false)
    }
    fn dup_ref(&self) {}
    fn close_ref(&self) {}
    fn as_any(&self) -> &dyn core::any::Any;
}

pub use inode::{
    follow_symlink, link_at, mkdir_at, open_file, open_file_at, rename_at, symlink_at, unlink_at,
    OpenFlags,
};
pub use pipe::make_pipe;
pub use stdio::{DevBlock, DevNull, DevZero, NullSocket, Stdin, Stdout, Urandom};
pub use vfs::{list_apps, mount_root};
