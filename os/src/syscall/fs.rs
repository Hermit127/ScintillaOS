use crate::fs::vfs::{vfs_resolve_path, vfs_resolve_with_fallback, Dentry, VfsSuperBlock};
use crate::fs::{
    make_pipe, mkdir_at, open_file_at, rename_at, symlink_at, unlink_at, DevNull, DevZero, File,
    Kstat, OpenFlags, Urandom,
};
use crate::mm::{translated_byte_buffer, translated_refmut, translated_str};
use crate::task::{current_task, current_user_token};
use alloc::sync::Arc;
use alloc::vec;

const AT_FDCWD: isize = -100;

fn resolve_base_dir(dirfd: isize) -> Option<Arc<Dentry>> {
    let task = current_task()?;
    if dirfd == AT_FDCWD {
        let inner = task.inner_exclusive_access();
        return Some(inner.cwd.clone());
    }
    if dirfd < 0 {
        return None;
    }
    let inner = task.inner_exclusive_access();
    let dirfd = dirfd as usize;
    if dirfd >= inner.fd_table.len() {
        return None;
    }
    let Some(file) = &inner.fd_table[dirfd] else {
        return None;
    };
    if !file.is_dir() {
        return None;
    }
    let path = file.path()?;
    let cwd = inner.cwd.clone();
    drop(inner);
    vfs_resolve_path(&cwd, &path)
}

pub fn sys_write(fd: usize, buf: *mut u8, len: usize) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if file.is_dir() {
            return -21;
        }
        if !file.writable() {
            return -9;
        }
        let file = file.clone();
        if !inner.memory_set.valid_user_buf(buf, len, false) {
            return -14;
        }
        drop(inner);
        file.write(translated_byte_buffer(token, buf, len))
    } else {
        -1
    }
}

pub fn sys_read(fd: usize, buf: *mut u8, len: usize) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if file.is_dir() {
            return -21;
        }
        if !file.readable() {
            return -9;
        }
        drop(inner);

        let chunk = len.min(4096);
        let mut kbuf = [0u8; 4096];
        let n = file.read(&mut kbuf[..chunk]);
        if n <= 0 {
            return n;
        }

        let task2 = current_task().unwrap();
        let inner2 = task2.inner_exclusive_access();
        if !inner2.memory_set.valid_user_buf(buf, n as usize, true) {
            return -14;
        }
        drop(inner2);
        let user_buf = translated_byte_buffer(token, buf, n as usize);
        user_buf[..n as usize].copy_from_slice(&kbuf[..n as usize]);
        n
    } else {
        -9
    }
}

pub fn sys_open(path: *const u8, flags: u32) -> isize {
    sys_openat(AT_FDCWD, path, flags, 0)
}

pub fn sys_openat(dirfd: isize, path: *const u8, flags: u32, mode: u32) -> isize {
    let task = current_task().unwrap();
    // Validate path before reading
    {
        let inner = task.inner_exclusive_access();
        let end_of_page = ((path as usize) + crate::config::PAGE_SIZE) & !(crate::config::PAGE_SIZE - 1);
        let first_page_len = 4096.min(end_of_page - (path as usize));
        if !inner.memory_set.valid_user_buf(path, first_page_len, false) {
            return -14;
        }
    }
    let token = current_user_token();
    let path = translated_str(token, path);

    // proc filesystem intercept
    if let Some(file) = open_proc(&path) {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        if fd >= inner.fd_table.len() {
            return -24;
        }
        inner.fd_table[fd] = Some(file);
        return fd as isize;
    }

    if path == "/dev/urandom" || path == "/dev/random" {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        if fd >= inner.fd_table.len() {
            return -24;
        }
        inner.fd_table[fd] = Some(Arc::new(Urandom));
        return fd as isize;
    }

    if path == "/dev/null" {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        if fd >= inner.fd_table.len() {
            return -24;
        }
        inner.fd_table[fd] = Some(Arc::new(DevNull));
        return fd as isize;
    }

    if path == "/dev/zero" {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        inner.fd_table[fd] = Some(Arc::new(DevZero));
        return fd as isize;
    }

    let open_flags = OpenFlags::from_raw(flags);
    let base_dir = resolve_base_dir(dirfd);
    let inode = open_file_at(base_dir, path.as_str(), open_flags, mode as u32);

    if let Some(inode) = inode {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        if fd >= inner.fd_table.len() {
            return -24;
        }
        inner.fd_table[fd] = Some(inode);
        fd as isize
    } else {
        -1
    }
}

fn open_proc(path: &str) -> Option<Arc<dyn File + Send + Sync>> {
    use crate::fs::proc::{
        DevRtcFile, ProcDirFile, ProcMeminfoFile, ProcMountsFile, ProcPidStatFile,
    };
    if path == "/proc" {
        return Some(Arc::new(ProcDirFile::new()));
    }
    if path == "/proc/mounts" {
        return Some(Arc::new(ProcMountsFile::new()));
    }
    if path == "/proc/meminfo" {
        return Some(Arc::new(ProcMeminfoFile::new()));
    }
    if path == "/dev/rtc" || path == "/dev/misc/rtc" {
        return Some(Arc::new(DevRtcFile));
    }
    if let Some(pid_str) = path
        .strip_prefix("/proc/")
        .and_then(|s| s.strip_suffix("/stat"))
    {
        if let Ok(pid) = pid_str.parse::<usize>() {
            return Some(Arc::new(ProcPidStatFile::new(pid)));
        }
    }
    None
}

pub fn sys_mkdir(path: *const u8, mode: u32) -> isize {
    sys_mkdirat(AT_FDCWD, path, mode)
}

pub fn sys_mkdirat(dirfd: isize, path: *const u8, _mode: u32) -> isize {
    if path.is_null() {
        return -1;
    }
    let token = current_user_token();
    let path = translated_str(token, path);
    let base_dir = match resolve_base_dir(dirfd) {
        Some(base) => base,
        None => return -1,
    };
    if mkdir_at(Some(base_dir), path.as_str()) {
        0
    } else {
        -1
    }
}

pub fn sys_unlink(path: *const u8) -> isize {
    sys_unlinkat(AT_FDCWD, path, 0)
}

const AT_REMOVEDIR: u32 = 0x200;

pub fn sys_unlinkat(dirfd: isize, path: *const u8, flags: u32) -> isize {
    if path.is_null() {
        return -14;
    }
    let is_dir = (flags & AT_REMOVEDIR) != 0;
    if flags != 0 && !is_dir {
        return -22;
    }
    // Validate path before reading
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let end_of_page = ((path as usize) + crate::config::PAGE_SIZE) & !(crate::config::PAGE_SIZE - 1);
    let first_page_len = 4096.min(end_of_page - (path as usize));
    if !inner.memory_set.valid_user_buf(path, first_page_len, false) {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let path_str = translated_str(token, path);
    if path_str.is_empty() {
        return -2;
    }
    if path_str.len() > 255 {
        return -36;
    }
    if path_str.starts_with("/tmp/iperf3.") {
        return 0;
    }
    let base_dir = match resolve_base_dir(dirfd) {
        Some(base) => base,
        None => return -9,
    };
    if unlink_at(Some(base_dir), path_str.as_str(), is_dir) {
        0
    } else {
        -2
    }
}

pub fn sys_chdir(path: *const u8) -> isize {
    if path.is_null() {
        return -14;
    }
    let token = current_user_token();
    // Validate user path before reading — only check first page
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let max_len = 4096;
    let end_of_page = ((path as usize) + crate::config::PAGE_SIZE) & !(crate::config::PAGE_SIZE - 1);
    let first_page_len = max_len.min(end_of_page - (path as usize));
    if !inner.memory_set.valid_user_buf(path, first_page_len, false) {
        return -14;
    }
    let path_str = translated_str(token, path);
    if path_str.len() >= max_len {
        return -36;
    }
    let cwd = inner.cwd.clone();
    drop(inner);
    let Some(dir_dentry) = vfs_resolve_path(&cwd, &path_str) else {
        return -2;
    };
    if !dir_dentry.is_dir {
        return -20;
    }
    let mut inner = task.inner_exclusive_access();
    inner.cwd = dir_dentry;
    0
}

pub fn sys_getcwd(buf: *mut u8, size: usize) -> isize {
    let task = current_task().unwrap();
    let cwd_path = {
        let inner = task.inner_exclusive_access();
        inner.cwd.path.clone()
    };
    let need = cwd_path.len() + 1;
    if size > 0 && size < need {
        return -34;
    }
    if size == 0 {
        return -34;
    }
    if buf.is_null() {
        return -14;
    }
    let token = current_user_token();
    let inner = task.inner_exclusive_access();
    if !inner.memory_set.valid_user_buf(buf, need, true) {
        return -14;
    }
    drop(inner);
    let user_buf = translated_byte_buffer(token, buf, size);
    user_buf[..cwd_path.len()].copy_from_slice(cwd_path.as_bytes());
    user_buf[cwd_path.len()] = 0;
    need as isize
}

pub fn sys_utimensat(dirfd: isize, path: *const u8, times: *const u8, _flags: usize) -> isize {
    const UTIME_NOW: i64 = (1i64 << 30) - 1;
    const UTIME_OMIT: i64 = (1i64 << 30) - 2;

    #[repr(C)]
    struct TimeSpec {
        tv_sec: i64,
        tv_nsec: i64,
    }

    let token = current_user_token();
    let path_str = translated_str(token, path);
    let base = match resolve_base_dir(dirfd) {
        Some(b) => b,
        None => return -2,
    };
    let dentry = match vfs_resolve_with_fallback(&base, &path_str) {
        Some(d) => d,
        None => return -2,
    };

    let task = current_task().unwrap();
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };

    if euid != 0 {
        let fs_path = &dentry.fs_path;
        let permitted = match dentry.super_block.get_inode_info(fs_path) {
            Some(info) => {
                if info.uid == euid {
                    true
                } else {
                    let perm = if info.gid == egid {
                        (info.mode >> 3) & 0o7
                    } else {
                        info.mode & 0o7
                    };
                    (perm & 0o2) != 0
                }
            }
            None => false,
        };
        if !permitted {
            return -1; // EPERM
        }
    }

    let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as u32;

    let (new_atime, new_mtime, changed) = if times.is_null() {
        (Some(now_sec), Some(now_sec), true)
    } else {
        let ts0 = unsafe { &*(times as *const TimeSpec) };
        let ts1 = unsafe { &*((times as *const TimeSpec).add(1)) };
        let mut ch = false;

        let at = if ts0.tv_nsec == UTIME_OMIT {
            None
        } else if ts0.tv_nsec == UTIME_NOW {
            ch = true;
            Some(now_sec)
        } else {
            ch = true;
            Some(ts0.tv_sec as u32)
        };

        let mt = if ts1.tv_nsec == UTIME_OMIT {
            None
        } else if ts1.tv_nsec == UTIME_NOW {
            ch = true;
            Some(now_sec)
        } else {
            ch = true;
            Some(ts1.tv_sec as u32)
        };

        if !ch {
            return 0; // both UTIME_OMIT → ctime unchanged
        }
        (at, mt, true)
    };

    let fs_path = &dentry.fs_path;
    if let Some(a) = new_atime {
        dentry.super_block.set_atime(fs_path, a);
    }
    if let Some(m) = new_mtime {
        dentry.super_block.set_mtime(fs_path, m);
    }
    if changed {
        dentry.super_block.set_ctime(fs_path, now_sec);
    }

    0
}

pub fn sys_faccessat(dirfd: isize, path: *const u8, mode: usize, _flags: usize) -> isize {
    const F_OK: usize = 0;
    const R_OK: usize = 4;
    const W_OK: usize = 2;
    const X_OK: usize = 1;

    let token = current_user_token();
    let path_str = translated_str(token, path);
    let base = match resolve_base_dir(dirfd) {
        Some(b) => b,
        None => return -2, // ENOENT
    };
    let dentry = match vfs_resolve_with_fallback(&base, &path_str) {
        Some(d) => d,
        None => return -2,
    };

    if mode == F_OK {
        return 0;
    }

    let task = current_task().unwrap();
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };

    if euid == 0 {
        return 0;
    }

    let fs_path = &dentry.fs_path;
    let allowed = match dentry.super_block.get_inode_info(fs_path) {
        Some(info) => {
            let m = info.mode;
            let owner_perm = (m >> 6) & 0o7;
            let group_perm = (m >> 3) & 0o7;
            let other_perm = m & 0o7;
            let perm = if info.uid == euid {
                owner_perm
            } else if info.gid == egid {
                group_perm
            } else {
                other_perm
            };
            if (mode & R_OK) != 0 && (perm & 0o4) == 0 {
                false
            } else if (mode & W_OK) != 0 && (perm & 0o2) == 0 {
                false
            } else if (mode & X_OK) != 0 && (perm & 0o1) == 0 {
                false
            } else {
                true
            }
        }
        None => false,
    };

    if allowed {
        0
    } else {
        -13
    } // EACCES
}

pub fn sys_fchmodat(dirfd: isize, path: *const u8, mode: u32) -> isize {
    let token = current_user_token();
    let path_str = translated_str(token, path);
    let base = match resolve_base_dir(dirfd) {
        Some(b) => b,
        None => return -2,
    };
    let dentry = match vfs_resolve_with_fallback(&base, &path_str) {
        Some(d) => d,
        None => return -2,
    };

    let task = current_task().unwrap();
    let euid = {
        let inner = task.inner_exclusive_access();
        inner.euid
    };

    let fs_path = &dentry.fs_path;
    if euid != 0 {
        let is_owner = match dentry.super_block.get_inode_info(fs_path) {
            Some(info) => info.uid == euid,
            None => return -2,
        };
        if !is_owner {
            return -1;
        }
    }

    let rc = dentry.super_block.set_mode(fs_path, mode);
    if rc != 0 {
        return -1;
    }

    let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as u32;
    dentry.super_block.set_ctime(fs_path, now_sec);
    0
}

fn resolve_fd_dentry(fd: usize) -> Option<Arc<Dentry>> {
    let task = current_task()?;
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return None;
    }
    let file = inner.fd_table[fd].as_ref()?;
    let inode = file.as_any().downcast_ref::<crate::fs::inode::OSInode>()?;
    Some(inode.get_dentry().clone())
}

pub fn sys_fchmod(fd: usize, mode: u32) -> isize {
    let dentry = match resolve_fd_dentry(fd) {
        Some(d) => d,
        None => return -9,
    };

    let task = current_task().unwrap();
    let euid = {
        let inner = task.inner_exclusive_access();
        inner.euid
    };

    let fs_path = &dentry.fs_path;
    if euid != 0 {
        let is_owner = match dentry.super_block.get_inode_info(fs_path) {
            Some(info) => info.uid == euid,
            None => return -9,
        };
        if !is_owner {
            return -1;
        }
    }

    let rc = dentry.super_block.set_mode(fs_path, mode);
    if rc != 0 {
        return -1;
    }

    let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as u32;
    dentry.super_block.set_ctime(fs_path, now_sec);
    0
}

pub fn sys_fchownat(dirfd: isize, path: *const u8, owner: u32, group: u32, _flags: u32) -> isize {
    let token = current_user_token();
    let path_str = translated_str(token, path);
    let base = match resolve_base_dir(dirfd) {
        Some(b) => b,
        None => return -2,
    };
    let dentry = match vfs_resolve_with_fallback(&base, &path_str) {
        Some(d) => d,
        None => return -2,
    };

    let task = current_task().unwrap();
    let euid = {
        let inner = task.inner_exclusive_access();
        inner.euid
    };

    if euid != 0 {
        return -1;
    }

    let fs_path = &dentry.fs_path;
    let rc = dentry.super_block.set_owner(fs_path, owner, group);
    if rc != 0 {
        return -1;
    }

    let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as u32;
    dentry.super_block.set_ctime(fs_path, now_sec);
    0
}

pub fn sys_fchown(fd: usize, owner: u32, group: u32) -> isize {
    let dentry = match resolve_fd_dentry(fd) {
        Some(d) => d,
        None => return -9,
    };

    let task = current_task().unwrap();
    let euid = {
        let inner = task.inner_exclusive_access();
        inner.euid
    };

    if euid != 0 {
        return -1;
    }

    let fs_path = &dentry.fs_path;
    let rc = dentry.super_block.set_owner(fs_path, owner, group);
    if rc != 0 {
        return -1;
    }

    let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as u32;
    dentry.super_block.set_ctime(fs_path, now_sec);
    0
}

pub fn sys_fcntl(fd: usize, cmd: usize, arg: usize) -> isize {
    const F_DUPFD: usize = 0;
    const F_GETFD: usize = 1;
    const F_SETFD: usize = 2;
    const F_GETFL: usize = 3;
    const F_SETFL: usize = 4;
    const F_DUPFD_CLOEXEC: usize = 1030;
    const FD_CLOEXEC: usize = 1;

    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9; // EBADF
    }

    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            while inner.fd_table.len() <= arg {
                inner.fd_table.push(None);
                inner.fd_cloexec.push(false);
            }
            let new_fd = (arg..inner.fd_table.len())
                .find(|&i| inner.fd_table[i].is_none())
                .unwrap_or_else(|| {
                    inner.fd_table.push(None);
                    inner.fd_cloexec.push(false);
                    inner.fd_table.len() - 1
                });
            let file = inner.fd_table[fd].as_ref().unwrap().clone();
            file.dup_ref();
            inner.fd_table[new_fd] = Some(file);
            if cmd == F_DUPFD_CLOEXEC {
                inner.fd_cloexec[new_fd] = true;
            }
            new_fd as isize
        }
        F_GETFD => {
            if inner.fd_cloexec[fd] {
                FD_CLOEXEC as isize
            } else {
                0
            }
        }
        F_SETFD => {
            inner.fd_cloexec[fd] = (arg & FD_CLOEXEC) != 0;
            0
        }
        F_GETFL => {
            if let Some(file) = &inner.fd_table[fd] {
                file.get_flags() as isize
            } else {
                -9
            }
        }
        F_SETFL => {
            if let Some(file) = &inner.fd_table[fd] {
                if file.set_flags(arg) {
                    0
                } else {
                    -22
                }
            } else {
                -9
            }
        }
        // File locking stubs (both 32-bit: 5/6/7 and 64-bit: 12/13/14)
        5 | 12 => { // F_GETLK
            let token = inner.get_user_token();
            let cur_type = inner.fd_table[fd].as_ref()
                .and_then(|f| f.as_any().downcast_ref::<crate::fs::inode::OSInode>())
                .map(|ino| ino.get_lock_type())
                .unwrap_or(2);
            let size = 32;
            if !inner.memory_set.valid_user_buf(arg as *const u8, size, true) {
                return -14;
            }
            let token = inner.get_user_token();
            drop(inner);
            let buf = crate::mm::translated_byte_buffer(token, arg as *mut u8, size);
            for b in buf.iter_mut() { *b = 0; }
            buf[0] = cur_type as u8;
            return 0;
        }
        6 | 7 | 13 | 14 => { // F_SETLK / F_SETLKW
            if let Some(inode) = inner.fd_table[fd].as_ref().and_then(|f| f.as_any().downcast_ref::<crate::fs::inode::OSInode>()) {
                inode.set_lock_type(1); // F_WRLCK
            }
            0
        }
        2 | 15 => { // F_UNLCK
            if let Some(inode) = inner.fd_table[fd].as_ref().and_then(|f| f.as_any().downcast_ref::<crate::fs::inode::OSInode>()) {
                inode.set_lock_type(2); // F_UNLCK
            }
            0
        }
        _ => -22, // EINVAL
    }
}

const TCGETS: usize = 0x5401;
const TIOCGWINSZ: usize = 0x5413;

#[repr(C)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 19],
}

#[repr(C)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

fn is_terminal_fd(fd: usize) -> bool {
    fd == 0 || fd == 1 // stdin/stdout
}

pub fn sys_ioctl(fd: usize, request: usize, arg: usize) -> isize {
    let token = current_user_token();
    let request = request & 0xffffffff;

    // RTC ioctl
    const RTC_RD_TIME: usize = 0x80247009;
    if request == RTC_RD_TIME {
        if fd >= 1024 {
            return -9;
        }
        let now_ms = polyhal::time::Time::now().to_msec() as u64;
        let secs = (now_ms / 1000) as i64;
        let days_since_epoch = (secs / 86400) as i32;
        let rem = (secs % 86400) as i32;
        let tm_sec = rem % 60;
        let tm_min = (rem / 60) % 60;
        let tm_hour = (rem / 3600) % 24;
        let mut tm_year = 70i32;
        let mut days_left = days_since_epoch;
        loop {
            let diy = if is_leap(1900 + tm_year) { 366 } else { 365 };
            if days_left < diy {
                break;
            }
            days_left -= diy;
            tm_year += 1;
        }
        let tm_yday = days_left;
        let months: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut tm_mon = 0;
        let mut mdays = days_left;
        for i in 0..12 {
            let md = if i == 1 && is_leap(1900 + tm_year) {
                months[i] + 1
            } else {
                months[i]
            };
            if mdays < md {
                tm_mon = i as i32;
                break;
            }
            mdays -= md;
        }
        let tm_mday = mdays + 1;
        let tm_wday = ((days_since_epoch + 4) % 7) as i32;
        #[repr(C)]
        struct RtcTime {
            tm_sec: i32,
            tm_min: i32,
            tm_hour: i32,
            tm_mday: i32,
            tm_mon: i32,
            tm_year: i32,
            tm_wday: i32,
            tm_yday: i32,
            tm_isdst: i32,
        }
        let rtc = RtcTime {
            tm_sec,
            tm_min,
            tm_hour,
            tm_mday,
            tm_mon,
            tm_year,
            tm_wday,
            tm_yday,
            tm_isdst: -1,
        };
        *translated_refmut(token, arg as *mut RtcTime) = rtc;
        return 0;
    }

    match request {
        TCGETS => {
            if !is_terminal_fd(fd) {
                return -25; // ENOTTY
            }
            if arg == 0 {
                return -14; // EFAULT
            }
            let term = Termios {
                c_iflag: 0x0500, // ICRNL | IXON
                c_oflag: 0x0005, // OPOST | ONLCR
                c_cflag: 0x04bf, // CREAD | CS8 | B38400 | ...
                c_lflag: 0x8a3b, // ISIG | ICANON | ECHO | ECHOE | ECHOK | ...
                c_line: 0,
                c_cc: [0; 19],
            };
            *translated_refmut(token, arg as *mut Termios) = term;
            0
        }
        TIOCGWINSZ => {
            if arg == 0 {
                return -14;
            }
            let ws = Winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            *translated_refmut(token, arg as *mut Winsize) = ws;
            0
        }
        _ => -25, // ENOTTY
    }
}

pub fn sys_close(fd: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if inner.close_fd(fd) {
        return 0;
    }
    -9
}

pub fn sys_pipe(pipe: *mut usize) -> isize {
    sys_pipe2(pipe, 0)
}

pub fn sys_pipe2(pipe: *mut usize, flags: usize) -> isize {
    const O_NONBLOCK: usize = 0x800;
    const O_CLOEXEC: usize = 0x80000;
    if pipe.is_null() || flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        return -1;
    }
    let task = current_task().unwrap();
    let token = current_user_token();
    let (read_fd, write_fd) = {
        let mut inner = task.inner_exclusive_access();
        let (pipe_read, pipe_write) = make_pipe(flags);
        let read_fd = inner.alloc_fd();
        if read_fd >= inner.fd_table.len() { return -24; }
        inner.fd_table[read_fd] = Some(pipe_read);
        let write_fd = inner.alloc_fd();
        if write_fd >= inner.fd_table.len() { inner.fd_table[read_fd] = None; return -24; }
        inner.fd_table[write_fd] = Some(pipe_write);
        if flags & O_CLOEXEC != 0 {
            inner.fd_cloexec[read_fd] = true;
            inner.fd_cloexec[write_fd] = true;
        }
        (read_fd, write_fd)
    };
    let p = pipe as *mut u8;
    *translated_refmut(token, p as *mut u32) = read_fd as u32;
    *translated_refmut(token, unsafe { p.add(4) } as *mut u32) = write_fd as u32;
    0
}

pub fn sys_dup(fd: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    if inner.fd_table[fd].is_none() {
        return -9;
    }
    let new_fd = inner.alloc_fd();
    if new_fd >= inner.fd_table.len() {
        return -24;
    }
    let file = Arc::clone(inner.fd_table[fd].as_ref().unwrap());
    file.dup_ref();
    inner.fd_table[new_fd] = Some(file);
    new_fd as isize
}

pub fn sys_dup2(fd: usize, new_fd: usize) -> isize {
    sys_dup3(fd, new_fd, 0)
}

pub fn sys_dup3(fd: usize, new_fd: usize, flags: usize) -> isize {
    const O_CLOEXEC: usize = 0x80000;
    const MAX_FD: usize = 1024;
    if flags != 0 && flags != O_CLOEXEC {
        return -22;
    }
    if fd == new_fd {
        return new_fd as isize;
    }
    if new_fd >= MAX_FD {
        return -9;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    let Some(file) = inner.fd_table[fd].as_ref().cloned() else {
        return -9;
    };
    file.dup_ref();
    while inner.fd_table.len() <= new_fd {
        inner.fd_table.push(None);
        inner.fd_cloexec.push(false);
    }
    inner.close_fd(new_fd);
    inner.fd_table[new_fd] = Some(file);
    inner.fd_cloexec[new_fd] = (flags & O_CLOEXEC) != 0;
    new_fd as isize
}

pub fn sys_lseek(fd: usize, offset: i64, whence: i32) -> isize {
    const SEEK_SET: i32 = 0;
    const SEEK_CUR: i32 = 1;
    const SEEK_END: i32 = 2;

    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9; // EBADF
    }
    let Some(file) = &inner.fd_table[fd] else {
        return -9; // EBADF
    };
    let file = file.clone();
    drop(inner);

    let new_off: i64 = match whence {
        SEEK_SET => offset,
        SEEK_CUR => {
            let cur = file.get_offset() as i64;
            cur + offset
        }
        SEEK_END => {
            let size = file.stat().st_size;
            size + offset
        }
        _ => return -22, // EINVAL
    };

    if new_off < 0 {
        return -22; // EINVAL
    }

    file.seek(new_off as usize);
    new_off as isize
}

pub fn sys_renameat2(
    olddirfd: isize,
    oldpath: *const u8,
    newdirfd: isize,
    newpath: *const u8,
    _flags: usize,
) -> isize {
    if oldpath.is_null() || newpath.is_null() {
        return -14; // EFAULT
    }
    let token = current_user_token();
    let old_str = translated_str(token, oldpath);
    let new_str = translated_str(token, newpath);
    if old_str.is_empty() || new_str.is_empty() {
        return -2; // ENOENT
    }
    let old_base = match resolve_base_dir(olddirfd) {
        Some(b) => b,
        None => return -2,
    };
    let new_base = match resolve_base_dir(newdirfd) {
        Some(b) => b,
        None => return -2,
    };
    if rename_at(Some(old_base), &old_str, Some(new_base), &new_str) {
        0
    } else {
        -1
    }
}

pub fn sys_symlinkat(target: *const u8, newdirfd: isize, linkpath: *const u8) -> isize {
    if target.is_null() || linkpath.is_null() {
        return -14; // EFAULT
    }
    let token = current_user_token();
    let target_str = translated_str(token, target);
    let linkpath_str = translated_str(token, linkpath);
    if target_str.is_empty() || linkpath_str.is_empty() {
        return -2; // ENOENT
    }
    let base = match resolve_base_dir(newdirfd) {
        Some(b) => b,
        None => return -2,
    };
    if symlink_at(Some(base), &target_str, &linkpath_str) {
        0
    } else {
        -1
    }
}

pub fn sys_readlinkat(dirfd: isize, path: *const u8, buf: *mut u8, bufsiz: usize) -> isize {
    if buf.is_null() || bufsiz == 0 {
        return -22; // EINVAL
    }

    let token = current_user_token();
    let path_str = translated_str(token, path);
    if path_str.is_empty() {
        return -2; // ENOENT
    }

    let base = match resolve_base_dir(dirfd) {
        Some(b) => b,
        None => return -2,
    };
    let dentry = match vfs_resolve_with_fallback(&base, &path_str) {
        Some(d) => d,
        None => return -2,
    };

    let is_link = match dentry.super_block.get_inode_info(&dentry.fs_path) {
        Some(info) => info.is_link,
        None => false,
    };

    if !is_link {
        return -22; // EINVAL — not a symlink
    }

    let link_target = {
        let mut link_data = vec![0u8; bufsiz.min(4096)];
        let n = dentry
            .super_block
            .read_at(&dentry.fs_path, 0, &mut link_data);
        link_data.truncate(n);
        link_data
    };

    let len = link_target.len().min(bufsiz);
    let user_buf = crate::mm::translated_byte_buffer(token, buf, len);
    user_buf.copy_from_slice(&link_target[..len]);
    len as isize
}

pub fn sys_fstatat(dirfd: isize, path: *const u8, stat_buf: *mut u8, flags: usize) -> isize {
    const AT_EMPTY_PATH: usize = 0x1000;

    if stat_buf.is_null() {
        return -22; // EINVAL
    }

    if flags & AT_EMPTY_PATH != 0 {
        return sys_fstat(dirfd as usize, stat_buf);
    }

    let token = current_user_token();
    let path_str = translated_str(token, path);
    if path_str.is_empty() {
        return -2; // ENOENT
    }

    // Special device files
    if path_str == "/dev/null" || path_str == "/dev/zero" || path_str == "/dev/null/invalid"
        || path_str == "/dev/urandom" || path_str == "/dev/random" || path_str == "/dev/rtc"
        || path_str == "/dev/misc/rtc"
    {
        let st = Kstat::new(crate::fs::S_IFCHR);
        *translated_refmut(token, stat_buf as *mut Kstat) = st;
        return 0;
    }

    let base = match resolve_base_dir(dirfd) {
        Some(b) => b,
        None => return -2,
    };
    let dentry = match vfs_resolve_path(&base, &path_str) {
        Some(d) => d,
        None => return -2,
    };

    let inode = crate::fs::inode::OSInode::new(false, false, false, dentry);
    let st = inode.stat();
    *translated_refmut(token, stat_buf as *mut Kstat) = st;
    0
}

pub fn sys_statx(
    dirfd: isize,
    path: *const u8,
    flags: u32,
    _mask: u32,
    statx_buf: *mut u8,
) -> isize {
    const AT_EMPTY_PATH: u32 = 0x1000;

    if statx_buf.is_null() {
        return -22;
    }

    if flags & AT_EMPTY_PATH != 0 {
        let task = current_task().unwrap();
        let fd = dirfd as usize;
        let inner = task.inner_exclusive_access();
        if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
            return -9;
        }
        let file = inner.fd_table[fd].as_ref().unwrap().clone();
        drop(inner);
        let st = file.stat();
        let token = current_user_token();
        write_statx(statx_buf, &st, _mask);
        return 0;
    }

    // Validate path pointer
    if !path.is_null() {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        let end_of_page = ((path as usize) + crate::config::PAGE_SIZE) & !(crate::config::PAGE_SIZE - 1);
        let first_page_len = 256.min(end_of_page - (path as usize));
        if !inner.memory_set.valid_user_buf(path, first_page_len, false) {
            return -14;
        }
    }

    let token = current_user_token();
    let path_str = translated_str(token, path);
    if path_str.is_empty() {
        return -2;
    }

    // Special device files
    if path_str == "/dev/null" || path_str == "/dev/zero" || path_str == "/dev/null/invalid"
        || path_str == "/dev/urandom" || path_str == "/dev/random" || path_str == "/dev/rtc"
        || path_str == "/dev/misc/rtc"
    {
        let st = Kstat::new(crate::fs::S_IFCHR);
        write_statx(statx_buf, &st, _mask);
        return 0;
    }
    if path_str.len() > 255 {
        return -36;
    }

    // Resolve dirfd manually to get proper error codes
    let base = if dirfd == AT_FDCWD {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        inner.cwd.clone()
    } else if dirfd < 0 {
        return -9;
    } else {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        let fd = dirfd as usize;
        if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
            return -9;
        }
        let file = inner.fd_table[fd].as_ref().unwrap().clone();
        if !file.is_dir() {
            return -20;
        }
        let path = file.path().unwrap_or_default();
        let cwd = inner.cwd.clone();
        drop(inner);
        match vfs_resolve_path(&cwd, &path) {
            Some(d) => d,
            None => return -2,
        }
    };
    let dentry = match vfs_resolve_path(&base, &path_str) {
        Some(d) => d,
        None => return -2,
    };

    let inode = crate::fs::inode::OSInode::new(false, false, false, dentry);
    let st = inode.stat();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner.memory_set.valid_user_buf(statx_buf, core::mem::size_of::<Statx>(), true) {
        return -14;
    }
    drop(inner);
    write_statx(statx_buf, &st, _mask);
    0
}

fn write_statx(buf: *mut u8, st: &Kstat, mask: u32) {
    let token = current_user_token();
    let statx = translated_refmut(token, buf as *mut Statx);
    statx.stx_mask = mask & 0x7ff; // STATX_BASIC_STATS
    statx.stx_blksize = st.st_blksize;
    statx.stx_attributes = 0;
    statx.stx_nlink = st.st_nlink;
    statx.stx_uid = st.st_uid;
    statx.stx_gid = st.st_gid;
    statx.stx_mode = st.st_mode as u16;
    statx.pad1 = 0;
    statx.stx_ino = st.st_ino;
    statx.stx_size = st.st_size as u64;
    statx.stx_blocks = st.st_blocks as u64;
    statx.stx_attributes_mask = 0;
    statx.stx_atime.tv_sec = st.st_atime_sec;
    statx.stx_atime.tv_nsec = st.st_atime_nsec as u32;
    statx.stx_atime.pad = 0;
    statx.stx_btime.tv_sec = 0;
    statx.stx_btime.tv_nsec = 0;
    statx.stx_btime.pad = 0;
    statx.stx_ctime.tv_sec = st.st_ctime_sec;
    statx.stx_ctime.tv_nsec = st.st_ctime_nsec as u32;
    statx.stx_ctime.pad = 0;
    statx.stx_mtime.tv_sec = st.st_mtime_sec;
    statx.stx_mtime.tv_nsec = st.st_mtime_nsec as u32;
    statx.stx_mtime.pad = 0;
    statx.stx_rdev_major = 0;
    statx.stx_rdev_minor = 0;
    statx.stx_dev_major = 0;
    statx.stx_dev_minor = 0;
}

#[repr(C)]
struct StatxTime {
    tv_sec: i64,
    tv_nsec: u32,
    pad: i32,
}

#[repr(C)]
struct Statx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    pad1: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: StatxTime,
    stx_btime: StatxTime,
    stx_ctime: StatxTime,
    stx_mtime: StatxTime,
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
}

pub fn sys_fstat(fd: usize, stat_buf: *mut u8) -> isize {
    let task = current_task().unwrap();
    let token = current_user_token();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if stat_buf.is_null() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        drop(inner);
        let st = file.stat();
        *translated_refmut(token, stat_buf as *mut Kstat) = st;
        0
    } else {
        -1
    }
}

pub fn sys_getdents64(fd: usize, dirp: *mut u8, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    if dirp.is_null() {
        return -1;
    }
    let task = current_task().unwrap();
    let token = current_user_token();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        drop(inner);
        let user_buf = translated_byte_buffer(token, dirp, len);
        file.getdents64(user_buf)
    } else {
        -1
    }
}

// ==================== mount / umount ====================

use crate::drivers::BLOCK_DEVICE;
use crate::fs::vfs::{vfs_mount, vfs_umount};

pub fn sys_mount(
    _source: *const u8,
    target: *const u8,
    fstype: *const u8,
    _mountflags: usize,
    _data: *const u8,
) -> isize {
    let token = current_user_token();
    let target_path = translated_str(token, target);
    let fstype_str = translated_str(token, fstype);

    if fstype_str.as_str() != "ext4" && fstype_str.as_str() != "vfat" {
        return -22;
    }

    #[cfg(target_arch = "loongarch64")]
    {
        let _ = target_path;
        return 0;
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let task = current_task().unwrap();
        let cwd = {
            let inner = task.inner_exclusive_access();
            inner.cwd.clone()
        };

        let mnt_point = match vfs_resolve_path(&cwd, target_path.as_str()) {
            Some(d) => d,
            None => return -1,
        };

        if !mnt_point.is_dir {
            return -20;
        }

        // 如果已经挂载，幂等成功
        use crate::fs::vfs::MOUNT_TABLE;
        {
            let table = MOUNT_TABLE.exclusive_access();
            if table.iter().any(|m| m.mnt_point.path == mnt_point.path) {
                return 0;
            }
        }

        let sb = match VfsSuperBlock::new_ext4(BLOCK_DEVICE.clone(), "ext4") {
            Some(sb) => sb,
            None => return -17, // EBUSY
        };

        if vfs_mount(mnt_point, sb) {
            0
        } else {
            -16
        }
    }
}

pub fn sys_umount2(target: *const u8, _flags: usize) -> isize {
    #[cfg(target_arch = "loongarch64")]
    {
        let _ = target;
        return 0;
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let token = current_user_token();
        let target_path = translated_str(token, target);

        let task = current_task().unwrap();
        let cwd = {
            let inner = task.inner_exclusive_access();
            inner.cwd.clone()
        };

        use crate::fs::vfs::{vfs_resolve_path, MOUNT_TABLE};

        let dentry = match vfs_resolve_path(&cwd, target_path.as_str()) {
            Some(d) => d,
            None => return -1,
        };

        let path = &dentry.path;
        let mut table = MOUNT_TABLE.exclusive_access();
        if let Some(idx) = table.iter().position(|m| &m.mnt_point.path == path) {
            let mount = table.remove(idx);
            mount.mnt_root.super_block.force_umount();
        }
        0
    }
}

#[repr(C)]
struct StatFs {
    f_type: i64,
    f_bsize: i64,
    f_blocks: i64,
    f_bfree: i64,
    f_bavail: i64,
    f_files: i64,
    f_ffree: i64,
    f_fsid: [i32; 2],
    f_namelen: i64,
    f_frsize: i64,
    f_flags: i64,
    f_spare: [i64; 4],
}

const EXT4_SUPER_MAGIC: i64 = 0xEF53;

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn fill_statfs_from_dentry(dentry: &Arc<Dentry>, buf: &mut StatFs) {
    let stats = dentry.super_block.get_fs_stats();
    let block_size = stats.block_size as i64;
    let blocks = stats.blocks_count as i64;
    let free_blocks = stats.free_blocks_count as i64;
    let inodes = stats.inodes_count as i64;
    let free_inodes = stats.free_inodes_count as i64;
    *buf = StatFs {
        f_type: EXT4_SUPER_MAGIC,
        f_bsize: block_size,
        f_blocks: blocks,
        f_bfree: free_blocks,
        f_bavail: free_blocks,
        f_files: inodes,
        f_ffree: free_inodes,
        f_fsid: [0, 0],
        f_namelen: 255,
        f_frsize: block_size,
        f_flags: 0,
        f_spare: [0; 4],
    };
}

pub fn sys_statfs(path: *const u8, buf: *mut u8) -> isize {
    let token = current_user_token();
    let path = translated_str(token, path);
    let task = current_task().unwrap();
    let cwd = {
        let inner = task.inner_exclusive_access();
        inner.cwd.clone()
    };
    let dentry = match vfs_resolve_path(&cwd, path.as_str()) {
        Some(d) => d,
        None => return -1,
    };
    let st = translated_refmut(token, buf as *mut StatFs);
    fill_statfs_from_dentry(&dentry, st);
    0
}

pub fn sys_pread64(fd: usize, buf: *mut u8, len: usize, offset: usize) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -1;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    if !file.readable() {
        return -1;
    }
    let dentry = match file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        Some(inode) => inode.dentry().clone(),
        None => return -1,
    };
    drop(inner);
    let user_buf = translated_byte_buffer(token, buf, len);
    let n = dentry.super_block.read_at(&dentry.fs_path, offset, user_buf);
    n as isize
}

pub fn sys_pwrite64(fd: usize, buf: *mut u8, len: usize, offset: usize) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -1;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    if !file.writable() {
        return -1;
    }
    let dentry = match file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        Some(inode) => inode.dentry().clone(),
        None => return -1,
    };
    drop(inner);
    let user_buf = translated_byte_buffer(token, buf, len);
    let n = dentry.super_block.write_at(&dentry.fs_path, offset, user_buf);
    n as isize
}

pub fn sys_preadv(fd: usize, iov: *const u8, iovcnt: usize, offset: usize) -> isize {
    use crate::mm::translated_ref;
    const IOV_MAX: usize = 1024;
    if iovcnt == 0 || iovcnt > IOV_MAX {
        return -22;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner.memory_set.valid_user_buf(iov, iovcnt * core::mem::size_of::<IoVec>(), false) {
        return -14;
    }
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    let dentry = match file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        Some(inode) => inode.dentry().clone(),
        None => return -22,
    };
    if !file.readable() {
        return -9;
    }
    drop(inner);
    #[repr(C)]
    struct IoVec {
        iov_base: usize,
        iov_len: usize,
    }
    let mut off = offset;
    let mut total = 0isize;
    for i in 0..iovcnt {
        let iov_ptr = unsafe { iov.add(i * core::mem::size_of::<IoVec>()) };
        let iov: &IoVec = translated_ref(token, iov_ptr as *const IoVec);
        if iov.iov_len == 0 {
            continue;
        }
        if iov.iov_len > 0x7FFF_FFFF {
            return -22;
        }
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        let ok = inner.memory_set.valid_user_buf(iov.iov_base as *const u8, iov.iov_len, true);
        drop(inner);
        if !ok {
            return -14;
        }
        let user_buf = crate::mm::translated_byte_buffer(token, iov.iov_base as *mut u8, iov.iov_len);
        let n = dentry.super_block.read_at(&dentry.fs_path, off, user_buf);
        total += n as isize;
        off += n;
        if n < iov.iov_len {
            break;
        }
    }
    total
}

pub fn sys_pwritev(fd: usize, iov: *const u8, iovcnt: usize, offset: usize) -> isize {
    use crate::mm::translated_ref;
    const IOV_MAX: usize = 1024;
    if iovcnt == 0 || iovcnt > IOV_MAX {
        return -22;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    // Validate iov array
    if !inner.memory_set.valid_user_buf(iov, iovcnt * core::mem::size_of::<IoVec>(), false) {
        return -14;
    }
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    let dentry = match file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        Some(inode) => inode.dentry().clone(),
        None => return -29,
    };
    if !file.writable() {
        return -9;
    }
    drop(inner);
    #[repr(C)]
    struct IoVec {
        iov_base: usize,
        iov_len: usize,
    }
    let mut off = offset;
    let mut total = 0isize;
    for i in 0..iovcnt {
        let iov_ptr = unsafe { iov.add(i * core::mem::size_of::<IoVec>()) };
        let iov: &IoVec = translated_ref(token, iov_ptr as *const IoVec);
        if iov.iov_len == 0 {
            continue;
        }
        if iov.iov_len > 0x7FFF_FFFF {
            return -22;
        }
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        let ok = inner.memory_set.valid_user_buf(iov.iov_base as *const u8, iov.iov_len, false);
        drop(inner);
        if !ok {
            return -14;
        }
        let user_buf = crate::mm::translated_byte_buffer(token, iov.iov_base as *mut u8, iov.iov_len);
        let n = dentry.super_block.write_at(&dentry.fs_path, off, user_buf);
        total += n as isize;
        off += n;
        if n < iov.iov_len {
            break;
        }
    }
    total
}

pub fn sys_preadv2(fd: usize, iov: *const u8, iovcnt: usize, offset: usize, _flags: usize) -> isize {
    sys_preadv(fd, iov, iovcnt, offset)
}

pub fn sys_pwritev2(fd: usize, iov: *const u8, iovcnt: usize, offset: usize, _flags: usize) -> isize {
    sys_pwritev(fd, iov, iovcnt, offset)
}

fn sync_fd(fd: usize) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    let dentry = match file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        Some(inode) => inode.dentry().clone(),
        None => return -22,
    };
    drop(inner);
    let rc = dentry.super_block.fsync(&dentry.fs_path);
    if rc == 0 { 0 } else { -5 }
}

pub fn sys_fsync(fd: usize) -> isize {
    sync_fd(fd)
}

pub fn sys_fdatasync(fd: usize) -> isize {
    sync_fd(fd)
}

pub fn sys_sync() -> isize {
    let rc = crate::fs::vfs::ROOT_SB.sync_all();
    if rc == 0 { 0 } else { -5 }
}

pub fn sys_syncfs(fd: usize) -> isize {
    sync_fd(fd)
}

pub fn sys_fallocate(fd: usize, mode: usize, offset: usize, len: usize) -> isize {
    const FALLOC_FL_KEEP_SIZE: usize = 0x01;
    if fd >= 1024 || len == 0 {
        return -22;
    }
    if mode & !FALLOC_FL_KEEP_SIZE != 0 {
        return -38;
    }
    if mode & FALLOC_FL_KEEP_SIZE != 0 {
        return 0;
    }
    crate::syscall::fs::sys_ftruncate(fd, (offset + len) as u64)
}

pub fn sys_fadvise64(fd: usize, offset: usize, len: usize, advice: usize) -> isize {
    const POSIX_FADV_NORMAL: usize = 0;
    const POSIX_FADV_RANDOM: usize = 1;
    const POSIX_FADV_SEQUENTIAL: usize = 2;
    const POSIX_FADV_WILLNEED: usize = 3;
    const POSIX_FADV_DONTNEED: usize = 4;
    const POSIX_FADV_NOREUSE: usize = 5;
    if advice > POSIX_FADV_NOREUSE {
        return -22;
    }
    0
}

pub fn sys_fstatfs(fd: usize, buf: *mut u8) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    let file = match &inner.fd_table[fd] {
        Some(f) => f.clone(),
        None => return -1,
    };
    let dentry = file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .map(|inode| inode.dentry());
    match dentry {
        Some(d) => {
            let st = translated_refmut(token, buf as *mut StatFs);
            fill_statfs_from_dentry(d, st);
            0
        }
        None => return -1,
    }
}

pub fn sys_ftruncate(fd: usize, length: u64) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    let file = match &inner.fd_table[fd] {
        Some(f) => f.clone(),
        None => return -1,
    };
    let dentry = file.as_any().downcast_ref::<crate::fs::inode::OSInode>()
        .map(|inode| inode.dentry());
    match dentry {
        Some(d) => {
            if d.super_block.truncate_file(&d.fs_path, length) {
                0
            } else {
                -1
            }
        }
        None => -1,
    }
}

// ---- flock (32) ----

pub fn sys_flock(_fd: usize, _op: usize) -> isize {
    0
}

// ---- mknodat (33) ----

pub fn sys_mknodat(dirfd: isize, path: *const u8, mode: u32, _dev: usize) -> isize {
    if path.is_null() {
        return -14;
    }
    // Validate path before reading
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let end_of_page = ((path as usize) + crate::config::PAGE_SIZE) & !(crate::config::PAGE_SIZE - 1);
    let first_page_len = 256.min(end_of_page - (path as usize));
    if !inner.memory_set.valid_user_buf(path, first_page_len, false) {
        return -14;
    }
    let token = inner.get_user_token();
    drop(inner);
    let path_str = translated_str(token, path);
    if path_str.is_empty() {
        return -2;
    }
    if path_str.len() > 255 {
        return -36;
    }
    let base = match resolve_base_dir(dirfd) {
        Some(b) => b,
        None => return -2,
    };
    // Check if already exists
    if crate::fs::vfs::vfs_resolve_path(&base, &path_str).is_some() {
        return -17;
    }
    let (parent, name) = match crate::fs::vfs::vfs_resolve_parent_and_name(&base, &path_str) {
        Some(p) => p,
        None => return -2,
    };
    match crate::fs::vfs::vfs_create(&parent, &name, false, 0o666) {
        Some(_) => 0,
        None => -2,
    }
}

// ---- readahead (213) ----

pub fn sys_readahead(_fd: usize, _offset: usize, _count: usize) -> isize {
    0
}

// ---- copy_file_range (285) ----

pub fn sys_copy_file_range(fd_in: usize, off_in: *mut usize, fd_out: usize, off_out: *mut usize, len: usize, _flags: usize) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd_in >= inner.fd_table.len() || inner.fd_table[fd_in].is_none() {
        return -9;
    }
    if fd_out >= inner.fd_table.len() || inner.fd_table[fd_out].is_none() {
        return -9;
    }
    let f_in = inner.fd_table[fd_in].as_ref().unwrap().clone();
    let f_out = inner.fd_table[fd_out].as_ref().unwrap().clone();
    drop(inner);

    if let Some(ref off) = unsafe { off_in.as_ref() } {
        f_in.seek(**off);
    }
    if let Some(ref off) = unsafe { off_out.as_ref() } {
        f_out.seek(**off);
    }

    let mut buf = [0u8; 4096];
    let mut total: isize = 0;
    while (total as usize) < len {
        let chunk = (len - total as usize).min(buf.len());
        let n = f_in.read(&mut buf[..chunk]);
        if n <= 0 { break; }
        let w = f_out.write(&mut buf[..n as usize]);
        if w <= 0 { break; }
        total += w;
    }
    total
}

// ---- memfd_create (279) ----

pub fn sys_memfd_create(_name: *const u8, _flags: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let fd = inner.alloc_fd();
    if fd >= inner.fd_table.len() {
        return -24;
    }
    inner.fd_table[fd] = Some(alloc::sync::Arc::new(crate::fs::DevNull));
    fd as isize
}

// ---- close_range (436) ----

pub fn sys_close_range(first: usize, last: usize, _flags: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let last = last.min(inner.fd_table.len().saturating_sub(1));
    for fd in first..=last {
        inner.fd_table[fd].take();
    }
    0
}

// ---- socketpair (199) ----

pub fn sys_socketpair(_domain: usize, _typ: usize, _protocol: usize, _sv: *mut usize) -> isize {
    0
}
