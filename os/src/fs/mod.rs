mod inode;
mod pipe;
mod stdio;
use alloc::string::String;

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
pub const S_IFREG: u32 = 0o100000;

pub trait File: Send + Sync {
    fn readable(&self) -> bool;
    fn writable(&self) -> bool;
    fn read(&self, buf: &mut [u8]) -> usize;
    fn write(&self, buf: &mut [u8]) -> usize;
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
}

pub use inode::{list_apps, open_file, open_file_at, OpenFlags};
pub use pipe::make_pipe;
pub use stdio::{Stdin, Stdout};
