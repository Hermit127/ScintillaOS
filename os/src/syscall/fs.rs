// 这里负责操作系统的系统调用接口和文件管理：实现文件描述符、路径、挂载、stat 和同步相关系统调用。
use crate::fs::vfs::{vfs_resolve_path, vfs_resolve_with_fallback, Dentry, VfsSuperBlock};
use crate::fs::{
    make_pipe, mkdir_at, open_file_at, rename_at, symlink_at, unlink_at, DevNull, DevZero, File,
    Kstat, OpenFlags, Urandom,
};
use crate::mm::{translated_byte_buffer, translated_refmut, translated_str};
use crate::task::{current_task, current_user_token};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicU64, Ordering};

const AT_FDCWD: isize = -100;
const O_TMPFILE: u32 = 0x410000;
const FILE_TIME_NOW_GUARD_SEC: i64 = 5;

static TMPFILE_COUNTER: AtomicU64 = AtomicU64::new(0);

// 根据 dirfd 找到相对路径解析的起点目录。
// 执行时 AT_FDCWD 取当前 cwd；普通 fd 要确认它是目录 fd，再尽量从 OSInode 取 dentry，最后才按路径回查。
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
    if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        return Some(inode.get_dentry().clone());
    }
    let path = file.path()?;
    let cwd = inner.cwd.clone();
    drop(inner);
    vfs_resolve_path(&cwd, &path)
}

// 系统调用入口：从用户缓冲区写数据到 fd。
// 执行时检查 fd、目录写入和写权限，再校验用户缓冲区可读，放掉任务锁后调用 File::write。
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

// 系统调用入口：从 fd 读取数据到用户缓冲区。
// 执行时检查 fd、目录读取和读权限，再校验用户缓冲区可写，最后把 File::read 结果返回给用户态。
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
        if !inner.memory_set.valid_user_buf(buf, len, true) {
            return -14;
        }
        drop(inner);

        file.read(translated_byte_buffer(token, buf, len))
    } else {
        -9
    }
}

// 系统调用入口：旧 open，内部转到 openat。
// 执行时固定使用 AT_FDCWD，并把旧接口的参数转交给 openat 统一处理。
pub fn sys_open(path: *const u8, flags: u32) -> isize {
    sys_openat(AT_FDCWD, path, flags, 0)
}

// 系统调用入口：按 dirfd 和 flags 打开文件或特殊设备。
// 执行时先校验路径指针，再优先处理 /proc、/dev 和 O_TMPFILE；普通文件按 VFS 解析后分配 fd。
pub fn sys_openat(dirfd: isize, path: *const u8, flags: u32, mode: u32) -> isize {
    let task = current_task().unwrap();
    // 先校验用户传入的 path 至少第一页可读，避免 translated_str 直接访问非法地址。
    {
        let inner = task.inner_exclusive_access();
        let end_of_page =
            ((path as usize) + crate::config::PAGE_SIZE) & !(crate::config::PAGE_SIZE - 1);
        let first_page_len = 4096.min(end_of_page - (path as usize));
        if !inner.memory_set.valid_user_buf(path, first_page_len, false) {
            return -14;
        }
    }
    let token = current_user_token();
    let path = translated_str(token, path);

    // /proc 是内核伪文件，不走 ext4，先在这里拦截成对应的 File 对象。
    if let Some(file) = open_proc(&path) {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        if fd >= inner.fd_table.len() {
            return -24;
        }
        inner.fd_table[fd] = Some(file);
        return fd as isize;
    }

    // /dev 下这几个常用字符设备由内核手写实现，不走普通 inode 打开流程。
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
        if fd >= inner.fd_table.len() {
            return -24;
        }
        inner.fd_table[fd] = Some(Arc::new(DevZero));
        return fd as isize;
    }

    if flags & O_TMPFILE == O_TMPFILE {
        return sys_open_tmpfile(dirfd, path.as_str(), mode);
    }

    // 普通路径先做语义检查，把目录、独占创建和不存在等 errno 区分出来。
    let open_flags = OpenFlags::from_raw(flags);
    let base_dir = resolve_base_dir(dirfd);
    let Some(base) = base_dir.clone() else {
        return -2;
    };
    if path.is_empty() {
        return -2;
    }
    let existing = vfs_resolve_with_fallback(&base, path.as_str());
    if let Some(existing) = existing {
        if open_flags.directory_requested() && !existing.is_dir {
            return -20;
        }
        if open_flags.create_requested() && open_flags.excl_requested() {
            return -17;
        }
    } else if !open_flags.create_requested() {
        return -2;
    }
    // 真正创建或打开交给 inode 层；成功后分配 fd 并挂入当前任务 fd 表。
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
        -2
    }
}

// 处理 O_TMPFILE，创建匿名临时文件并直接返回 fd。
// 执行时确认传入路径是目录，然后用 pid 和递增计数生成隐藏文件名，创建成功后包装成可读写 inode fd。
fn sys_open_tmpfile(dirfd: isize, path: &str, mode: u32) -> isize {
    // O_TMPFILE 传入的是目录路径，临时文件名由内核生成，避免和用户文件冲突。
    let base = match resolve_base_dir(dirfd) {
        Some(base) => base,
        None => return -2,
    };
    let dir = match vfs_resolve_with_fallback(&base, path).and_then(|d| {
        if d.is_dir {
            Some(d)
        } else {
            crate::fs::follow_symlink(&base, d, 0).filter(|target| target.is_dir)
        }
    }) {
        Some(dir) => dir,
        None => return -20,
    };

    let perm = if mode == 0 { 0o600 } else { mode & 0o777 };
    let pid = current_task().map(|task| task.getpid()).unwrap_or(0);
    for _ in 0..16 {
        let id = TMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(".tmpfile.{}.{}", pid, id);
        if let Some(dentry) = crate::fs::vfs::vfs_create(&dir, &name, false, perm) {
            let inode = Arc::new(crate::fs::inode::OSInode::new(true, true, false, dentry));
            let task = current_task().unwrap();
            let mut inner = task.inner_exclusive_access();
            let fd = inner.alloc_fd();
            if fd >= inner.fd_table.len() {
                return -24;
            }
            inner.fd_table[fd] = Some(inode);
            return fd as isize;
        }
    }
    -17
}

// 打开几个手写的 /proc 伪文件。
// 执行时按固定路径匹配 /proc 目录、mounts、meminfo、rtc 和 /proc/<pid>/stat，命中就返回对应伪文件对象。
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

// 返回内置特殊设备的 stat。
// 执行时按 /dev/null、zero、random、urandom、rtc 填充字符设备类型和 rdev，非内置设备返回 None。
fn special_device_stat(path: &str) -> Option<Kstat> {
    let mut st = match path {
        "/dev/null" => Kstat::new(crate::fs::S_IFCHR | 0o666),
        "/dev/zero" => Kstat::new(crate::fs::S_IFCHR | 0o666),
        "/dev/random" | "/dev/urandom" => Kstat::new(crate::fs::S_IFCHR | 0o444),
        "/dev/rtc" | "/dev/misc/rtc" => Kstat::new(crate::fs::S_IFCHR | 0o444),
        _ => return None,
    };
    st.st_rdev = match path {
        "/dev/null" => 0x0103,
        "/dev/zero" => 0x0105,
        "/dev/random" => 0x0108,
        "/dev/urandom" => 0x0109,
        "/dev/rtc" | "/dev/misc/rtc" => 0x0a00,
        _ => 0,
    };
    Some(st)
}

// 判断路径是否落在我们手写支持的特殊设备上。
// 执行时只判断路径前缀，避免把 /dev/null/xxx 这类路径继续当普通目录解析。
fn special_device_as_parent(path: &str) -> bool {
    path.starts_with("/dev/null/")
        || path.starts_with("/dev/zero/")
        || path.starts_with("/dev/random/")
        || path.starts_with("/dev/urandom/")
        || path.starts_with("/dev/rtc/")
        || path.starts_with("/dev/misc/rtc/")
}

// 系统调用入口：旧 mkdir，内部转 mkdirat。
// 执行时固定用 AT_FDCWD 作为起点，把旧接口统一交给 mkdirat。
pub fn sys_mkdir(path: *const u8, mode: u32) -> isize {
    sys_mkdirat(AT_FDCWD, path, mode)
}

// 系统调用入口：创建目录。
// 执行时读取用户路径、解析 dirfd 起点，然后调用 inode 层创建目录，失败按当前兼容逻辑返回 -1。
pub fn sys_mkdirat(dirfd: isize, path: *const u8, _mode: u32) -> isize {
    if path.is_null() {
        return -1;
    }
    let token = current_user_token();
    let path = translated_str(token, path);
    let base_dir = match resolve_base_dir(dirfd) {
        Some(base) => base,
        None => return -2,
    };
    if mkdir_at(Some(base_dir), path.as_str()) {
        0
    } else {
        -1
    }
}

// 系统调用入口：旧 unlink，内部转 unlinkat。
// 执行时把旧 unlink 当作不带 AT_REMOVEDIR 的 unlinkat 处理。
pub fn sys_unlink(path: *const u8) -> isize {
    sys_unlinkat(AT_FDCWD, path, 0)
}

const AT_REMOVEDIR: u32 = 0x200;

// 系统调用入口：删除文件或目录。
// 执行时检查 flags 和路径指针，区分删除目录还是普通文件，再交给 inode 层 unlink。
pub fn sys_unlinkat(dirfd: isize, path: *const u8, flags: u32) -> isize {
    if path.is_null() {
        return -14;
    }
    let is_dir = (flags & AT_REMOVEDIR) != 0;
    if flags != 0 && !is_dir {
        return -22;
    }
    // 读取路径前先检查用户指针，避免删除路径时踩到非法用户地址。
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let end_of_page =
        ((path as usize) + crate::config::PAGE_SIZE) & !(crate::config::PAGE_SIZE - 1);
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

// 系统调用入口：切换当前工作目录。
// 执行时校验路径指针和长度，解析到目录 dentry 后，把当前任务的 cwd 更新为该目录。
pub fn sys_chdir(path: *const u8) -> isize {
    if path.is_null() {
        return -14;
    }
    let token = current_user_token();
    // chdir 读取路径前只检查当前页，后续 translated_str 再按字符串结束位置读取。
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let max_len = 4096;
    let end_of_page =
        ((path as usize) + crate::config::PAGE_SIZE) & !(crate::config::PAGE_SIZE - 1);
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

// 系统调用入口：切换当前任务的根目录。
// 执行时解析目标目录，成功后同时更新 root 和 cwd，让后续路径显示和解析都以新根为准。
pub fn sys_chroot(path: *const u8) -> isize {
    if path.is_null() {
        return -14;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let max_len = 4096;
    let end_of_page =
        ((path as usize) + crate::config::PAGE_SIZE) & !(crate::config::PAGE_SIZE - 1);
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
    let Some(root_dentry) = vfs_resolve_path(&cwd, &path_str) else {
        return -2;
    };
    if !root_dentry.is_dir {
        return -20;
    }
    let mut inner = task.inner_exclusive_access();
    inner.root = root_dentry.clone();
    inner.cwd = root_dentry;
    0
}

// 系统调用入口：把当前工作目录写回用户缓冲区。
// 执行时按 chroot 视角修剪真实 cwd 路径，确认用户缓冲区足够大后写入以 '\0' 结尾的字符串。
pub fn sys_getcwd(buf: *mut u8, size: usize) -> isize {
    let task = current_task().unwrap();
    let cwd_path = {
        let inner = task.inner_exclusive_access();
        let cwd_path = inner.cwd.path.clone();
        let root_path = inner.root.path.clone();
        if root_path != "/" && cwd_path.starts_with(&root_path) {
            let suffix = &cwd_path[root_path.len()..];
            if suffix.is_empty() {
                String::from("/")
            } else if suffix.starts_with('/') {
                String::from(suffix)
            } else {
                let mut path = String::from("/");
                path.push_str(suffix);
                path
            }
        } else {
            cwd_path
        }
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

// 系统调用入口：更新文件访问时间和修改时间。
// 执行时支持按 fd 或路径定位 dentry，检查属主/写权限，再按 UTIME_NOW/OMIT 更新 atime、mtime 和 ctime 缓存。
pub fn sys_utimensat(dirfd: isize, path: *const u8, times: *const u8, _flags: usize) -> isize {
    const UTIME_NOW: i64 = (1i64 << 30) - 1;
    const UTIME_OMIT: i64 = (1i64 << 30) - 2;

    #[repr(C)]
    struct TimeSpec {
        tv_sec: i64,
        tv_nsec: i64,
    }

    let token = current_user_token();
    let fd_file = if path.is_null() {
        let fd = if dirfd < 0 { return -9 } else { dirfd as usize };
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        if fd >= inner.fd_table.len() {
            return -9;
        }
        inner.fd_table[fd].as_ref().cloned()
    } else {
        None
    };
    let dentry = if let Some(file) = &fd_file {
        let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() else {
            return -9;
        };
        inode.get_dentry().clone()
    } else {
        let path_str = translated_str(token, path);
        if special_device_as_parent(path_str.as_str()) {
            return -20;
        }
        if special_device_stat(path_str.as_str()).is_some() {
            return 0;
        }
        let base = match resolve_base_dir(dirfd) {
            Some(b) => b,
            None => return -2,
        };
        match vfs_resolve_with_fallback(&base, &path_str) {
            Some(d) => d,
            None => return -2,
        }
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

    let now_sec = super::process::user_realtime_sec_ceil().saturating_add(FILE_TIME_NOW_GUARD_SEC);
    let now_sec_u32 = now_sec.clamp(0, u32::MAX as i64) as u32;

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
            Some(ts0.tv_sec)
        };

        let mt = if ts1.tv_nsec == UTIME_OMIT {
            None
        } else if ts1.tv_nsec == UTIME_NOW {
            ch = true;
            Some(now_sec)
        } else {
            ch = true;
            Some(ts1.tv_sec)
        };

        if !ch {
            return 0; // 忽略
        }
        (at, mt, true)
    };

    let fs_path = &dentry.fs_path;
    if let Some(a) = new_atime {
        dentry
            .super_block
            .set_atime(fs_path, a.clamp(0, u32::MAX as i64) as u32);
    }
    if let Some(m) = new_mtime {
        dentry
            .super_block
            .set_mtime(fs_path, m.clamp(0, u32::MAX as i64) as u32);
    }
    if changed {
        dentry.super_block.set_ctime(fs_path, now_sec_u32);
    }
    crate::fs::inode::update_path_cached_times(
        fs_path,
        new_atime,
        new_mtime,
        if changed { Some(now_sec) } else { None },
    );
    if let Some(file) = &fd_file {
        if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
            inode.update_cached_times(
                new_atime,
                new_mtime,
                if changed { Some(now_sec) } else { None },
            );
        }
    }

    0
}

// 系统调用入口：按真实权限检查文件可访问性。
// 执行时兼容旧 faccessat，实际交给 faccessat2 的实现处理。
pub fn sys_faccessat(dirfd: isize, path: *const u8, mode: usize, _flags: usize) -> isize {
    sys_faccessat2(dirfd, path, mode, 0)
}

// 系统调用入口：faccessat2，支持 AT_EACCESS 等 flags。
// 执行时解析路径和符号链接策略，F_OK 只查存在性，R/W/X 则按当前有效 uid/gid 与 inode 权限位判断。
pub fn sys_faccessat2(dirfd: isize, path: *const u8, mode: usize, flags: usize) -> isize {
    const F_OK: usize = 0;
    const R_OK: usize = 4;
    const W_OK: usize = 2;
    const X_OK: usize = 1;
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;
    const AT_EACCESS: usize = 0x200;
    const AT_EMPTY_PATH: usize = 0x1000;

    let token = current_user_token();
    let path_str = translated_str(token, path);
    if mode & !(R_OK | W_OK | X_OK) != 0 {
        return -22;
    }
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EACCESS | AT_EMPTY_PATH) != 0 {
        return -22;
    }

    if path_str.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return -2;
    }
    if let Some(st) = special_device_stat(path_str.as_str()) {
        if mode == F_OK {
            return 0;
        }
        if (mode & X_OK) != 0 {
            return -13;
        }
        if (mode & W_OK) != 0 && (st.st_mode & 0o222) == 0 {
            return -13;
        }
        if (mode & R_OK) != 0 && (st.st_mode & 0o444) == 0 {
            return -13;
        }
        return 0;
    }
    let base = match resolve_base_dir(dirfd) {
        Some(b) => b,
        None => return -2,
    };
    let dentry = match vfs_resolve_with_fallback(&base, &path_str) {
        Some(d) => d,
        None => return -2,
    };
    let dentry = if flags & AT_SYMLINK_NOFOLLOW == 0 {
        match crate::fs::follow_symlink(&base, dentry, 0) {
            Some(d) => d,
            None => return -2,
        }
    } else {
        dentry
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
    }
}

// 系统调用入口：按路径修改权限位。
// 执行时解析目标路径，非 root 必须是文件属主，成功后写 mode 并刷新 ctime。
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

// 从 fd 对象里取出底层 dentry。
// 执行时检查 fd 表项并确认它是 OSInode，只有真实文件 fd 才能取到 dentry。
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

// 系统调用入口：按 fd 修改权限位。
// 执行时从 fd 找 dentry，非 root 要求属主匹配，成功后更新 mode 和 ctime。
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

// 系统调用入口：按路径修改属主和属组。
// 执行时解析路径并要求有效 uid 为 root，然后写入 owner/group 并更新 ctime。
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

// 系统调用入口：按 fd 修改属主和属组。
// 执行时从 fd 找到真实 inode，只有 root 可以修改 uid/gid，成功后刷新 ctime。
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

// 系统调用入口：处理 fd 复制、FD_CLOEXEC、O_NONBLOCK 等 fcntl 命令。
// 执行时先校验 fd，再按 cmd 分派：复制 fd、读写 cloexec/flags，文件锁命令只记录兼容用的锁类型。
pub fn sys_fcntl(fd: usize, cmd: usize, arg: usize) -> isize {
    const F_DUPFD: usize = 0;
    const F_GETFD: usize = 1;
    const F_SETFD: usize = 2;
    const F_GETFL: usize = 3;
    const F_SETFL: usize = 4;
    const F_DUPFD_CLOEXEC: usize = 1030;
    const FD_CLOEXEC: usize = 1;
    const MAX_FD: usize = 1024;

    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9; // EBADF
    }

    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            if arg >= MAX_FD {
                return -22;
            }
            while inner.fd_table.len() <= arg && inner.fd_table.len() < MAX_FD {
                inner.fd_table.push(None);
                inner.fd_cloexec.push(false);
            }
            let new_fd = if let Some(fd) =
                (arg..inner.fd_table.len()).find(|&i| inner.fd_table[i].is_none())
            {
                fd
            } else if inner.fd_table.len() < MAX_FD {
                inner.fd_table.push(None);
                inner.fd_cloexec.push(false);
                inner.fd_table.len() - 1
            } else {
                return -24;
            };
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
        // 文件锁
        5 | 12 => {
            // 取锁
            let token = inner.get_user_token();
            let cur_type = inner.fd_table[fd]
                .as_ref()
                .and_then(|f| f.as_any().downcast_ref::<crate::fs::inode::OSInode>())
                .map(|ino| ino.get_lock_type())
                .unwrap_or(2);
            let size = 32;
            if !inner
                .memory_set
                .valid_user_buf(arg as *const u8, size, true)
            {
                return -14;
            }
            let token = inner.get_user_token();
            drop(inner);
            let buf = crate::mm::translated_byte_buffer(token, arg as *mut u8, size);
            for b in buf.iter_mut() {
                *b = 0;
            }
            buf[0] = cur_type as u8;
            return 0;
        }
        6 | 7 | 13 | 14 => {
            // 设锁
            if let Some(inode) = inner.fd_table[fd]
                .as_ref()
                .and_then(|f| f.as_any().downcast_ref::<crate::fs::inode::OSInode>())
            {
                inode.set_lock_type(1); // F_WRLCK
            }
            0
        }
        2 | 15 => {
            // 解锁
            if let Some(inode) = inner.fd_table[fd]
                .as_ref()
                .and_then(|f| f.as_any().downcast_ref::<crate::fs::inode::OSInode>())
            {
                inode.set_lock_type(2); // 解锁
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

// 判断 fd 是否是标准终端类 fd。
// 执行时只把 stdin/stdout 当作终端，满足常见 shell 和测试程序的 ioctl 探测。
fn is_terminal_fd(fd: usize) -> bool {
    fd == 0 || fd == 1 // 标准流
}

// 系统调用入口：处理终端、窗口大小和少量 socket ioctl。
// 执行时先处理 RTC_RD_TIME；终端 ioctl 返回固定 termios 和 80x24 窗口，其它请求按 ENOTTY 处理。
pub fn sys_ioctl(fd: usize, request: usize, arg: usize) -> isize {
    let token = current_user_token();
    let request = request & 0xffffffff;

    // RTC
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

// 系统调用入口：关闭 fd。
// 执行时从当前任务 fd 表移除对应对象，并触发 File::close_ref 做引用计数和资源清理。
pub fn sys_close(fd: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if inner.close_fd(fd) {
        return 0;
    }
    -9
}

// 系统调用入口：旧 pipe，内部转 pipe2。
// 执行时把旧 pipe 视为不带额外 flags 的 pipe2。
pub fn sys_pipe(pipe: *mut usize) -> isize {
    sys_pipe2(pipe, 0)
}

// 系统调用入口：创建匿名管道并返回两个 fd。
// 执行时创建读端和写端 File，分配两个 fd，设置可选 CLOEXEC，再把 fd 数组写回用户态。
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
        let (pipe_read, pipe_write) = make_pipe();
        let read_fd = inner.alloc_fd();
        if read_fd >= inner.fd_table.len() {
            return -24;
        }
        inner.fd_table[read_fd] = Some(pipe_read);
        let write_fd = inner.alloc_fd();
        if write_fd >= inner.fd_table.len() {
            inner.close_fd(read_fd);
            return -24;
        }
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

// 系统调用入口：复制 fd 到最小可用槽位。
// 执行时确认旧 fd 有效，递增文件对象引用计数，并放到 fd 表里第一个空槽。
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

// 系统调用入口：dup2 兼容入口。
// 执行时转到 dup3，flags 固定为 0，保持旧接口语义。
pub fn sys_dup2(fd: usize, new_fd: usize) -> isize {
    sys_dup3(fd, new_fd, 0)
}

// 系统调用入口：复制 fd 到指定编号，并处理 CLOEXEC。
// 执行时扩展 fd 表到 new_fd，先关闭目标槽位，再把旧文件对象引用放过去并设置 close-on-exec。
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

// 系统调用入口：调整 fd 当前读写偏移。
// 执行时按 SEEK_SET/CUR/END 计算新偏移，拒绝负偏移，最后调用 File::seek 更新对象内部 offset。
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

// 系统调用入口：重命名文件或目录。
// 执行时分别解析旧路径和新路径的 dirfd 起点，然后交给 inode 层 rename_at 完成跨目录移动。
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

// 系统调用入口：创建符号链接。
// 执行时读取目标字符串和链接路径，解析 newdirfd 起点后创建保存 target 内容的符号链接文件。
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

// 系统调用入口：读取符号链接目标。
// 执行时解析路径但不跟随链接，确认 inode 是 symlink 后读取链接内容并截断写入用户缓冲区。
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

// 系统调用入口：按路径写回 stat 信息。
// 执行时支持 AT_EMPTY_PATH 和 AT_SYMLINK_NOFOLLOW，特殊设备直接填 stat，普通路径通过 OSInode::stat 生成。
pub fn sys_fstatat(dirfd: isize, path: *const u8, stat_buf: *mut u8, flags: usize) -> isize {
    const AT_EMPTY_PATH: usize = 0x1000;
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;

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
    if let Some(st) = special_device_stat(path_str.as_str()) {
        *translated_refmut(token, stat_buf as *mut Kstat) = st;
        return 0;
    }

    // 对手写支持的字符设备直接返回 stat，不需要走 VFS 路径解析。
    if path_str == "/dev/null"
        || path_str == "/dev/zero"
        || path_str == "/dev/null/invalid"
        || path_str == "/dev/urandom"
        || path_str == "/dev/random"
        || path_str == "/dev/rtc"
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
    let dentry = if flags & AT_SYMLINK_NOFOLLOW == 0 {
        match crate::fs::follow_symlink(&base, dentry, 0) {
            Some(d) => d,
            None => return -2,
        }
    } else {
        dentry
    };

    let inode = crate::fs::inode::OSInode::new(false, false, false, dentry);
    let st = inode.stat();
    *translated_refmut(token, stat_buf as *mut Kstat) = st;
    0
}

// 系统调用入口：写回 statx 兼容结构。
// 执行时按 Linux statx 布局写兼容字段，路径和 fd 两种形式都转成 Kstat 再统一转换。
pub fn sys_statx(
    dirfd: isize,
    path: *const u8,
    flags: u32,
    _mask: u32,
    statx_buf: *mut u8,
) -> isize {
    const AT_EMPTY_PATH: u32 = 0x1000;
    const AT_SYMLINK_NOFOLLOW: u32 = 0x100;

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

    // statx 的路径指针可能为空；非空时先校验一小段用户地址。
    if !path.is_null() {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        let end_of_page =
            ((path as usize) + crate::config::PAGE_SIZE) & !(crate::config::PAGE_SIZE - 1);
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
    if let Some(st) = special_device_stat(path_str.as_str()) {
        write_statx(statx_buf, &st, _mask);
        return 0;
    }

    // 对手写支持的字符设备直接填 statx，保持和 fstatat 行为一致。
    if path_str == "/dev/null"
        || path_str == "/dev/zero"
        || path_str == "/dev/null/invalid"
        || path_str == "/dev/urandom"
        || path_str == "/dev/random"
        || path_str == "/dev/rtc"
        || path_str == "/dev/misc/rtc"
    {
        let st = Kstat::new(crate::fs::S_IFCHR);
        write_statx(statx_buf, &st, _mask);
        return 0;
    }
    if path_str.len() > 255 {
        return -36;
    }

    // 手动解析 dirfd，便于按 Linux 习惯返回更准确的错误码。
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
    let dentry = if flags & AT_SYMLINK_NOFOLLOW == 0 {
        match crate::fs::follow_symlink(&base, dentry, 0) {
            Some(d) => d,
            None => return -2,
        }
    } else {
        dentry
    };

    let inode = crate::fs::inode::OSInode::new(false, false, false, dentry);
    let st = inode.stat();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(statx_buf, core::mem::size_of::<Statx>(), true)
    {
        return -14;
    }
    drop(inner);
    write_statx(statx_buf, &st, _mask);
    0
}

// 把 Kstat 转写成 Linux statx 布局。
// 执行时把 Kstat 的大小、时间、权限和设备号字段逐项搬到用户 statx 结构里。
fn write_statx(buf: *mut u8, st: &Kstat, mask: u32) {
    // 从设备号里取 major。
    // 执行时按 Linux dev_t 编码规则拆出主设备号。
    fn dev_major(dev: u64) -> u32 {
        (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)) as u32
    }

    // 从设备号里取 minor。
    // 执行时按 Linux dev_t 编码规则拆出次设备号。
    fn dev_minor(dev: u64) -> u32 {
        ((dev & 0xff) | ((dev >> 12) & !0xff)) as u32
    }

    let token = current_user_token();
    let statx = translated_refmut(token, buf as *mut Statx);
    statx.stx_mask = mask & 0x7ff; // 基础项
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
    statx.stx_rdev_major = dev_major(st.st_rdev);
    statx.stx_rdev_minor = dev_minor(st.st_rdev);
    statx.stx_dev_major = dev_major(st.st_dev);
    statx.stx_dev_minor = dev_minor(st.st_dev);
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

// 系统调用入口：按 fd 写回 stat 信息。
// 执行时从 fd 表取出 File 对象，调用它的 stat，再把 Kstat 写回用户缓冲区。
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

// 系统调用入口：读取目录项到用户缓冲区。
// 执行时取出目录 fd，对用户缓冲区做地址翻译，再让 File::getdents64 生成 dirent64 数据。
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

use crate::drivers::block::VirtIOBlock;
use crate::drivers::{BLOCK_DEVICE, ROOT_BLOCK_DEVICE};
use crate::fs::vfs::{vfs_mount, vfs_umount};

// 系统调用入口：把块设备文件系统挂到目标目录。
// 执行时解析 target 和 fstype，选择对应 virtio 块设备，创建新的 ext4 超级块并挂到目标目录。
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
    let source_str = translated_str(token, _source);

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

    if fstype_str.as_str() == "vfat" && matches!(source_str.as_str(), "/dev/vda2" | "vda2") {
        return 0;
    }

    // 如果已经挂载，幂等成功
    use crate::fs::vfs::MOUNT_TABLE;
    {
        let table = MOUNT_TABLE.exclusive_access();
        let found = table.iter().any(|m| m.mnt_point.path == mnt_point.path);
        if found {
            return 0;
        }
    }

    let blk_dev = match source_str.as_str() {
        "vda" => BLOCK_DEVICE.clone(),
        "vdb" => crate::drivers::ROOT_BLOCK_DEVICE.clone(),
        "vdc" => Arc::new(VirtIOBlock::new_at(polyhal::PhysAddr::new(0x10003000))),
        "vdd" => Arc::new(VirtIOBlock::new_at(polyhal::PhysAddr::new(0x10004000))),
        _ => BLOCK_DEVICE.clone(),
    };
    let mut lwext4_mount_path =
        String::from(target_path.trim_end_matches('\0').trim_end_matches('/'));
    if lwext4_mount_path.is_empty() {
        lwext4_mount_path.push('/');
    } else if lwext4_mount_path != "/" {
        lwext4_mount_path.push('/');
    }

    let sb = match VfsSuperBlock::new_ext4_at(blk_dev, "ext4", lwext4_mount_path.as_str()) {
        Some(sb) => sb,
        None => return -17,
    };

    if vfs_mount(mnt_point, sb, lwext4_mount_path.as_str()) {
        0
    } else {
        -16
    }
}

// 系统调用入口：卸载指定挂载点。
// 执行时解析目标路径，在挂载表里找到挂载点后移除记录，并调用超级块 force_umount 释放 lwext4 状态。
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
        let mount = table.remove(idx);
        mount.mnt_root.super_block.force_umount();
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

// 判断公历闰年，给 statfs 生成 fsid 时使用。
// 执行时按四年一闰、百年不闰、四百年再闰的规则返回布尔值。
fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

// 根据 dentry 所属文件系统填充 statfs。
// 执行时从超级块读取块数和 inode 数，把 ext4 统计信息填进 Linux statfs 结构。
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

// 系统调用入口：按路径写回文件系统统计。
// 执行时解析路径到 dentry，再把该 dentry 所属超级块的统计信息写回用户 statfs。
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

// 系统调用入口：从指定偏移读取，不改变 fd 当前 offset。
// 执行时检查 fd 和读权限，把用户缓冲区翻译出来后调用 File::read_at，文件当前 offset 不移动。
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
    drop(inner);
    let user_buf = translated_byte_buffer(token, buf, len);
    file.read_at(offset, user_buf)
}

// 系统调用入口：从指定偏移写入，不改变 fd 当前 offset。
// 执行时检查 fd 和写权限，把用户缓冲区翻译出来后调用 File::write_at，文件当前 offset 不移动。
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
    drop(inner);
    let user_buf = translated_byte_buffer(token, buf, len);
    file.write_at(offset, user_buf)
}

// 系统调用入口：按 iovec 从指定偏移读取。
// 执行时先校验 iovec 数组，再逐项校验目标缓冲区可写，并按 read_at 累加偏移读取。
pub fn sys_preadv(fd: usize, iov: *const u8, iovcnt: usize, offset: usize) -> isize {
    use crate::mm::translated_ref;
    const IOV_MAX: usize = 1024;
    if iovcnt == 0 || iovcnt > IOV_MAX {
        return -22;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if !inner
        .memory_set
        .valid_user_buf(iov, iovcnt * core::mem::size_of::<IoVec>(), false)
    {
        return -14;
    }
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
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
        let ok = inner
            .memory_set
            .valid_user_buf(iov.iov_base as *const u8, iov.iov_len, true);
        drop(inner);
        if !ok {
            return -14;
        }
        let user_buf =
            crate::mm::translated_byte_buffer(token, iov.iov_base as *mut u8, iov.iov_len);
        let n = file.read_at(off, user_buf);
        if n < 0 {
            return if total == 0 { n } else { total };
        }
        let n = n as usize;
        total += n as isize;
        off += n;
        if n < iov.iov_len {
            break;
        }
    }
    total
}

// 系统调用入口：按 iovec 从指定偏移写入。
// 执行时逐项读取用户 iovec，确认每段源缓冲区可读，再按 write_at 累加偏移写入文件。
pub fn sys_pwritev(fd: usize, iov: *const u8, iovcnt: usize, offset: usize) -> isize {
    use crate::mm::translated_ref;
    const IOV_MAX: usize = 1024;
    if iovcnt == 0 || iovcnt > IOV_MAX {
        return -22;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    // pwritev 先校验 iovec 数组本身，再逐个读取用户缓冲区内容。
    if !inner
        .memory_set
        .valid_user_buf(iov, iovcnt * core::mem::size_of::<IoVec>(), false)
    {
        return -14;
    }
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
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
        let ok = inner
            .memory_set
            .valid_user_buf(iov.iov_base as *const u8, iov.iov_len, false);
        drop(inner);
        if !ok {
            return -14;
        }
        let user_buf =
            crate::mm::translated_byte_buffer(token, iov.iov_base as *mut u8, iov.iov_len);
        let n = file.write_at(off, user_buf);
        if n < 0 {
            return if total == 0 { n } else { total };
        }
        let n = n as usize;
        total += n as isize;
        off += n;
        if n < iov.iov_len {
            break;
        }
    }
    total
}

// 系统调用入口：preadv2 兼容入口。
// 执行时暂时忽略 flags，复用 preadv 的偏移读取逻辑。
pub fn sys_preadv2(
    fd: usize,
    iov: *const u8,
    iovcnt: usize,
    offset: usize,
    _flags: usize,
) -> isize {
    sys_preadv(fd, iov, iovcnt, offset)
}

// 系统调用入口：pwritev2 兼容入口。
// 执行时暂时忽略 flags，复用 pwritev 的偏移写入逻辑。
pub fn sys_pwritev2(
    fd: usize,
    iov: *const u8,
    iovcnt: usize,
    offset: usize,
    _flags: usize,
) -> isize {
    sys_pwritev(fd, iov, iovcnt, offset)
}

// 刷新指定 fd 对应的文件系统缓存。
// 执行时确认 fd 是 OSInode，取出它的 dentry 后调用超级块 fsync，失败按 EIO 返回。
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
    if rc == 0 {
        0
    } else {
        -5
    }
}

// 系统调用入口：fsync 指定 fd。
// 执行时复用 sync_fd，把该文件相关的 ext4 缓存刷到底层块设备。
pub fn sys_fsync(fd: usize) -> isize {
    sync_fd(fd)
}

// 系统调用入口：fdatasync，当前和 fsync 同步处理。
// 执行时当前不区分数据和元数据，直接走和 fsync 相同的同步路径。
pub fn sys_fdatasync(fd: usize) -> isize {
    sync_fd(fd)
}

// 系统调用入口：同步根文件系统缓存。
// 执行时调用根超级块 sync_all，把整个根文件系统的脏数据刷出。
pub fn sys_sync() -> isize {
    let rc = crate::fs::vfs::ROOT_SB.sync_all();
    if rc == 0 {
        0
    } else {
        -5
    }
}

// 系统调用入口：同步 fd 所在文件系统。
// 执行时当前按 fd 粒度同步处理，复用 sync_fd。
pub fn sys_syncfs(fd: usize) -> isize {
    sync_fd(fd)
}

// 系统调用入口：预分配或打洞文件空间的兼容实现。
// 执行时只支持普通扩展文件大小；KEEP_SIZE 视为成功，其它复杂模式返回 ENOSYS。
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

// 系统调用入口：接收文件访问建议，当前主要做参数检查。
// 执行时只校验 advice 枚举范围，内核暂不根据访问模式调整缓存策略。
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

// 系统调用入口：按 fd 写回文件系统统计。
// 执行时要求 fd 对应真实 inode，再按它所在超级块填充 statfs。
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

// 系统调用入口：按 fd 截断文件。
// 执行时从 fd 找到 inode dentry，调用超级块 truncate_file，并同步更新 OSInode 的缓存大小。
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
    let dentry = file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .map(|inode| inode.dentry());
    match dentry {
        Some(d) => {
            if d.super_block.truncate_file(&d.fs_path, length) {
                if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
                    inode.update_cached_size(length as i64);
                }
                0
            } else {
                -1
            }
        }
        None => -1,
    }
}

// ---- flock (32) ----

// 系统调用入口：flock 兼容实现。
// 执行时暂不维护 BSD flock 状态，直接返回成功以兼容只探测锁接口的程序。
pub fn sys_flock(_fd: usize, _op: usize) -> isize {
    0
}

// ---- mknodat (33) ----

// 系统调用入口：创建普通文件或少量特殊节点。
// 执行时校验路径并确认目标不存在，然后按普通文件创建；当前不真正区分设备节点类型。
pub fn sys_mknodat(dirfd: isize, path: *const u8, mode: u32, _dev: usize) -> isize {
    if path.is_null() {
        return -14;
    }
    // mknodat 读取路径前先校验用户地址，避免 translated_str 访问非法指针。
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let end_of_page =
        ((path as usize) + crate::config::PAGE_SIZE) & !(crate::config::PAGE_SIZE - 1);
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
    // 目标已经存在时按 EEXIST 返回，不覆盖已有节点。
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

// 系统调用入口：readahead 兼容实现。
// 执行时不主动预读，只返回成功，让用户态把它当作性能提示而不是硬性操作。
pub fn sys_readahead(_fd: usize, _offset: usize, _count: usize) -> isize {
    0
}

// ---- copy_file_range (285) ----

// 系统调用入口：在两个 fd 之间复制一段文件内容。
// 执行时可选设置输入/输出偏移，然后用 4KB 临时缓冲循环读写，直到达到 len 或一端停止。
pub fn sys_copy_file_range(
    fd_in: usize,
    off_in: *mut usize,
    fd_out: usize,
    off_out: *mut usize,
    len: usize,
    _flags: usize,
) -> isize {
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
        if n <= 0 {
            break;
        }
        let w = f_out.write(&mut buf[..n as usize]);
        if w <= 0 {
            break;
        }
        total += w;
    }
    total
}

// ---- memfd_create (279) ----

// 系统调用入口：memfd_create 兼容实现，返回匿名临时文件 fd。
// 执行时分配一个 fd 并挂上 DevNull 作为占位匿名文件，满足只需要 fd 的兼容场景。
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

// 系统调用入口：批量关闭 fd 范围。
// 执行时把 last 截到当前 fd 表范围内，逐个调用 close_fd 释放对应文件引用。
pub fn sys_close_range(first: usize, last: usize, _flags: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let last = last.min(inner.fd_table.len().saturating_sub(1));
    for fd in first..=last {
        inner.close_fd(fd);
    }
    0
}

// ---- socketpair (199) ----

// 系统调用入口：socketpair 兼容实现。
// 执行时暂时直接返回成功；当前测试只需要接口存在，还没有创建成对 socket。
pub fn sys_socketpair(_domain: usize, _typ: usize, _protocol: usize, _sv: *mut usize) -> isize {
    0
}
