use crate::fs::vfs::{vfs_resolve_path, vfs_resolve_with_fallback, Dentry, VfsSuperBlock};
use crate::fs::{make_pipe, mkdir_at, open_file_at, rename_at, symlink_at, unlink_at, File, Kstat, OpenFlags};
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
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() {
            return -1;
        }
        let file = file.clone();
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
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if !file.readable() {
            return -1;
        }
        drop(inner);
        file.read(translated_byte_buffer(token, buf, len))
    } else {
        -1
    }
}

pub fn sys_open(path: *const u8, flags: u32) -> isize {
    sys_openat(AT_FDCWD, path, flags, 0)
}

pub fn sys_openat(dirfd: isize, path: *const u8, flags: u32, _mode: u32) -> isize {
    let task = current_task().unwrap();
    let token = current_user_token();
    let path = translated_str(token, path);

    // proc filesystem intercept
    if let Some(file) = open_proc(&path) {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        inner.fd_table[fd] = Some(file);
        return fd as isize;
    }

    let open_flags = OpenFlags::from_raw(flags);
    let base_dir = resolve_base_dir(dirfd);
    let inode = open_file_at(base_dir, path.as_str(), open_flags);

    if let Some(inode) = inode {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        inner.fd_table[fd] = Some(inode);
        fd as isize
    } else {
        -1
    }
}

fn open_proc(path: &str) -> Option<Arc<dyn File + Send + Sync>> {
    use crate::fs::proc::{DevRtcFile, ProcDirFile, ProcMeminfoFile, ProcMountsFile, ProcPidStatFile};
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
    if let Some(pid_str) = path.strip_prefix("/proc/").and_then(|s| s.strip_suffix("/stat")) {
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

pub fn sys_unlinkat(dirfd: isize, path: *const u8, flags: u32) -> isize {
    if flags != 0 || path.is_null() {
        return -1;
    }
    let token = current_user_token();
    let path = translated_str(token, path);
    let base_dir = match resolve_base_dir(dirfd) {
        Some(base) => base,
        None => return -1,
    };
    if unlink_at(Some(base_dir), path.as_str()) {
        0
    } else {
        -1
    }
}

pub fn sys_chdir(path: *const u8) -> isize {
    if path.is_null() {
        return -1;
    }
    let task = current_task().unwrap();
    let token = current_user_token();
    let path = translated_str(token, path);
    let cwd = {
        let inner = task.inner_exclusive_access();
        inner.cwd.clone()
    };
    let Some(dir_dentry) = vfs_resolve_path(&cwd, &path) else {
        return -1;
    };
    if !dir_dentry.is_dir {
        return -20;
    }
    let mut inner = task.inner_exclusive_access();
    inner.cwd = dir_dentry;
    0
}

pub fn sys_getcwd(buf: *mut u8, size: usize) -> isize {
    if buf.is_null() || size == 0 {
        return 0;
    }
    let task = current_task().unwrap();
    let cwd_path = {
        let inner = task.inner_exclusive_access();
        inner.cwd.path.clone()
    };
    let need = cwd_path.len() + 1;
    if need > size {
        return 0;
    }
    let token = current_user_token();
    let user_buf = translated_byte_buffer(token, buf, size);
    user_buf[..cwd_path.len()].copy_from_slice(cwd_path.as_bytes());
    user_buf[cwd_path.len()] = 0;
    buf as isize
}

pub fn sys_utimensat(
    dirfd: isize,
    path: *const u8,
    times: *const u8,
    _flags: usize,
) -> isize {
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
        let permitted = dentry.super_block.with_ext4(|ext4| {
            let inode_ref = ext4.get_inode_ref(dentry.inode_num);
            if inode_ref.inode.uid() as u32 == euid {
                return true;
            }
            let mode = inode_ref.inode.mode();
            let perm = if inode_ref.inode.gid() as u32 == egid {
                (mode >> 3) & 0o7
            } else {
                mode & 0o7
            };
            (perm & 0o2) != 0
        });
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

    dentry.super_block.with_ext4(|ext4| {
        let mut inode_ref = ext4.get_inode_ref(dentry.inode_num);
        if let Some(a) = new_atime {
            inode_ref.inode.set_atime(a);
        }
        if let Some(m) = new_mtime {
            inode_ref.inode.set_mtime(m);
        }
        if changed {
            inode_ref.inode.set_ctime(now_sec);
        }
        ext4.write_back_inode(&mut inode_ref);
    });

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

    let allowed = dentry.super_block.with_ext4(|ext4| {
        let inode_ref = ext4.get_inode_ref(dentry.inode_num);
        let m = inode_ref.inode.mode();
        let owner_perm = (m >> 6) & 0o7;
        let group_perm = (m >> 3) & 0o7;
        let other_perm = m & 0o7;
        let perm = if inode_ref.inode.uid() as u32 == euid {
            owner_perm
        } else if inode_ref.inode.gid() as u32 == egid {
            group_perm
        } else {
            other_perm
        };
        if (mode & R_OK) != 0 && (perm & 0o4) == 0 { return false; }
        if (mode & W_OK) != 0 && (perm & 0o2) == 0 { return false; }
        if (mode & X_OK) != 0 && (perm & 0o1) == 0 { return false; }
        true
    });

    if allowed { 0 } else { -13 } // EACCES
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
            inner.fd_table[new_fd] = Some(file);
            if cmd == F_DUPFD_CLOEXEC {
                inner.fd_cloexec[new_fd] = true;
            }
            new_fd as isize
        }
        F_GETFD => {
            if inner.fd_cloexec[fd] { FD_CLOEXEC as isize } else { 0 }
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
                if file.set_flags(arg) { 0 } else { -22 }
            } else {
                -9
            }
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
    fd == 0 || fd == 1  // stdin/stdout
}

pub fn sys_ioctl(fd: usize, request: usize, arg: usize) -> isize {
    let token = current_user_token();
    let request = request & 0xffffffff;

    // RTC ioctl
    const RTC_RD_TIME: usize = 0x80247009;
    if request == RTC_RD_TIME {
        if fd >= 1024 { return -9; }
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
            if days_left < diy { break; }
            days_left -= diy;
            tm_year += 1;
        }
        let tm_yday = days_left;
        let months: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut tm_mon = 0;
        let mut mdays = days_left;
        for i in 0..12 {
            let md = if i == 1 && is_leap(1900 + tm_year) { months[i] + 1 } else { months[i] };
            if mdays < md { tm_mon = i as i32; break; }
            mdays -= md;
        }
        let tm_mday = mdays + 1;
        let tm_wday = ((days_since_epoch + 4) % 7) as i32;
        #[repr(C)]
        struct RtcTime {
            tm_sec: i32, tm_min: i32, tm_hour: i32, tm_mday: i32, tm_mon: i32,
            tm_year: i32, tm_wday: i32, tm_yday: i32, tm_isdst: i32,
        }
        let rtc = RtcTime { tm_sec, tm_min, tm_hour, tm_mday, tm_mon, tm_year, tm_wday, tm_yday, tm_isdst: -1 };
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
                c_iflag: 0x0500,  // ICRNL | IXON
                c_oflag: 0x0005,  // OPOST | ONLCR
                c_cflag: 0x04bf,  // CREAD | CS8 | B38400 | ...
                c_lflag: 0x8a3b,  // ISIG | ICANON | ECHO | ECHOE | ECHOK | ...
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
    if fd < inner.fd_table.len() && inner.fd_table[fd].is_some() {
        inner.fd_table[fd].take();
        return 0;
    }
    -1
}

pub fn sys_pipe(pipe: *mut usize) -> isize {
    sys_pipe2(pipe, 0)
}

pub fn sys_pipe2(pipe: *mut usize, flags: usize) -> isize {
    if flags != 0 || pipe.is_null() {
        return -1;
    }
    let task = current_task().unwrap();
    let token = current_user_token();
    let mut inner = task.inner_exclusive_access();
    let (pipe_read, pipe_write) = make_pipe();
    let read_fd = inner.alloc_fd();
    inner.fd_table[read_fd] = Some(pipe_read);
    let write_fd = inner.alloc_fd();
    inner.fd_table[write_fd] = Some(pipe_write);
    let p = pipe as *mut u8;
    *translated_refmut(token, p as *mut u32) = read_fd as u32;
    *translated_refmut(token, unsafe { p.add(4) } as *mut u32) = write_fd as u32;
    0
}

pub fn sys_dup(fd: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    let new_fd = inner.alloc_fd();
    inner.fd_table[new_fd] = Some(Arc::clone(inner.fd_table[fd].as_ref().unwrap()));
    new_fd as isize
}

pub fn sys_dup2(fd: usize, new_fd: usize) -> isize {
    sys_dup3(fd, new_fd, 0)
}

pub fn sys_dup3(fd: usize, new_fd: usize, flags: usize) -> isize {
    const O_CLOEXEC: usize = 0x80000;
    if flags != 0 && flags != O_CLOEXEC {
        return -22; // EINVAL
    }
    if fd == new_fd {
        return new_fd as isize;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    let Some(file) = inner.fd_table[fd].as_ref().cloned() else {
        return -1;
    };
    while inner.fd_table.len() <= new_fd {
        inner.fd_table.push(None);
        inner.fd_cloexec.push(false);
    }
    inner.fd_table[new_fd].take();
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

    let is_link = dentry.super_block.with_ext4(|ext4| {
        let inode_ref = ext4.get_inode_ref(dentry.inode_num);
        inode_ref.inode.is_link()
    });

    if !is_link {
        return -22; // EINVAL — not a symlink
    }

    let link_target = dentry.super_block.with_ext4(|ext4| {
        let mut link_data = vec![0u8; bufsiz.min(4096)];
        let n = ext4
            .read_at(dentry.inode_num, 0, &mut link_data)
            .ok()
            .unwrap_or(0);
        link_data.truncate(n);
        link_data
    });

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

    let base = match resolve_base_dir(dirfd) {
        Some(b) => b,
        None => return -2,
    };
    let dentry = match vfs_resolve_path(&base, &path_str) {
        Some(d) => d,
        None => return -2,
    };

    let inode = crate::fs::inode::OSInode::new(false, false, dentry);
    let st = inode.stat();
    *translated_refmut(token, stat_buf as *mut Kstat) = st;
    0
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

    let sb = VfsSuperBlock::new_ext4(BLOCK_DEVICE.clone(), "ext4");

    if vfs_mount(mnt_point, sb) {
        0
    } else {
        -16
    }
}

pub fn sys_umount2(target: *const u8, _flags: usize) -> isize {
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
        table.remove(idx);
    }
    0
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
    dentry.super_block.with_ext4(|ext4| {
        let sb = &ext4.super_block;
        let block_size = (1024u64 << sb.log_block_size) as i64;
        let blocks = sb.blocks_count_lo as i64;
        let free_blocks = sb.free_blocks_count_lo as i64;
        let inodes = sb.inodes_count as i64;
        let free_inodes = sb.free_inodes_count as i64;
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
    });
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
    let dentry = file.as_any().downcast_ref::<crate::fs::inode::OSInode>()
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
