use crate::fs::vfs::{
    vfs_clear_tmpfs_mount, vfs_create, vfs_dentry_is_readonly, vfs_lookup, vfs_resolve_path,
    vfs_resolve_with_fallback, vfs_set_readonly_mount, vfs_set_tmpfs_mount, vfs_tmpfs_can_resize,
    vfs_tmpfs_commit_resize, Dentry, VfsSuperBlock,
};
use crate::fs::{
    follow_symlink, link_at, make_pipe, open_file_at, rename_at, symlink_at, unlink_at, DevBlock,
    DevNull, DevZero, File, Kstat, OpenFlags, Urandom, S_IFDIR, S_IFREG,
};
use crate::mm::{translated_byte_buffer, translated_refmut, translated_str};
use crate::task::{current_task, current_user_token, PID2TCB};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

const AT_FDCWD: isize = -100;
const O_NOATIME: u32 = 0x40000;
const O_CLOEXEC: u32 = 0x80000;
const O_TMPFILE: u32 = 0x410000;
const FILE_TIME_NOW_GUARD_SEC: i64 = 5;
const PATH_MAX: usize = 4096;
const NAME_MAX: usize = 255;
const EROFS: isize = -30;

fn map_dev_shm_path(path: &str) -> String {
    if let Some(name) = path.strip_prefix("/dev/shm/") {
        format!("/tmp/.shm_{}", name)
    } else {
        String::from(path)
    }
}

fn valid_user_write_buf(ptr: *mut u8, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if ptr.is_null() {
        return false;
    }
    let Some(task) = current_task() else {
        return false;
    };
    let inner = task.inner_exclusive_access();
    inner.memory_set.valid_user_buf(ptr as *const u8, len, true)
}

fn valid_user_read_buf(ptr: *const u8, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if ptr.is_null() {
        return false;
    }
    let Some(task) = current_task() else {
        return false;
    };
    let inner = task.inner_exclusive_access();
    inner.memory_set.valid_user_buf(ptr, len, false)
}

fn fd_path_for_proc(fd: usize) -> Option<String> {
    let task = current_task()?;
    let inner = task.inner_exclusive_access();
    let file = inner.fd_table.get(fd)?.as_ref()?;
    let raw_path = file.path()?;
    let root_path = inner.root.path.clone();
    if root_path != "/" && raw_path.starts_with(&root_path) {
        let suffix = &raw_path[root_path.len()..];
        if suffix.is_empty() {
            Some(String::from("/"))
        } else if suffix.starts_with('/') {
            Some(String::from(suffix))
        } else {
            Some(format!("/{}", suffix))
        }
    } else {
        Some(raw_path)
    }
}

fn proc_fd_readlink(path: &str) -> Option<String> {
    let rest = if let Some(rest) = path.strip_prefix("/proc/self/fd/") {
        rest
    } else if let Some(rest) = path.strip_prefix("/proc/") {
        let (pid, tail) = rest.split_once('/')?;
        if tail.strip_prefix("fd/").is_none() {
            return None;
        }
        let current_pid = current_task().map(|task| task.getpid())?;
        if pid.parse::<usize>().ok()? != current_pid {
            return None;
        }
        tail.strip_prefix("fd/")?
    } else {
        return None;
    };
    let fd = rest.parse::<usize>().ok()?;
    fd_path_for_proc(fd)
}

fn install_fd_for_thread_group(
    owner_pid: usize,
    tgid: usize,
    fd: usize,
    file: Arc<dyn File + Send + Sync>,
    cloexec: bool,
) {
    let tasks: Vec<_> = PID2TCB
        .exclusive_access()
        .values()
        .filter(|task| task.getpid() != owner_pid)
        .filter(|task| {
            task.try_inner_borrow()
                .map(|inner| inner.tgid == tgid)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    for task in tasks {
        if let Some(mut inner) = task.try_inner_exclusive_access() {
            while inner.fd_table.len() <= fd {
                inner.fd_table.push(None);
                inner.fd_cloexec.push(false);
            }
            file.dup_ref();
            inner.fd_table[fd] = Some(file.clone());
            inner.fd_cloexec[fd] = cloexec;
        }
    }
}

fn close_fd_for_thread_group(owner_pid: usize, tgid: usize, fd: usize) {
    let tasks: Vec<_> = PID2TCB
        .exclusive_access()
        .values()
        .filter(|task| task.getpid() != owner_pid)
        .filter(|task| {
            task.try_inner_borrow()
                .map(|inner| inner.tgid == tgid)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    for task in tasks {
        if let Some(mut inner) = task.try_inner_exclusive_access() {
            inner.close_fd(fd);
        }
    }
}

fn install_fd(
    task: &Arc<crate::task::TaskControlBlock>,
    file: Arc<dyn File + Send + Sync>,
    cloexec: bool,
) -> Result<usize, isize> {
    let mut inner = task.inner_exclusive_access();
    let fd = inner.alloc_fd();
    if fd >= inner.fd_table.len() {
        return Err(-24);
    }
    inner.fd_table[fd] = Some(file);
    inner.fd_cloexec[fd] = cloexec;
    Ok(fd)
}

fn install_fd_shared(
    task: &Arc<crate::task::TaskControlBlock>,
    file: Arc<dyn File + Send + Sync>,
    cloexec: bool,
) -> Result<usize, isize> {
    let (fd, tgid) = {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        if fd >= inner.fd_table.len() {
            return Err(-24);
        }
        inner.fd_table[fd] = Some(file.clone());
        inner.fd_cloexec[fd] = cloexec;
        (fd, inner.tgid)
    };
    install_fd_for_thread_group(task.getpid(), tgid, fd, file, cloexec);
    Ok(fd)
}

fn translated_path_bounded(path: *const u8) -> Result<String, isize> {
    if path.is_null() {
        return Err(-14);
    }

    let task = current_task().unwrap();
    let mut bytes = Vec::new();

    while bytes.len() < PATH_MAX {
        let offset = bytes.len();
        let addr = unsafe { path.add(offset) };
        let page_offset = (addr as usize) & (crate::config::PAGE_SIZE - 1);
        let page_left = crate::config::PAGE_SIZE - page_offset;
        let check_len = page_left.min(PATH_MAX - offset);

        {
            let inner = task.inner_exclusive_access();
            if !inner.memory_set.valid_user_buf(addr, check_len, false) {
                return Err(-14);
            }
        }

        let chunk = unsafe { core::slice::from_raw_parts(addr, check_len) };
        if let Some(nul_pos) = chunk.iter().position(|&b| b == 0) {
            bytes.extend_from_slice(&chunk[..nul_pos]);
            let s = unsafe { core::str::from_utf8_unchecked(&bytes) };
            return Ok(String::from(s));
        }
        bytes.extend_from_slice(chunk);
    }

    Err(-36)
}

fn path_has_too_long_component(path: &str) -> bool {
    path.split('/')
        .any(|component| component.as_bytes().len() > NAME_MAX)
}

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

fn resolve_base_dir_for_path(dirfd: isize, path: &str) -> Result<Arc<Dentry>, isize> {
    if path.starts_with('/') {
        return Ok(current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone()));
    }
    let Some(task) = current_task() else {
        return Err(-9);
    };
    if dirfd == AT_FDCWD {
        return Ok(task.inner_exclusive_access().cwd.clone());
    }
    if dirfd < 0 {
        return Err(-9);
    }
    let inner = task.inner_exclusive_access();
    let fd = dirfd as usize;
    if fd >= inner.fd_table.len() {
        return Err(-9);
    }
    let Some(file) = &inner.fd_table[fd] else {
        return Err(-9);
    };
    if !file.is_dir() {
        return Err(-20);
    }
    if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        let dentry = inode.get_dentry().clone();
        if dentry_still_exists(&dentry) {
            Ok(dentry)
        } else {
            Err(-2)
        }
    } else {
        Err(-20)
    }
}

fn dentry_still_exists(dentry: &Arc<Dentry>) -> bool {
    if dentry.fs_path == "/" {
        return true;
    }
    dentry
        .super_block
        .get_inode_info(&dentry.fs_path)
        .map(|info| info.inode_num == dentry.inode_num)
        .unwrap_or(false)
}

fn resolve_search_dir(current: Arc<Dentry>) -> Result<Arc<Dentry>, isize> {
    if current.is_dir {
        return Ok(current);
    }
    if current.is_link {
        let resolved = follow_symlink(&current, current.clone(), 0).ok_or(-40isize)?;
        if resolved.is_dir {
            return Ok(resolved);
        }
    }
    Err(-20)
}

fn resolve_parent_and_name_checked(
    start: &Arc<Dentry>,
    path: &str,
) -> Result<(Arc<Dentry>, String), isize> {
    let path = path.trim_matches('\0');
    if path.is_empty() {
        return Err(-2);
    }

    let mut current = if path.starts_with('/') {
        current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone())
    } else {
        start.clone()
    };

    let components: Vec<&str> = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect();
    if components.is_empty() {
        return Err(-2);
    }

    for component in components.iter().take(components.len() - 1) {
        current = resolve_search_dir(current)?;
        let next = vfs_lookup(&current, component).ok_or(-2isize)?;
        current = if next.is_link {
            follow_symlink(&current, next, 0).ok_or(-40isize)?
        } else {
            next
        };
    }

    current = resolve_search_dir(current)?;

    Ok((current, String::from(*components.last().unwrap())))
}

fn resolve_path_checked(start: &Arc<Dentry>, path: &str) -> Result<Arc<Dentry>, isize> {
    let path = path.trim_matches('\0');
    if path.is_empty() {
        return Err(-2);
    }

    let mut current = if path.starts_with('/') {
        current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone())
    } else {
        start.clone()
    };

    let components: Vec<&str> = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect();

    for (idx, component) in components.iter().enumerate() {
        current = resolve_search_dir(current)?;
        let next = vfs_lookup(&current, component).ok_or(-2isize)?;
        current = if idx + 1 < components.len() && next.is_link {
            follow_symlink(&current, next, 0).ok_or(-40isize)?
        } else {
            next
        };
    }

    Ok(current)
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
        if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
            if vfs_dentry_is_readonly(inode.get_dentry()) {
                return EROFS;
            }
        }
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
        if !inner.memory_set.valid_user_buf(buf, len, true) {
            return -14;
        }
        drop(inner);

        file.read(translated_byte_buffer(token, buf, len))
    } else {
        -9
    }
}

pub fn sys_open(path: *const u8, flags: u32) -> isize {
    sys_openat(AT_FDCWD, path, flags, 0)
}

fn open_existing_access(dentry: &Arc<Dentry>, flags: OpenFlags) -> isize {
    let (readable, writable) = flags.read_write();
    let need = ((readable as u32) * 0o4) | (((writable || flags.trunc_requested()) as u32) * 0o2);
    if need == 0 {
        return 0;
    }
    if dentry.is_dir {
        return if writable || flags.trunc_requested() {
            -21
        } else {
            0
        };
    }
    let Some(task) = current_task() else {
        return -13;
    };
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if euid == 0 {
        return 0;
    }
    match dentry.inode_info() {
        Some(info) if inode_perm_allows(info.mode, info.uid, info.gid, euid, egid, need) => 0,
        Some(_) => -13,
        None => -2,
    }
}

fn is_libc_tmpfile_fallback(path: &str, open_flags: OpenFlags) -> bool {
    if !open_flags.create_requested() || !open_flags.excl_requested() {
        return false;
    }
    let (readable, writable) = open_flags.read_write();
    if !readable || !writable {
        return false;
    }
    let path = path.trim_matches('\0');
    path.starts_with("/tmp/tmpfile_") || path.starts_with("/var/tmp/tmpfile_")
}

pub fn sys_openat(dirfd: isize, path: *const u8, flags: u32, mode: u32) -> isize {
    let task = current_task().unwrap();
    let cloexec = (flags & O_CLOEXEC) != 0;
    let raw_path = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&raw_path) {
        return -36;
    }
    let path = map_dev_shm_path(&raw_path);

    // proc filesystem intercept
    if let Some(file) = open_proc(&path) {
        return install_fd(&task, file, cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|e| e);
    }

    if path == "/dev/urandom" || path == "/dev/random" {
        return install_fd(&task, Arc::new(Urandom), cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|e| e);
    }

    if path == "/dev/null" {
        return install_fd(&task, Arc::new(DevNull), cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|e| e);
    }

    if path == "/dev/zero" {
        let (readable, writable) = OpenFlags::from_raw(flags).read_write();
        return install_fd(&task, Arc::new(DevZero::new(readable, writable)), cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|e| e);
    }

    if let Some(index) = block_device_index(path.as_str()) {
        return install_fd(&task, Arc::new(DevBlock::new(index)), cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|e| e);
    }

    if flags & O_TMPFILE == O_TMPFILE {
        return sys_open_tmpfile(dirfd, path.as_str(), mode, cloexec);
    }

    let open_flags = OpenFlags::from_raw(flags);
    let base = match resolve_base_dir_for_path(dirfd, path.as_str()) {
        Ok(base) => base,
        Err(errno) => return errno,
    };
    if path.is_empty() {
        return -2;
    }
    let existing = vfs_resolve_with_fallback(&base, path.as_str());
    if let Some(existing) = existing {
        if existing.is_link && open_flags.nofollow_requested() {
            if open_flags.path_requested() {
                let inode = Arc::new(crate::fs::inode::OSInode::new(
                    false, false, false, existing,
                ));
                return install_fd(&task, inode, cloexec)
                    .map(|fd| fd as isize)
                    .unwrap_or_else(|e| e);
            }
            return -40;
        }
        if open_flags.directory_requested() && !existing.is_dir {
            return -20;
        }
        if open_flags.create_requested() && open_flags.excl_requested() {
            return -17;
        }
        if open_flags.trunc_requested() && vfs_dentry_is_readonly(&existing) {
            return EROFS;
        }
        let access = open_existing_access(&existing, open_flags);
        if access != 0 {
            return access;
        }
        if flags & O_NOATIME != 0 {
            let euid = {
                let inner = task.inner_exclusive_access();
                inner.euid
            };
            let owner = existing
                .inode_info()
                .map(|info| info.uid)
                .unwrap_or(0);
            if euid != 0 && euid != owner {
                return -1;
            }
        }
    } else if !open_flags.create_requested() {
        return -2;
    } else {
        let (parent, _) = match resolve_parent_and_name_checked(&base, path.as_str()) {
            Ok(parts) => parts,
            Err(errno) => return errno,
        };
        if vfs_dentry_is_readonly(&parent) {
            return EROFS;
        }
        if !can_modify_directory(&parent) {
            return -13;
        }
    }
    let use_anon_tmpfile = is_libc_tmpfile_fallback(path.as_str(), open_flags);
    let inode = open_file_at(Some(base), path.as_str(), open_flags, mode as u32);

    if let Some(inode) = inode {
        let file: Arc<dyn File + Send + Sync> = if use_anon_tmpfile {
            Arc::new(crate::fs::TmpFile::new())
        } else {
            inode.clone()
        };
        let (fd, tgid) = {
            let mut inner = task.inner_exclusive_access();
            let fd = inner.alloc_fd();
            if fd >= inner.fd_table.len() {
                return -24;
            }
            inner.fd_table[fd] = Some(file.clone());
            inner.fd_cloexec[fd] = cloexec;
            (fd, inner.tgid)
        };
        install_fd_for_thread_group(task.getpid(), tgid, fd, file, cloexec);
        fd as isize
    } else {
        -2
    }
}

fn sys_open_tmpfile(dirfd: isize, path: &str, _mode: u32, cloexec: bool) -> isize {
    let base = match resolve_base_dir_for_path(dirfd, path) {
        Ok(base) => base,
        Err(errno) => return errno,
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

    if vfs_dentry_is_readonly(&dir) {
        return EROFS;
    }

    let task = current_task().unwrap();
    install_fd_shared(&task, Arc::new(crate::fs::TmpFile::new()), cloexec)
        .map(|fd| fd as isize)
        .unwrap_or_else(|e| e)
}

fn open_proc(path: &str) -> Option<Arc<dyn File + Send + Sync>> {
    use crate::fs::proc::{
        DevRtcFile, ProcDirFile, ProcMeminfoFile, ProcMountsFile, ProcPidStatFile,
        ProcPidStatusFile, ProcStaticFile, PseudoDirFile, SysBlockStatFile,
    };
    let path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
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
    if path == "/proc/cpuinfo" {
        return Some(Arc::new(ProcStaticFile::new(
            "processor\t: 0\nhart\t\t: 0\nisa\t\t: rv64imafdc\nmmu\t\t: sv39\n",
        )));
    }
    if path == "/proc/sys/kernel/pid_max" {
        return Some(Arc::new(ProcStaticFile::new("4194304\n")));
    }
    if path == "/proc/sys/kernel/tainted" {
        return Some(Arc::new(ProcStaticFile::new("0\n")));
    }
    if path == "/proc/self/maps" {
        return Some(Arc::new(ProcStaticFile::new(
            "10000-20000 r-xp 00000000 00:00 0 /proc/self/exe\n",
        )));
    }
    if path == "/proc/self/stat" {
        let pid = current_task().map(|task| task.getpid())?;
        return Some(Arc::new(ProcPidStatFile::new(pid)));
    }
    if path == "/proc/self/status" {
        let pid = current_task().map(|task| task.getpid())?;
        return Some(Arc::new(ProcPidStatusFile::new(pid)));
    }
    if path == "/dev/rtc" || path == "/dev/misc/rtc" {
        return Some(Arc::new(DevRtcFile));
    }
    match path {
        "/sys" => return Some(Arc::new(PseudoDirFile::sys_root())),
        "/sys/block" => return Some(Arc::new(PseudoDirFile::sys_block())),
        "/sys/block/vda" | "/sys/block/vdb" => {
            return Some(Arc::new(PseudoDirFile::sys_block_device()));
        }
        "/sys/dev" => return Some(Arc::new(PseudoDirFile::sys_dev())),
        "/sys/dev/block" => return Some(Arc::new(PseudoDirFile::sys_dev_block())),
        "/sys/dev/block/254:0" | "/sys/dev/block/254:1" => {
            return Some(Arc::new(PseudoDirFile::sys_dev_block_device()));
        }
        "/sys/block/vda/stat" | "/sys/block/vdb/stat" => {
            return Some(Arc::new(SysBlockStatFile::new()));
        }
        "/sys/block/vda/uevent" | "/sys/dev/block/254:0/uevent" => {
            return Some(Arc::new(ProcStaticFile::new(
                "MAJOR=254\nMINOR=0\nDEVNAME=vda\nDEVTYPE=disk\n",
            )));
        }
        "/sys/block/vdb/uevent" | "/sys/dev/block/254:1/uevent" => {
            return Some(Arc::new(ProcStaticFile::new(
                "MAJOR=254\nMINOR=1\nDEVNAME=vdb\nDEVTYPE=disk\n",
            )));
        }
        _ => {}
    }
    if let Some(pid_str) = path
        .strip_prefix("/proc/")
        .and_then(|s| s.strip_suffix("/stat"))
    {
        if let Ok(pid) = pid_str.parse::<usize>() {
            return Some(Arc::new(ProcPidStatFile::new(pid)));
        }
    }
    if let Some(pid_str) = path
        .strip_prefix("/proc/")
        .and_then(|s| s.strip_suffix("/status"))
    {
        if let Ok(pid) = pid_str.parse::<usize>() {
            return Some(Arc::new(ProcPidStatusFile::new(pid)));
        }
    }
    if let Some(pid_str) = path
        .strip_prefix("/proc/")
        .and_then(|s| s.strip_suffix("/maps"))
    {
        if pid_str.parse::<usize>().is_ok() {
            return Some(Arc::new(ProcStaticFile::new(
                "10000-20000 r-xp 00000000 00:00 0 /proc/self/exe\n",
            )));
        }
    }
    None
}

fn special_device_stat(path: &str) -> Option<Kstat> {
    let path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };
    match path {
        "/sys"
        | "/sys/block"
        | "/sys/block/vda"
        | "/sys/block/vdb"
        | "/sys/dev"
        | "/sys/dev/block"
        | "/sys/dev/block/254:0"
        | "/sys/dev/block/254:1" => return Some(Kstat::new(crate::fs::S_IFDIR | 0o555)),
        "/sys/block/vda/stat"
        | "/sys/block/vdb/stat"
        | "/sys/block/vda/uevent"
        | "/sys/block/vdb/uevent"
        | "/sys/dev/block/254:0/uevent"
        | "/sys/dev/block/254:1/uevent" => {
            return Some(Kstat::new(crate::fs::S_IFREG | 0o444));
        }
        _ => {}
    }
    let mut st = match path {
        "/dev/null" => Kstat::new(crate::fs::S_IFCHR | 0o666),
        "/dev/zero" => Kstat::new(crate::fs::S_IFCHR | 0o666),
        "/dev/random" | "/dev/urandom" => Kstat::new(crate::fs::S_IFCHR | 0o444),
        "/dev/rtc" | "/dev/misc/rtc" => Kstat::new(crate::fs::S_IFCHR | 0o444),
        _ => {
            let index = block_device_index(path)?;
            let mut st = Kstat::new(crate::fs::S_IFBLK | 0o660);
            st.st_rdev = 0xfe00 + index as u64;
            st.st_size = crate::drivers::block::get_block_device(index)
                .and_then(|dev| dev.capacity_bytes())
                .unwrap_or(512 * 1024 * 1024) as i64;
            return Some(st);
        }
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

fn block_device_index(path: &str) -> Option<usize> {
    match path {
        "/dev/vda" | "vda" => Some(0),
        "/dev/vdb" | "vdb" => Some(1),
        "/dev/vdc" | "vdc" => Some(2),
        "/dev/vdd" | "vdd" => Some(3),
        _ => None,
    }
}

fn special_device_as_parent(path: &str) -> bool {
    path.starts_with("/dev/null/")
        || path.starts_with("/dev/zero/")
        || path.starts_with("/dev/random/")
        || path.starts_with("/dev/urandom/")
        || path.starts_with("/dev/rtc/")
        || path.starts_with("/dev/misc/rtc/")
}

pub fn sys_mkdir(path: *const u8, mode: u32) -> isize {
    sys_mkdirat(AT_FDCWD, path, mode)
}

pub fn sys_mkdirat(dirfd: isize, path: *const u8, mode: u32) -> isize {
    let path = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path) {
        return -36;
    }
    let base_dir = if path.starts_with('/') {
        current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone())
    } else if dirfd == AT_FDCWD {
        current_task().unwrap().inner_exclusive_access().cwd.clone()
    } else if dirfd < 0 {
        return -9; // EBADF
    } else {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        let fd = dirfd as usize;
        if fd >= inner.fd_table.len() {
            return -9; // EBADF
        }
        let Some(file) = &inner.fd_table[fd] else {
            return -9; // EBADF
        };
        if !file.is_dir() {
            return -20; // ENOTDIR
        }
        if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
            inode.get_dentry().clone()
        } else {
            return -20; // ENOTDIR
        }
    };
    if path.is_empty() {
        return -2;
    }
    if special_device_stat(path.as_str()).is_some() {
        return -17;
    }
    if vfs_resolve_path(&base_dir, path.as_str()).is_some() {
        return -17;
    }
    let (parent, name) = match resolve_parent_and_name_checked(&base_dir, path.as_str()) {
        Ok(parts) => parts,
        Err(errno) => return errno,
    };
    if vfs_dentry_is_readonly(&parent) {
        return EROFS;
    }
    if !can_modify_directory(&parent) {
        return -13;
    }
    if vfs_create(&parent, &name, true, mode).is_some() {
        0
    } else {
        -2 // ENOENT
    }
}

pub fn sys_unlink(path: *const u8) -> isize {
    sys_unlinkat(AT_FDCWD, path, 0)
}

const AT_REMOVEDIR: u32 = 0x200;

pub fn sys_unlinkat(dirfd: isize, path: *const u8, flags: u32) -> isize {
    let is_dir = (flags & AT_REMOVEDIR) != 0;
    if flags & !AT_REMOVEDIR != 0 {
        return -22;
    }
    let raw_path = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&raw_path) {
        return -36;
    }
    let path = map_dev_shm_path(&raw_path);
    let base_dir = match resolve_base_dir_for_path(dirfd, path.as_str()) {
        Ok(base) => base,
        Err(errno) => return errno,
    };
    if path.is_empty() {
        return -2;
    }
    let (parent, name) = match resolve_parent_and_name_checked(&base_dir, path.as_str()) {
        Ok(parts) => parts,
        Err(errno) => return errno,
    };
    let dentry = match vfs_lookup(&parent, &name) {
        Some(dentry) => dentry,
        None => return -2,
    };
    const FS_IMMUTABLE_FL: usize = 0x00000010;
    const FS_APPEND_FL: usize = 0x00000020;
    if crate::fs::inode::path_fs_flags(&dentry.fs_path) & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
        return -1;
    }
    if dentry.is_dir && !is_dir {
        return -21;
    }
    if !dentry.is_dir && is_dir {
        return -20;
    }
    if vfs_dentry_is_readonly(&parent) {
        return EROFS;
    }
    if !can_modify_directory(&parent) {
        return -13;
    }
    if unlink_at(Some(base_dir.clone()), path.as_str(), is_dir) {
        if !is_dir {
            crate::syscall::process::remove_shared_file_mappings(&dentry.fs_path);
        }
        0
    } else {
        -1
    }
}

pub fn sys_chdir(path: *const u8) -> isize {
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path_str) {
        return -36;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let cwd = inner.cwd.clone();
    drop(inner);
    let Some(dir_dentry) = vfs_resolve_path(&cwd, &path_str) else {
        return -2;
    };
    let dir_dentry = if dir_dentry.is_link {
        match follow_symlink(&cwd, dir_dentry, 0) {
            Some(target) => target,
            None => return -40,
        }
    } else {
        dir_dentry
    };
    if !dir_dentry.is_dir {
        return -20;
    }
    if !can_search_directory(&dir_dentry) {
        return -13;
    }
    let mut inner = task.inner_exclusive_access();
    inner.cwd = dir_dentry;
    0
}

pub fn sys_fchdir(fd: usize) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    let dentry = file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .map(|inode| inode.dentry().clone());
    drop(inner);
    let Some(dentry) = dentry else {
        return -20;
    };
    if !dentry.is_dir {
        return -20;
    }
    if !can_search_directory(&dentry) {
        return -13;
    }
    let mut inner = task.inner_exclusive_access();
    inner.cwd = dentry;
    0
}

pub fn sys_chroot(path: *const u8) -> isize {
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path_str) {
        return -36;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
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
        let permitted = match dentry.inode_info() {
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
            return 0; // both UTIME_OMIT → ctime unchanged
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
    dentry.update_cached_times(
        new_atime.map(|v| v.clamp(0, u32::MAX as i64) as u32),
        new_mtime.map(|v| v.clamp(0, u32::MAX as i64) as u32),
        if changed {
            Some(now_sec.clamp(0, u32::MAX as i64) as u32)
        } else {
            None
        },
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

pub fn sys_faccessat(dirfd: isize, path: *const u8, mode: usize, _flags: usize) -> isize {
    sys_faccessat2(dirfd, path, mode, 0)
}

fn inode_perm_allows(
    mode: u32,
    uid: u32,
    gid: u32,
    check_uid: u32,
    check_gid: u32,
    need: u32,
) -> bool {
    let mode = mode & 0o777;
    let owner_perm = (mode >> 6) & 0o7;
    let group_perm = (mode >> 3) & 0o7;
    let other_perm = mode & 0o7;
    let perm = if uid == check_uid {
        owner_perm
    } else if gid == check_gid || current_has_group(gid) {
        group_perm
    } else {
        other_perm
    };
    (perm & need) == need
}

fn current_has_group(gid: u32) -> bool {
    current_task()
        .map(|task| {
            let inner = task.inner_exclusive_access();
            inner.egid == gid || inner.supplementary_groups.contains(&gid)
        })
        .unwrap_or(false)
}

fn can_modify_directory(dir: &Arc<Dentry>) -> bool {
    let Some(task) = current_task() else {
        return false;
    };
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if euid == 0 {
        return true;
    }
    match dir.inode_info() {
        Some(info) => inode_perm_allows(info.mode, info.uid, info.gid, euid, egid, 0o3),
        None => false,
    }
}

fn can_search_directory(dir: &Arc<Dentry>) -> bool {
    let Some(task) = current_task() else {
        return false;
    };
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if euid == 0 {
        return true;
    }
    match dir.inode_info() {
        Some(info) => inode_perm_allows(info.mode, info.uid, info.gid, euid, egid, 0o1),
        None => false,
    }
}

fn chmod_requested_mode(requested: u32, file_gid: u32) -> u32 {
    let mut perm = requested & 0o7777;
    let Some(task) = current_task() else {
        return perm;
    };
    let euid = task.inner_exclusive_access().euid;
    if euid != 0 && (perm & 0o2000) != 0 && !current_has_group(file_gid) {
        perm &= !0o2000;
    }
    perm
}

fn check_traverse_to_leaf(
    base: &Arc<Dentry>,
    path: &str,
    check_uid: u32,
    check_gid: u32,
) -> Result<(), isize> {
    let path = path.trim_matches('\0');
    let mut current = if path.starts_with('/') {
        current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone())
    } else {
        base.clone()
    };
    let components: Vec<&str> = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect();
    if components.len() <= 1 {
        return Ok(());
    }
    for component in components.iter().take(components.len() - 1) {
        current = resolve_search_dir(current)?;
        let Some(next) = vfs_lookup(&current, component) else {
            return Err(-2);
        };
        let next = if next.is_link {
            follow_symlink(&current, next, 0).ok_or(-40isize)?
        } else {
            next
        };
        if !next.is_dir {
            return Err(-20);
        }
        if check_uid != 0 {
            let Some(info) = next.inode_info() else {
                return Err(-2);
            };
            if !inode_perm_allows(info.mode, info.uid, info.gid, check_uid, check_gid, 0o1) {
                return Err(-13);
            }
        }
        current = next;
    }
    Ok(())
}

pub fn sys_faccessat2(dirfd: isize, path: *const u8, mode: usize, flags: usize) -> isize {
    const F_OK: usize = 0;
    const R_OK: usize = 4;
    const W_OK: usize = 2;
    const X_OK: usize = 1;
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;
    const AT_EACCESS: usize = 0x200;
    const AT_EMPTY_PATH: usize = 0x1000;

    if mode & !(R_OK | W_OK | X_OK) != 0 {
        return -22;
    }
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EACCESS | AT_EMPTY_PATH) != 0 {
        return -22;
    }
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path_str) {
        return -36;
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
    let base = match resolve_base_dir_for_path(dirfd, path_str.as_str()) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let task = current_task().unwrap();
    let (uid, gid, euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.uid, inner.gid, inner.euid, inner.egid)
    };
    let (check_uid, check_gid) = if flags & AT_EACCESS != 0 {
        (euid, egid)
    } else {
        (uid, gid)
    };
    if let Err(errno) = check_traverse_to_leaf(&base, &path_str, check_uid, check_gid) {
        return errno;
    }
    let dentry = match resolve_path_checked(&base, &path_str) {
        Ok(d) => d,
        Err(errno) => return errno,
    };
    let dentry = if flags & AT_SYMLINK_NOFOLLOW == 0 {
        match crate::fs::follow_symlink(&base, dentry, 0) {
            Some(d) => d,
            None => return -40,
        }
    } else {
        dentry
    };

    if mode == F_OK {
        return 0;
    }
    if (mode & W_OK) != 0 && vfs_dentry_is_readonly(&dentry) {
        return EROFS;
    }

    let allowed = match dentry.inode_info() {
        Some(info) => {
            if check_uid == 0 {
                return if (mode & X_OK) != 0 && (info.mode & 0o111) == 0 {
                    -13
                } else {
                    0
                };
            }
            let mut need = 0;
            if (mode & R_OK) != 0 {
                need |= 0o4;
            }
            if (mode & W_OK) != 0 {
                need |= 0o2;
            }
            if (mode & X_OK) != 0 {
                need |= 0o1;
            }
            if !inode_perm_allows(info.mode, info.uid, info.gid, check_uid, check_gid, need) {
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

pub fn sys_fchmodat(dirfd: isize, path: *const u8, mode: u32) -> isize {
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_str.is_empty() {
        return -2;
    }
    if path_has_too_long_component(&path_str) {
        return -36;
    }
    let base = match resolve_base_dir_for_path(dirfd, &path_str) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let dentry = match vfs_resolve_with_fallback(&base, &path_str) {
        Some(d) => d,
        None => return -2,
    };
    let dentry = match follow_symlink(&base, dentry, 0) {
        Some(d) => d,
        None => return -40,
    };
    if vfs_dentry_is_readonly(&dentry) {
        return EROFS;
    }

    let task = current_task().unwrap();
    let euid = {
        let inner = task.inner_exclusive_access();
        inner.euid
    };

    let fs_path = &dentry.fs_path;
    if euid != 0 {
        let is_owner = match dentry.inode_info() {
            Some(info) => info.uid == euid,
            None => return -2,
        };
        if !is_owner {
            return -1;
        }
    }

    let info = match dentry.inode_info() {
        Some(info) => info,
        None => return -2,
    };
    let old_mode = info.mode;
    let file_type = if dentry.is_dir {
        crate::fs::S_IFDIR
    } else {
        let old_type = old_mode & 0o170000;
        if old_type == 0 {
            crate::fs::S_IFREG
        } else {
            old_type
        }
    };
    let new_perm = chmod_requested_mode(mode, info.gid);
    let rc = dentry.super_block.set_mode(fs_path, file_type | new_perm);
    if rc != 0 {
        return -1;
    }

    let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as u32;
    dentry.super_block.set_ctime(fs_path, now_sec);
    dentry.update_cached_mode(file_type | new_perm);
    dentry.update_cached_times(None, None, Some(now_sec));
    0
}

pub fn sys_fchmodat2(dirfd: isize, path: *const u8, mode: u32, flags: usize) -> isize {
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;
    const AT_EMPTY_PATH: usize = 0x1000;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return -22;
    }
    if flags != 0 {
        return -95;
    }
    sys_fchmodat(dirfd, path, mode)
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

fn chown_dentry(dentry: &Arc<Dentry>, owner: u32, group: u32) -> isize {
    if vfs_dentry_is_readonly(dentry) {
        return EROFS;
    }
    let Some(task) = current_task() else {
        return -1;
    };
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    let fs_path = &dentry.fs_path;
    let info = match dentry.inode_info() {
        Some(info) => info,
        None => return -2,
    };

    let uid_unchanged = owner == u32::MAX;
    let gid_unchanged = group == u32::MAX;
    if euid != 0 {
        if !uid_unchanged || info.uid != euid {
            return -1;
        }
        if !gid_unchanged && group != egid && !current_has_group(group) {
            return -1;
        }
    }

    let new_uid = if uid_unchanged { info.uid } else { owner };
    let new_gid = if gid_unchanged { info.gid } else { group };
    let rc = dentry.super_block.set_owner(fs_path, new_uid, new_gid);
    if rc != 0 {
        return -1;
    }

    let mut new_mode = info.mode;
    if (info.mode & 0o170000) == crate::fs::S_IFREG {
        new_mode &= !0o4000;
        if (info.mode & 0o0010) != 0 {
            new_mode &= !0o2000;
        }
    }
    if new_mode != info.mode {
        let _ = dentry.super_block.set_mode(fs_path, new_mode);
    }

    let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as u32;
    dentry.super_block.set_ctime(fs_path, now_sec);
    dentry.update_cached_owner(new_uid, new_gid, new_mode, now_sec);
    0
}

pub fn sys_fchmod(fd: usize, mode: u32) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    let file = match inner.fd_table[fd].as_ref() {
        Some(file) => file.clone(),
        None => return -9,
    };
    drop(inner);

    let inode = match file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        Some(inode) => inode,
        None => return -9,
    };
    let dentry = inode.get_dentry().clone();
    if vfs_dentry_is_readonly(&dentry) {
        return EROFS;
    }

    let euid = {
        let inner = task.inner_exclusive_access();
        inner.euid
    };

    let fs_path = &dentry.fs_path;
    if euid != 0 {
        let is_owner = match dentry.inode_info() {
            Some(info) => info.uid == euid,
            None => return -9,
        };
        if !is_owner {
            return -1;
        }
    }

    let info = match dentry.inode_info() {
        Some(info) => info,
        None => return -9,
    };
    let old_mode = info.mode;
    let file_type = if dentry.is_dir {
        crate::fs::S_IFDIR
    } else {
        let old_type = old_mode & 0o170000;
        if old_type == 0 {
            crate::fs::S_IFREG
        } else {
            old_type
        }
    };
    let new_mode = file_type | chmod_requested_mode(mode, info.gid);
    let rc = dentry.super_block.set_mode(fs_path, new_mode);
    if rc != 0 {
        return -1;
    }

    let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as u32;
    dentry.super_block.set_ctime(fs_path, now_sec);
    inode.update_cached_mode(new_mode);
    inode.update_cached_times(None, None, Some(now_sec as i64));
    0
}

pub fn sys_fchownat(dirfd: isize, path: *const u8, owner: u32, group: u32, flags: u32) -> isize {
    const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
    const AT_EMPTY_PATH: u32 = 0x1000;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return -22;
    }
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_str.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return -2;
    }
    if path_has_too_long_component(&path_str) {
        return -36;
    }

    let dentry = if path_str.is_empty() {
        if dirfd < 0 {
            return -9;
        }
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        let fd = dirfd as usize;
        if fd >= inner.fd_table.len() {
            return -9;
        }
        let Some(file) = &inner.fd_table[fd] else {
            return -9;
        };
        let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() else {
            return -9;
        };
        inode.get_dentry().clone()
    } else {
        let base = match resolve_base_dir_for_path(dirfd, &path_str) {
            Ok(b) => b,
            Err(errno) => return errno,
        };
        let dentry = match vfs_resolve_with_fallback(&base, &path_str) {
            Some(d) => d,
            None => return -2,
        };
        if flags & AT_SYMLINK_NOFOLLOW == 0 {
            match follow_symlink(&base, dentry, 0) {
                Some(d) => d,
                None => return -40,
            }
        } else {
            dentry
        }
    };

    chown_dentry(&dentry, owner, group)
}

pub fn sys_fchown(fd: usize, owner: u32, group: u32) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    let file = match inner.fd_table[fd].as_ref() {
        Some(file) => file.clone(),
        None => return -9,
    };
    drop(inner);
    let inode = match file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        Some(inode) => inode,
        None => return -9,
    };
    let dentry = inode.get_dentry().clone();

    let ret = chown_dentry(&dentry, owner, group);
    if ret == 0 {
        if let Some(info) = dentry.inode_info() {
            inode.update_cached_owner(info.uid, info.gid, info.mode, info.ctime as i64);
        }
    }
    ret
}

pub fn sys_fcntl(fd: usize, cmd: usize, arg: usize) -> isize {
    const F_DUPFD: usize = 0;
    const F_GETFD: usize = 1;
    const F_SETFD: usize = 2;
    const F_GETFL: usize = 3;
    const F_SETFL: usize = 4;
    const F_DUPFD_CLOEXEC: usize = 1030;
    const FD_CLOEXEC: usize = 1;
    const MAX_FD: usize = 1024;
    const F_RDLCK: i16 = 0;
    const F_WRLCK: i16 = 1;
    const F_UNLCK: i16 = 2;
    const FLOCK_SIZE: usize = 32;

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
        // File locking stubs (both 32-bit: 5/6/7 and 64-bit: 12/13/14)
        5 | 12 => {
            let pid = current_task().map(|task| task.getpid() as i32).unwrap_or(0);
            if !inner
                .memory_set
                .valid_user_buf(arg as *const u8, FLOCK_SIZE, true)
            {
                return -14;
            }
            let token = inner.get_user_token();
            drop(inner);
            let buf = crate::mm::translated_byte_buffer(token, arg as *mut u8, FLOCK_SIZE);
            let lock_type = F_UNLCK.to_le_bytes();
            buf[0..2].copy_from_slice(&lock_type);
            buf[24..28].copy_from_slice(&pid.to_le_bytes());
            return 0;
        }
        6 | 7 | 13 | 14 => {
            if !inner
                .memory_set
                .valid_user_buf(arg as *const u8, FLOCK_SIZE, false)
            {
                return -14;
            }
            let token = inner.get_user_token();
            let file = inner.fd_table[fd].as_ref().unwrap().clone();
            drop(inner);
            let buf = crate::mm::translated_byte_buffer(token, arg as *mut u8, FLOCK_SIZE);
            let lock_type = i16::from_le_bytes([buf[0], buf[1]]);
            let whence = i16::from_le_bytes([buf[2], buf[3]]);
            if !matches!(lock_type, F_RDLCK | F_WRLCK | F_UNLCK) {
                return -22;
            }
            if !matches!(whence, 0 | 1 | 2) {
                return -22;
            }
            if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
                inode.set_lock_type(lock_type);
            }
            0
        }
        _ => -22, // EINVAL
    }
}

const TCGETS: usize = 0x5401;
const TIOCGWINSZ: usize = 0x5413;
const BLKGETSIZE: usize = 0x1260;
const BLKGETSIZE64: usize = 0x80081272;
const FS_IOC_GETFLAGS: usize = 0x80086601;
const FS_IOC_SETFLAGS: usize = 0x40086602;

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

    if request == BLKGETSIZE || request == BLKGETSIZE64 {
        if arg == 0 {
            return -14;
        }
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        if fd >= inner.fd_table.len() {
            return -9;
        }
        let file = match &inner.fd_table[fd] {
            Some(file) => file.clone(),
            None => return -9,
        };
        drop(inner);
        let Some(dev) = file.as_any().downcast_ref::<DevBlock>() else {
            return -25;
        };
        let size = dev.capacity_bytes();
        if request == BLKGETSIZE64 {
            *translated_refmut(token, arg as *mut u64) = size;
        } else {
            *translated_refmut(token, arg as *mut u64) = size / 512;
        }
        return 0;
    }

    if request == FS_IOC_GETFLAGS || request == FS_IOC_SETFLAGS {
        let word = core::mem::size_of::<usize>();
        if arg == 0 {
            return -14;
        }
        if request == FS_IOC_GETFLAGS && !valid_user_write_buf(arg as *mut u8, word) {
            return -14;
        }
        if request == FS_IOC_SETFLAGS && !valid_user_read_buf(arg as *const u8, word) {
            return -14;
        }
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        if fd >= inner.fd_table.len() {
            return -9;
        }
        let file = match &inner.fd_table[fd] {
            Some(file) => file.clone(),
            None => return -9,
        };
        drop(inner);
        let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() else {
            return -25;
        };
        let fs_path = inode.get_dentry().fs_path.clone();
        if request == FS_IOC_GETFLAGS {
            *translated_refmut(token, arg as *mut usize) =
                crate::fs::inode::path_fs_flags(&fs_path);
        } else {
            let flags = *translated_refmut(token, arg as *mut usize);
            crate::fs::inode::set_path_fs_flags(&fs_path, flags);
        }
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
    let tgid = inner.tgid;
    if inner.close_fd(fd) {
        drop(inner);
        close_fd_for_thread_group(task.getpid(), tgid, fd);
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
    if pipe.is_null() {
        return -14; // EFAULT
    }
    if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        return -22; // EINVAL
    }
    let task = current_task().unwrap();
    let token = current_user_token();
    let (read_fd, write_fd) = {
        let mut inner = task.inner_exclusive_access();
        let (pipe_read, pipe_write) = make_pipe();
        if flags & O_NONBLOCK != 0 {
            pipe_read.set_flags(O_NONBLOCK);
            pipe_write.set_flags(O_NONBLOCK);
        }
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
    let mode_type = file.stat().st_mode & 0o170000;
    if mode_type != 0o100000 {
        return -29; // ESPIPE
    }

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
    let target_str = match translated_path_bounded(target) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let linkpath_str = match translated_path_bounded(linkpath) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if target_str.is_empty() || linkpath_str.is_empty() {
        return -2; // ENOENT
    }
    if path_has_too_long_component(&linkpath_str) {
        return -36;
    }
    let base = match resolve_base_dir_for_path(newdirfd, &linkpath_str) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    if vfs_resolve_path(&base, &linkpath_str).is_some() {
        return -17;
    }
    let (parent, _) = match resolve_parent_and_name_checked(&base, &linkpath_str) {
        Ok(parts) => parts,
        Err(errno) => return errno,
    };
    if vfs_dentry_is_readonly(&parent) {
        return EROFS;
    }
    if !can_modify_directory(&parent) {
        return -13;
    }
    if symlink_at(Some(base), &target_str, &linkpath_str) {
        0
    } else {
        -1
    }
}

pub fn sys_linkat(
    olddirfd: isize,
    oldpath: *const u8,
    newdirfd: isize,
    newpath: *const u8,
    flags: u32,
) -> isize {
    const AT_SYMLINK_FOLLOW: u32 = 0x400;
    const EXDEV: isize = -18;
    if flags & !AT_SYMLINK_FOLLOW != 0 {
        return -22;
    }
    let old_str = match translated_path_bounded(oldpath) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let new_str = match translated_path_bounded(newpath) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&old_str) || path_has_too_long_component(&new_str) {
        return -36;
    }
    if old_str.is_empty() || new_str.is_empty() {
        return -2;
    }
    let old_lookup = if flags & AT_SYMLINK_FOLLOW != 0 {
        proc_fd_readlink(&old_str).unwrap_or_else(|| old_str.clone())
    } else {
        old_str.clone()
    };
    if old_lookup.starts_with("/proc/") {
        return EXDEV;
    }
    let old_base = match resolve_base_dir_for_path(olddirfd, &old_lookup) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let new_base = match resolve_base_dir_for_path(newdirfd, &new_str) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let task = current_task().unwrap();
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if let Err(errno) = check_traverse_to_leaf(&old_base, &old_lookup, euid, egid) {
        return errno;
    }
    let old_dentry = match resolve_path_checked(&old_base, &old_lookup) {
        Ok(dentry) => {
            if flags & AT_SYMLINK_FOLLOW != 0 && dentry.is_link {
                match follow_symlink(&old_base, dentry, 0) {
                    Some(target) => target,
                    None => return -40,
                }
            } else {
                dentry
            }
        }
        Err(errno) => return errno,
    };
    if old_dentry.is_dir {
        return -1;
    }
    if let Err(errno) = check_traverse_to_leaf(&new_base, &new_str, euid, egid) {
        return errno;
    }
    let (new_parent, new_name) = match resolve_parent_and_name_checked(&new_base, &new_str) {
        Ok(parts) => parts,
        Err(errno) => return errno,
    };
    if vfs_lookup(&new_parent, &new_name).is_some() {
        return -17;
    }
    if vfs_dentry_is_readonly(&new_parent) {
        return EROFS;
    }
    if !can_modify_directory(&new_parent) {
        return -13;
    }
    if link_at(Some(old_base), &old_lookup, Some(new_base), &new_str) {
        0
    } else {
        -1
    }
}

fn read_symlink_target(dentry: &Arc<Dentry>, max_len: usize) -> Result<Vec<u8>, isize> {
    let is_link = match dentry.inode_info() {
        Some(info) => info.is_link,
        None => false,
    };
    if !is_link {
        return Err(-22);
    }

    let mut link_data = vec![0u8; max_len.min(4096)];
    let mut read_len: usize = 0;
    let c_path = match alloc::ffi::CString::new(dentry.fs_path.as_bytes()) {
        Ok(path) => path,
        Err(_) => return Err(-22),
    };
    let rc = unsafe {
        lwext4_rust::bindings::ext4_readlink(
            c_path.as_ptr() as *const core::ffi::c_char,
            link_data.as_mut_ptr() as *mut core::ffi::c_char,
            link_data.len(),
            &mut read_len,
        )
    };
    if rc != 0 {
        return Err(-22);
    }
    link_data.truncate(read_len.min(link_data.len()));
    Ok(link_data)
}

pub fn sys_readlinkat(dirfd: isize, path: *const u8, buf: *mut u8, bufsiz: usize) -> isize {
    if path.is_null() || buf.is_null() {
        return -14;
    }
    if bufsiz == 0 {
        return -22;
    }
    if !valid_user_write_buf(buf, bufsiz) {
        return -14;
    }

    let token = current_user_token();
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path_str) {
        return -36;
    }
    if path_str.is_empty() {
        if dirfd < 0 {
            return -2;
        }
        let task = current_task().unwrap();
        let fd = dirfd as usize;
        let dentry = {
            let inner = task.inner_exclusive_access();
            if fd >= inner.fd_table.len() {
                return -9;
            }
            let Some(file) = &inner.fd_table[fd] else {
                return -9;
            };
            let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() else {
                return -22;
            };
            inode.get_dentry().clone()
        };
        let link_target = match read_symlink_target(&dentry, bufsiz) {
            Ok(target) => target,
            Err(errno) => return errno,
        };
        let len = link_target.len().min(bufsiz);
        let user_buf = crate::mm::translated_byte_buffer(token, buf, len);
        user_buf.copy_from_slice(&link_target[..len]);
        return len as isize;
    }
    if let Some(link_target) = proc_fd_readlink(path_str.as_str()) {
        let bytes = link_target.as_bytes();
        let len = bytes.len().min(bufsiz);
        let user_buf = crate::mm::translated_byte_buffer(token, buf, len);
        user_buf.copy_from_slice(&bytes[..len]);
        return len as isize;
    }

    let base = match resolve_base_dir_for_path(dirfd, &path_str) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let task = current_task().unwrap();
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if let Err(errno) = check_traverse_to_leaf(&base, &path_str, euid, egid) {
        return errno;
    }
    let dentry = match resolve_path_checked(&base, &path_str) {
        Ok(d) => d,
        Err(errno) => return errno,
    };

    let link_target = match read_symlink_target(&dentry, bufsiz) {
        Ok(target) => target,
        Err(errno) => return errno,
    };

    let len = link_target.len().min(bufsiz);
    let user_buf = crate::mm::translated_byte_buffer(token, buf, len);
    user_buf.copy_from_slice(&link_target[..len]);
    len as isize
}

pub fn sys_fstatat(dirfd: isize, path: *const u8, stat_buf: *mut u8, flags: usize) -> isize {
    const AT_EMPTY_PATH: usize = 0x1000;
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;
    const AT_NO_AUTOMOUNT: usize = 0x800;

    if !valid_user_write_buf(stat_buf, core::mem::size_of::<Kstat>()) {
        return -14; // EFAULT
    }
    if flags & !(AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT) != 0 {
        return -22;
    }

    let token = current_user_token();
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path_str) {
        return -36;
    }
    if path_str.is_empty() {
        return if flags & AT_EMPTY_PATH != 0 {
            sys_fstat(dirfd as usize, stat_buf)
        } else {
            -2
        };
    }
    if let Some(st) = special_device_stat(path_str.as_str()) {
        *translated_refmut(token, stat_buf as *mut Kstat) = st;
        return 0;
    }

    let base = match resolve_base_dir_for_path(dirfd, &path_str) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let task = current_task().unwrap();
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if let Err(errno) = check_traverse_to_leaf(&base, &path_str, euid, egid) {
        return errno;
    }
    let dentry = match resolve_path_checked(&base, &path_str) {
        Ok(d) => d,
        Err(errno) => return errno,
    };
    let dentry = if flags & AT_SYMLINK_NOFOLLOW == 0 {
        match crate::fs::follow_symlink(&base, dentry, 0) {
            Some(d) => d,
            None => return -40,
        }
    } else {
        dentry
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
    mask: u32,
    statx_buf: *mut u8,
) -> isize {
    const AT_EMPTY_PATH: u32 = 0x1000;
    const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
    const AT_NO_AUTOMOUNT: u32 = 0x800;
    const AT_STATX_FORCE_SYNC: u32 = 0x2000;
    const AT_STATX_DONT_SYNC: u32 = 0x4000;
    const STATX_KNOWN_MASK: u32 = 0x3fff;

    if !valid_user_write_buf(statx_buf, core::mem::size_of::<Statx>()) {
        return -14;
    }
    if flags
        & !(AT_EMPTY_PATH
            | AT_SYMLINK_NOFOLLOW
            | AT_NO_AUTOMOUNT
            | AT_STATX_FORCE_SYNC
            | AT_STATX_DONT_SYNC)
        != 0
    {
        return -22;
    }
    if mask & !STATX_KNOWN_MASK != 0 {
        return -22;
    }

    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path_str) {
        return -36;
    }

    if path_str.is_empty() && flags & AT_EMPTY_PATH != 0 {
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
        write_statx(statx_buf, &st, mask);
        return 0;
    }

    if path_str.is_empty() {
        return -2;
    }
    if let Some(st) = special_device_stat(path_str.as_str()) {
        write_statx(statx_buf, &st, mask);
        return 0;
    }

    let base = match resolve_base_dir_for_path(dirfd, &path_str) {
        Ok(b) => b,
        Err(errno) => return errno,
    };
    let task = current_task().unwrap();
    let (euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.euid, inner.egid)
    };
    if let Err(errno) = check_traverse_to_leaf(&base, &path_str, euid, egid) {
        return errno;
    }
    let dentry = match resolve_path_checked(&base, &path_str) {
        Ok(d) => d,
        Err(errno) => return errno,
    };
    let dentry = if flags & AT_SYMLINK_NOFOLLOW == 0 {
        match crate::fs::follow_symlink(&base, dentry, 0) {
            Some(d) => d,
            None => return -40,
        }
    } else {
        dentry
    };

    let inode = crate::fs::inode::OSInode::new(false, false, false, dentry);
    let st = inode.stat();
    write_statx(statx_buf, &st, mask);
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
    if !valid_user_write_buf(stat_buf, core::mem::size_of::<Kstat>()) {
        return -14; // EFAULT
    }
    let task = current_task().unwrap();
    let token = current_user_token();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9; // EBADF
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        drop(inner);
        let st = file.stat();
        *translated_refmut(token, stat_buf as *mut Kstat) = st;
        0
    } else {
        -9 // EBADF
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

use crate::drivers::block::VirtIOBlock;
use crate::drivers::{BLOCK_DEVICE, ROOT_BLOCK_DEVICE};
use crate::fs::vfs::{vfs_mount, vfs_umount};

fn parse_tmpfs_size(data: &str) -> Option<u64> {
    for option in data.split(',') {
        let Some(value) = option.strip_prefix("size=") else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let mut digits_end = 0usize;
        for byte in value.as_bytes() {
            if byte.is_ascii_digit() {
                digits_end += 1;
            } else {
                break;
            }
        }
        if digits_end == 0 {
            return None;
        }
        let number = value[..digits_end].parse::<u64>().ok()?;
        let suffix = value[digits_end..].trim();
        let multiplier = match suffix
            .as_bytes()
            .first()
            .copied()
            .map(|b| b.to_ascii_lowercase())
        {
            Some(b'k') => 1024,
            Some(b'm') => 1024 * 1024,
            Some(b'g') => 1024 * 1024 * 1024,
            Some(b'%') => return None,
            Some(_) => 1,
            None => 1,
        };
        return number.checked_mul(multiplier);
    }
    None
}

pub fn sys_mount(
    source: *const u8,
    target: *const u8,
    fstype: *const u8,
    mountflags: usize,
    data: *const u8,
) -> isize {
    const MS_RDONLY: usize = 1;
    const MS_REMOUNT: usize = 32;

    if target.is_null() {
        return -14;
    }
    if fstype.is_null() {
        return -22;
    }
    let token = current_user_token();
    let target_path = translated_str(token, target);
    let fstype_str = translated_str(token, fstype);
    let source_str = if source.is_null() {
        String::new()
    } else {
        translated_str(token, source)
    };

    let fstype_name = fstype_str.trim_end_matches('\0');
    let source_name = source_str.trim_end_matches('\0');

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

    if fstype_name == "tmpfs" {
        let data_str = if data.is_null() {
            String::new()
        } else {
            translated_str(token, data)
        };
        let data_name = data_str.trim_end_matches('\0');
        vfs_set_tmpfs_mount(&mnt_point.path, parse_tmpfs_size(data_name));
        vfs_set_readonly_mount(&mnt_point.path, mountflags & MS_RDONLY != 0);
        return 0;
    }

    if mountflags & MS_REMOUNT != 0 {
        vfs_set_readonly_mount(&mnt_point.path, mountflags & MS_RDONLY != 0);
        return 0;
    }

    if fstype_name != "ext4" && fstype_name != "vfat" {
        return -19;
    }

    if fstype_name == "vfat" && matches!(source_name, "/dev/vda2" | "vda2") {
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

    let blk_dev = match source_name {
        "" | "vda" | "/dev/vda" => BLOCK_DEVICE.clone(),
        "vdb" | "/dev/vdb" => ROOT_BLOCK_DEVICE.clone(),
        "vdc" | "/dev/vdc" => Arc::new(VirtIOBlock::new_at(polyhal::PhysAddr::new(0x10003000))),
        "vdd" | "/dev/vdd" => Arc::new(VirtIOBlock::new_at(polyhal::PhysAddr::new(0x10004000))),
        name if name.starts_with("/dev/") => return -15,
        _ => return -2,
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

    if vfs_mount(mnt_point.clone(), sb, lwext4_mount_path.as_str()) {
        vfs_set_readonly_mount(&mnt_point.path, mountflags & MS_RDONLY != 0);
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
        let mount = table.remove(idx);
        mount.mnt_root.super_block.force_umount();
    }
    drop(table);
    vfs_clear_tmpfs_mount(path);
    vfs_set_readonly_mount(path, false);
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
const PIPEFS_MAGIC: i64 = 0x50495045;
const PROC_SUPER_MAGIC: i64 = 0x9fa0;
const SOCKFS_MAGIC: i64 = 0x534f434b;
const SYSFS_MAGIC: i64 = 0x62656572;
const TMPFS_MAGIC: i64 = 0x01021994;

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

fn fill_pseudo_statfs(f_type: i64, buf: &mut StatFs) {
    *buf = StatFs {
        f_type,
        f_bsize: 4096,
        f_blocks: 0,
        f_bfree: 0,
        f_bavail: 0,
        f_files: 0,
        f_ffree: 0,
        f_fsid: [0, 0],
        f_namelen: 255,
        f_frsize: 4096,
        f_flags: 0,
        f_spare: [0; 4],
    };
}

fn pseudo_statfs_magic_for_path(path: &str) -> Option<i64> {
    if path == "/proc" || path.starts_with("/proc/") {
        return Some(PROC_SUPER_MAGIC);
    }
    if path == "/sys" || path.starts_with("/sys/") {
        return Some(SYSFS_MAGIC);
    }
    if path == "/dev" || path.starts_with("/dev/") {
        return Some(TMPFS_MAGIC);
    }
    None
}

fn pseudo_statfs_magic_for_mode(mode: u32) -> i64 {
    match mode & 0o170000 {
        crate::fs::S_IFIFO => PIPEFS_MAGIC,
        crate::fs::S_IFSOCK => SOCKFS_MAGIC,
        crate::fs::S_IFCHR | crate::fs::S_IFBLK => TMPFS_MAGIC,
        _ => TMPFS_MAGIC,
    }
}

pub fn sys_statfs(path: *const u8, buf: *mut u8) -> isize {
    let token = current_user_token();
    if !valid_user_write_buf(buf, core::mem::size_of::<StatFs>()) {
        return -14;
    }
    let path = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_has_too_long_component(&path) {
        return -36;
    }
    if let Some(magic) = pseudo_statfs_magic_for_path(&path) {
        let st = translated_refmut(token, buf as *mut StatFs);
        fill_pseudo_statfs(magic, st);
        return 0;
    }
    let task = current_task().unwrap();
    let (cwd, euid, egid) = {
        let inner = task.inner_exclusive_access();
        (inner.cwd.clone(), inner.euid, inner.egid)
    };
    if let Err(errno) = check_traverse_to_leaf(&cwd, path.as_str(), euid, egid) {
        return errno;
    }
    let dentry = match resolve_path_checked(&cwd, path.as_str()) {
        Ok(d) => d,
        Err(errno) => return errno,
    };
    let dentry = if dentry.is_link {
        match follow_symlink(&cwd, dentry, 0) {
            Some(target) => target,
            None => return -40,
        }
    } else {
        dentry
    };
    let st = translated_refmut(token, buf as *mut StatFs);
    fill_statfs_from_dentry(&dentry, st);
    0
}

pub fn sys_pread64(fd: usize, buf: *mut u8, len: usize, offset: usize) -> isize {
    if offset > isize::MAX as usize {
        return -22;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    let inode = file.as_any().downcast_ref::<crate::fs::inode::OSInode>();
    if inode.is_none() {
        return -29;
    }
    if file.is_dir() {
        return -21;
    }
    if !file.readable() {
        return -9;
    }
    drop(inner);
    if !valid_user_write_buf(buf, len) {
        return -14;
    }
    let user_buf = translated_byte_buffer(token, buf, len);
    file.read_at(offset, user_buf)
}

pub fn sys_pwrite64(fd: usize, buf: *mut u8, len: usize, offset: usize) -> isize {
    if offset > isize::MAX as usize {
        return -22;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    let inode = file.as_any().downcast_ref::<crate::fs::inode::OSInode>();
    if inode.is_none() {
        return -29;
    }
    if file.is_dir() {
        return -21;
    }
    if !file.writable() {
        return -9;
    }
    drop(inner);
    if !valid_user_read_buf(buf as *const u8, len) {
        return -14;
    }
    let user_buf = translated_byte_buffer(token, buf, len);
    file.write_at(offset, user_buf)
}

pub fn sys_preadv(fd: usize, iov: *const u8, iovcnt: usize, offset: usize) -> isize {
    use crate::mm::translated_ref;
    const IOV_MAX: usize = 1024;
    if offset > isize::MAX as usize {
        return -22;
    }
    if iovcnt > IOV_MAX {
        return -22;
    }
    if iovcnt == 0 {
        return 0;
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
    let inode = file.as_any().downcast_ref::<crate::fs::inode::OSInode>();
    if inode.is_none() {
        return -29;
    }
    if file.is_dir() {
        return -21;
    }
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
    let mut requested = 0usize;
    for i in 0..iovcnt {
        let iov_ptr = unsafe { iov.add(i * core::mem::size_of::<IoVec>()) };
        let iov: &IoVec = translated_ref(token, iov_ptr as *const IoVec);
        if iov.iov_len == 0 {
            continue;
        }
        requested = match requested.checked_add(iov.iov_len) {
            Some(sum) if sum <= isize::MAX as usize => sum,
            _ => return -22,
        };
        if iov.iov_len > isize::MAX as usize {
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

pub fn sys_pwritev(fd: usize, iov: *const u8, iovcnt: usize, offset: usize) -> isize {
    use crate::mm::translated_ref;
    const IOV_MAX: usize = 1024;
    if offset > isize::MAX as usize {
        return -22;
    }
    if iovcnt > IOV_MAX {
        return -22;
    }
    if iovcnt == 0 {
        return 0;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    // Validate iov array
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
    let inode = file.as_any().downcast_ref::<crate::fs::inode::OSInode>();
    if inode.is_none() {
        return -29;
    }
    if file.is_dir() {
        return -21;
    }
    if !file.writable() {
        return -9;
    }
    drop(inner);
    #[repr(C)]
    struct IoVec {
        iov_base: usize,
        iov_len: usize,
    }
    let mut iovecs = Vec::new();
    let mut requested = 0usize;
    for i in 0..iovcnt {
        let iov_ptr = unsafe { iov.add(i * core::mem::size_of::<IoVec>()) };
        let iov: &IoVec = translated_ref(token, iov_ptr as *const IoVec);
        if iov.iov_len == 0 {
            continue;
        }
        requested = match requested.checked_add(iov.iov_len) {
            Some(sum) if sum <= isize::MAX as usize => sum,
            _ => return -22,
        };
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        let ok = inner
            .memory_set
            .valid_user_buf(iov.iov_base as *const u8, iov.iov_len, false);
        drop(inner);
        if !ok {
            return -14;
        }
        iovecs.push((iov.iov_base, iov.iov_len));
    }
    let mut off = offset;
    let mut total = 0isize;
    for (base, len) in iovecs {
        let user_buf = crate::mm::translated_byte_buffer(token, base as *mut u8, len);
        let n = file.write_at(off, user_buf);
        if n < 0 {
            return if total == 0 { n } else { total };
        }
        let n = n as usize;
        total += n as isize;
        off += n;
        if n < len {
            break;
        }
    }
    total
}

pub fn sys_preadv2(fd: usize, iov: *const u8, iovcnt: usize, offset: usize, flags: usize) -> isize {
    if flags != 0 {
        return -95;
    }
    if offset == usize::MAX {
        return super::process::sys_readv(fd, iov, iovcnt);
    }
    sys_preadv(fd, iov, iovcnt, offset)
}

pub fn sys_pwritev2(
    fd: usize,
    iov: *const u8,
    iovcnt: usize,
    offset: usize,
    flags: usize,
) -> isize {
    if flags != 0 {
        return -95;
    }
    if offset == usize::MAX {
        return super::process::sys_writev(fd, iov, iovcnt);
    }
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
        Some(inode) => {
            let mode_type = inode.stat().st_mode & 0o170000;
            if mode_type != S_IFREG && mode_type != S_IFDIR {
                return -22;
            }
            inode.dentry().clone()
        }
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

pub fn sys_fsync(fd: usize) -> isize {
    sync_fd(fd)
}

pub fn sys_fdatasync(fd: usize) -> isize {
    sync_fd(fd)
}

pub fn sys_sync() -> isize {
    let rc = crate::fs::vfs::ROOT_SB.sync_all();
    if rc == 0 {
        0
    } else {
        -5
    }
}

pub fn sys_syncfs(fd: usize) -> isize {
    sync_fd(fd)
}

pub fn sys_fallocate(fd: usize, mode: usize, offset: usize, len: usize) -> isize {
    const FALLOC_FL_KEEP_SIZE: usize = 0x01;
    const EFBIG: isize = -27;
    if len == 0 {
        return -22;
    }
    if offset > isize::MAX as usize || len > isize::MAX as usize {
        return -22;
    }
    let Some(end) = offset.checked_add(len) else {
        return EFBIG;
    };
    if end > isize::MAX as usize {
        return EFBIG;
    }
    if mode & !FALLOC_FL_KEEP_SIZE != 0 {
        return -95;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    if !file.writable() {
        return -9;
    }
    if file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .is_none()
    {
        return -29;
    }
    if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        if vfs_dentry_is_readonly(inode.get_dentry()) {
            return EROFS;
        }
    }
    drop(inner);
    if mode & FALLOC_FL_KEEP_SIZE != 0 {
        return 0;
    }
    crate::syscall::fs::sys_ftruncate(fd, end as u64)
}

pub fn sys_fadvise64(fd: usize, _offset: usize, _len: usize, advice: usize) -> isize {
    const POSIX_FADV_NOREUSE: usize = 5;
    if advice > POSIX_FADV_NOREUSE {
        return -22;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    if file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .is_none()
    {
        return -29;
    }
    0
}

pub fn sys_fstatfs(fd: usize, buf: *mut u8) -> isize {
    let token = current_user_token();
    if !valid_user_write_buf(buf, core::mem::size_of::<StatFs>()) {
        return -14;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    let file = match &inner.fd_table[fd] {
        Some(f) => f.clone(),
        None => return -9,
    };
    let dentry = file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .map(|inode| inode.dentry());
    drop(inner);
    let st = translated_refmut(token, buf as *mut StatFs);
    match dentry {
        Some(d) => {
            fill_statfs_from_dentry(d, st);
            0
        }
        None => {
            let mode = file.stat().st_mode;
            fill_pseudo_statfs(pseudo_statfs_magic_for_mode(mode), st);
            0
        }
    }
}

pub fn sys_ftruncate(fd: usize, length: u64) -> isize {
    if length > isize::MAX as u64 {
        return -22;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    let file = match &inner.fd_table[fd] {
        Some(f) => f.clone(),
        None => return -9,
    };
    if !file.writable() {
        return -22;
    }
    if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
        if vfs_dentry_is_readonly(inode.get_dentry()) {
            return EROFS;
        }
    }
    drop(inner);
    let dentry = file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .map(|inode| inode.dentry());
    match dentry {
        Some(d) => {
            let old_size = d
                .inode_info()
                .map(|info| info.size)
                .unwrap_or(0);
            if !vfs_tmpfs_can_resize(&d.path, &d.fs_path, old_size, length) {
                return -28;
            }
            if d.super_block.truncate_file(&d.fs_path, length) {
                crate::syscall::process::remove_shared_file_mappings(&d.fs_path);
                if let Some(inode) = file.as_any().downcast_ref::<crate::fs::inode::OSInode>() {
                    inode.refresh_after_truncate(length as i64);
                }
                vfs_tmpfs_commit_resize(&d.path, &d.fs_path, old_size, length);
                0
            } else {
                -1
            }
        }
        None => -22,
    }
}

// ---- flock (32) ----

pub fn sys_flock(fd: usize, op: usize) -> isize {
    const LOCK_SH: usize = 1;
    const LOCK_EX: usize = 2;
    const LOCK_NB: usize = 4;
    const LOCK_UN: usize = 8;
    let op_kind = op & !LOCK_NB;
    if op_kind != LOCK_SH && op_kind != LOCK_EX && op_kind != LOCK_UN {
        return -22;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    0
}

// ---- mknodat (33) ----

pub fn sys_mknodat(dirfd: isize, path: *const u8, mode: u32, _dev: usize) -> isize {
    let path_str = match translated_path_bounded(path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if path_str.is_empty() {
        return -2;
    }
    if path_has_too_long_component(&path_str) {
        return -36;
    }
    let base = match resolve_base_dir_for_path(dirfd, &path_str) {
        Ok(base) => base,
        Err(errno) => return errno,
    };
    if vfs_resolve_path(&base, &path_str).is_some() {
        return -17;
    }
    let (parent, name) = match resolve_parent_and_name_checked(&base, &path_str) {
        Ok(parts) => parts,
        Err(errno) => return errno,
    };
    if vfs_dentry_is_readonly(&parent) {
        return EROFS;
    }
    if !can_modify_directory(&parent) {
        return -13;
    }
    let create_mode = if mode == 0 { 0o666 } else { mode };
    let mode_type = create_mode & 0o170000;
    match mode_type {
        0 | crate::fs::S_IFREG | crate::fs::S_IFIFO | crate::fs::S_IFSOCK => {}
        crate::fs::S_IFCHR | crate::fs::S_IFBLK => {
            let euid = current_task()
                .map(|task| task.inner_exclusive_access().euid)
                .unwrap_or(1);
            if euid != 0 {
                return -1;
            }
        }
        _ => return -22,
    }
    if let Some(dentry) = crate::fs::vfs::vfs_create(&parent, &name, false, create_mode) {
        crate::fs::inode::record_special_path_mode(&dentry.fs_path, mode_type);
        0
    } else {
        -17
    }
}

// ---- readahead (213) ----

pub fn sys_readahead(fd: usize, _offset: usize, _count: usize) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
    }
    let file = inner.fd_table[fd].as_ref().unwrap().clone();
    if !file.readable() {
        return -9;
    }
    if file.is_dir() {
        return -22;
    }
    if file
        .as_any()
        .downcast_ref::<crate::fs::inode::OSInode>()
        .is_none()
    {
        return -22;
    }
    0
}

// ---- copy_file_range (285) ----

pub fn sys_copy_file_range(
    fd_in: usize,
    off_in: *mut usize,
    fd_out: usize,
    off_out: *mut usize,
    len: usize,
    flags: usize,
) -> isize {
    const EBADF: isize = -9;
    const EFAULT: isize = -14;
    const EISDIR: isize = -21;
    const EINVAL: isize = -22;
    const EFBIG: isize = -27;
    const EOVERFLOW: isize = -75;
    const S_IFMT: u32 = 0o170000;

    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd_in >= inner.fd_table.len() || inner.fd_table[fd_in].is_none() {
        return EBADF;
    }
    if fd_out >= inner.fd_table.len() || inner.fd_table[fd_out].is_none() {
        return EBADF;
    }
    let word = core::mem::size_of::<usize>();
    if !off_in.is_null()
        && !inner
            .memory_set
            .valid_user_buf(off_in as *const u8, word, true)
    {
        return EFAULT;
    }
    if !off_out.is_null()
        && !inner
            .memory_set
            .valid_user_buf(off_out as *const u8, word, true)
    {
        return EFAULT;
    }
    if flags != 0 {
        return EINVAL;
    }
    let token = inner.get_user_token();
    let f_in = inner.fd_table[fd_in].as_ref().unwrap().clone();
    let f_out = inner.fd_table[fd_out].as_ref().unwrap().clone();
    drop(inner);
    let in_offset_ptr = if off_in.is_null() {
        None::<usize>
    } else {
        Some(*translated_refmut(token, off_in))
    };
    let out_offset_ptr = if off_out.is_null() {
        None::<usize>
    } else {
        Some(*translated_refmut(token, off_out))
    };

    let mode_in = f_in.stat().st_mode & S_IFMT;
    let mode_out = f_out.stat().st_mode & S_IFMT;
    if mode_in == crate::fs::S_IFDIR || mode_out == crate::fs::S_IFDIR {
        return EISDIR;
    }
    if mode_in != crate::fs::S_IFREG || mode_out != crate::fs::S_IFREG {
        return EINVAL;
    }
    if !f_in.readable() || !f_out.writable() {
        return EBADF;
    }
    if f_out.get_flags() & OpenFlags::APPEND.bits() as usize != 0 {
        return EBADF;
    }

    let in_start = in_offset_ptr.unwrap_or_else(|| f_in.get_offset());
    let out_start = out_offset_ptr.unwrap_or_else(|| f_out.get_offset());
    if in_start > isize::MAX as usize || out_start > isize::MAX as usize {
        return EINVAL;
    }
    if len > isize::MAX as usize || out_start.checked_add(len).is_none() {
        return EOVERFLOW;
    }
    let Some(out_end) = out_start.checked_add(len) else {
        return EOVERFLOW;
    };
    if out_end > isize::MAX as usize {
        return EFBIG;
    }
    if len == 0 {
        return 0;
    }

    if let (Some(in_path), Some(out_path)) = (f_in.path(), f_out.path()) {
        if in_path == out_path {
            let in_end = in_start.saturating_add(len);
            let out_end = out_start.saturating_add(len);
            if in_start < out_end && out_start < in_end {
                return EINVAL;
            }
        }
    }

    let mut buf = [0u8; 4096];
    let mut total: isize = 0;
    while (total as usize) < len {
        let chunk = (len - total as usize).min(buf.len());
        let in_pos = in_start + total as usize;
        let out_pos = out_start + total as usize;
        let n = f_in.read_at(in_pos, &mut buf[..chunk]);
        if n == 0 {
            break;
        }
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        let w = f_out.write_at(out_pos, &mut buf[..n as usize]);
        if w < 0 {
            return if total > 0 { total } else { w };
        }
        if w == 0 {
            break;
        }
        total += w;
        if w < n {
            break;
        }
    }
    let copied = total.max(0) as usize;
    if let Some(off) = in_offset_ptr {
        *translated_refmut(token, off_in) = off + copied;
    } else {
        f_in.seek(in_start + copied);
    }
    if let Some(off) = out_offset_ptr {
        *translated_refmut(token, off_out) = off + copied;
    } else {
        f_out.seek(out_start + copied);
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

pub fn sys_close_range(first: usize, last: usize, flags: usize) -> isize {
    const CLOSE_RANGE_UNSHARE: usize = 1 << 1;
    const CLOSE_RANGE_CLOEXEC: usize = 1 << 2;
    if first > last {
        return -22;
    }
    if flags & !(CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC) != 0 {
        return -22;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let last = last.min(inner.fd_table.len().saturating_sub(1));
    if first > last {
        return 0;
    }
    if flags & CLOSE_RANGE_CLOEXEC != 0 {
        for fd in first..=last {
            if fd < inner.fd_cloexec.len()
                && inner.fd_table.get(fd).and_then(|f| f.as_ref()).is_some()
            {
                inner.fd_cloexec[fd] = true;
            }
        }
    } else {
        let tgid = inner.tgid;
        for fd in first..=last {
            inner.close_fd(fd);
        }
        drop(inner);
        for fd in first..=last {
            close_fd_for_thread_group(task.getpid(), tgid, fd);
        }
    }
    0
}

// ---- socketpair (199) ----

pub fn sys_socketpair(_domain: usize, _typ: usize, _protocol: usize, _sv: *mut usize) -> isize {
    0
}
