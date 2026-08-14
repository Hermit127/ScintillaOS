// 这里负责操作系统的文件管理：定义文件系统模块的公共类型、File trait 和子模块导出。
pub mod block_device_stream;
mod epoll;
mod eventfd;
pub mod inode;
mod pipe;
pub mod proc;
pub mod stdio;
mod unix_socket;
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
    // 构造一份最小可用的 stat，调用者再按具体文件类型补字段。
    // 执行时把传入的 st_mode 写入结构体，其余字段按 Linux stat 的安全默认值初始化。
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
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFSOCK: u32 = 0o140000;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FilePageCacheKey {
    pub super_block: usize,
    pub inode: u32,
}

pub trait File: Send + Sync {
    // 这个 fd 是否支持读取。
    // 执行时由具体文件对象返回自己的读权限，不要求访问底层文件内容。
    fn readable(&self) -> bool;
    // 这个 fd 是否支持写入。
    // 执行时由具体文件对象返回自己的写权限，不修改任何文件状态。
    fn writable(&self) -> bool;
    // 从当前对象读取数据，具体 offset 语义由实现决定。
    // 执行时由具体实现决定从哪里取数据，并按约定返回实际读取字节数或错误码。
    fn read(&self, buf: &mut [u8]) -> isize;
    // 默认的定点读取：先 seek，再走普通 read。
    // 执行时先调用 seek 把对象位置移到 offset，再调用 read 完成读取。
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        self.seek(offset);
        self.read(buf)
    }
    // 向当前对象写入数据，具体 offset 语义由实现决定。
    // 执行时由具体实现决定把数据写到哪里，并按约定返回实际写入字节数或错误码。
    fn write(&self, buf: &mut [u8]) -> isize;
    // 默认的定点写入：先 seek，再走普通 write。
    // 执行时先调用 seek 把对象位置移到 offset，再调用 write 完成写入。
    fn write_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        self.seek(offset);
        self.write(buf)
    }
    // 返回 Linux stat 兼容的信息。
    // 执行时由具体实现把对象状态转换成 Kstat，供 fstat/stat 系统调用写回用户态。
    fn stat(&self) -> Kstat;
    // 默认不是目录，目录类文件自己覆盖。
    // 执行时默认直接返回 false，目录文件需要覆盖这个方法。
    fn is_dir(&self) -> bool {
        false
    }
    // 默认没有稳定路径，普通 inode 会返回 VFS 路径。
    // 执行时默认返回 None，只有能对应到 VFS 路径的文件对象才需要覆盖。
    fn path(&self) -> Option<String> {
        None
    }
    // Return stable inode identity for clean file-backed mmap page sharing.
    fn page_cache_key(&self) -> Option<FilePageCacheKey> {
        None
    }
    // 默认不支持读目录项。
    // 执行时默认返回 -1 表示不支持 getdents64，目录对象需要自己实现。
    fn getdents64(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    // 默认 seek 什么也不做，只有有 offset 的文件需要实现。
    // 执行时默认忽略 offset，不维护位置的设备文件可以直接沿用这个实现。
    fn seek(&self, _offset: usize) {}
    // 默认 offset 为 0，给不关心偏移的设备文件使用。
    // 执行时默认返回 0，表示这个文件对象没有独立维护当前位置。
    fn get_offset(&self) -> usize {
        0
    }
    // 根据读写能力拼出用户态能看到的基本 open flags。
    // 执行时根据当前打开状态重新拼出用户态可见的 flag 位，最后返回给 fcntl 等调用。
    fn get_flags(&self) -> usize {
        let mut flags = 0usize;
        if self.readable() && self.writable() {
            flags |= 2;
        } else if self.writable() {
            flags |= 1;
        }
        0x8000 | flags
    }
    // 默认不允许修改打开标志。
    // 执行时默认忽略传入 flags 并返回 false，表示这个对象不支持动态修改标志。
    fn set_flags(&self, _flags: usize) -> bool {
        false
    }
    // 默认认为普通文件总是可读可写且无异常。
    // 执行时直接返回可读、可写、无挂断，特殊文件或 socket 可以覆盖成真实状态。
    fn poll(&self) -> (bool, bool, bool) {
        (true, true, false)
    }
    // Stateful files can increment this when poll-visible readiness changes,
    // allowing edge-triggered epoll to observe transitions between samples.
    // Permanently ready files can keep the default generation.
    fn readiness_generation(&self) -> u64 {
        0
    }
    // fd 复制时的引用钩子，普通文件不需要额外动作。
    // 执行时默认什么都不做，只有管道或 socket 这类需要计数的对象才覆盖。
    fn dup_ref(&self) {}
    // fd 关闭时的引用钩子，普通文件不需要额外动作。
    // 执行时默认什么都不做，具体对象可以在这里减少引用或唤醒等待者。
    fn close_ref(&self) {}
    // 给上层做 downcast 用。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any;
}

pub use inode::{
    follow_symlink, link_at, mkdir_at, open_file, open_file_at, rename_at, symlink_at, unlink_at,
    unlink_at_result, OpenFlags,
};
pub use epoll::{EpollEvent, EpollFile, EPOLL_MAX_INTERESTS};
pub use eventfd::EventFdFile;
pub use pipe::make_pipe;
pub use stdio::{DevBlock, DevNull, DevTty, DevZero, NullSocket, Stdin, Stdout, TmpFile, Urandom};
pub use unix_socket::{make_unix_socket_pair, UnixSocketFile, UnixSocketKind};
pub use vfs::{list_apps, mount_root};
