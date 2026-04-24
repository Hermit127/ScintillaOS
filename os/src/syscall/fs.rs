use crate::fs::{make_pipe, mkdir_at, open_file_at, unlink_at, File, Kstat, OpenFlags};
use crate::mm::{translated_byte_buffer, translated_refmut, translated_str};
use crate::task::{current_task, current_user_token};
use alloc::string::String;
use alloc::sync::Arc;

const AT_FDCWD: isize = -100;

fn resolve_base_dir(dirfd: isize) -> Option<String> {
    let task = current_task().unwrap();
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
    file.path()
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
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        file.write(translated_byte_buffer(token, buf, len)) as isize
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
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        file.read(translated_byte_buffer(token, buf, len)) as isize
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

    let open_flags = OpenFlags::from_raw(flags);
    let base_dir = match resolve_base_dir(dirfd) {
        Some(base) => base,
        None => return -1,
    };
    let inode = open_file_at(Some(base_dir.as_str()), path.as_str(), open_flags);

    if let Some(inode) = inode {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        inner.fd_table[fd] = Some(inode);
        fd as isize
    } else {
        -1
    }
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
    if mkdir_at(Some(base_dir.as_str()), path.as_str()) {
        0
    } else {
        -1
    }
}

pub fn sys_unlink(path: *const u8) -> isize {
    sys_unlinkat(AT_FDCWD, path, 0)
}

pub fn sys_unlinkat(dirfd: isize, path: *const u8, flags: u32) -> isize {
    // minimal unlinkat: only support regular unlink (flags == 0)
    if flags != 0 || path.is_null() {
        return -1;
    }
    let token = current_user_token();
    let path = translated_str(token, path);
    let base_dir = match resolve_base_dir(dirfd) {
        Some(base) => base,
        None => return -1,
    };
    if unlink_at(Some(base_dir.as_str()), path.as_str()) {
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
    let Some(dir_inode) = open_file_at(Some(cwd.as_str()), path.as_str(), OpenFlags::DIRECTORY) else {
        return -1;
    };
    let Some(new_cwd) = dir_inode.path() else {
        return -1;
    };
    let mut inner = task.inner_exclusive_access();
    inner.cwd = new_cwd;
    0
}

pub fn sys_getcwd(buf: *mut u8, size: usize) -> isize {
    if buf.is_null() || size == 0 {
        return 0;
    }
    let task = current_task().unwrap();
    let cwd = {
        let inner = task.inner_exclusive_access();
        inner.cwd.clone()
    };
    let need = cwd.len() + 1;
    if need > size {
        return 0;
    }
    let token = current_user_token();
    let user_buf = translated_byte_buffer(token, buf, size);
    user_buf[..cwd.len()].copy_from_slice(cwd.as_bytes());
    user_buf[cwd.len()] = 0;
    buf as isize
}

pub fn sys_close(fd: usize) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    inner.fd_table[fd].take();
    0
}

pub fn sys_pipe(pipe: *mut usize) -> isize {
    sys_pipe2(pipe, 0)
}

pub fn sys_pipe2(pipe: *mut usize, flags: usize) -> isize {
    // minimal pipe2: only support flags == 0
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
    *translated_refmut(token, pipe) = read_fd;
    *translated_refmut(token, unsafe { pipe.add(1) }) = write_fd;
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
    // minimal dup3: only support flags == 0 (dup2 semantics)
    if flags != 0 {
        return -1;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    let Some(file) = inner.fd_table[fd].as_ref().cloned() else {
        return -1;
    };
    if fd == new_fd {
        return new_fd as isize;
    }
    while inner.fd_table.len() <= new_fd {
        inner.fd_table.push(None);
    }
    inner.fd_table[new_fd].take();
    inner.fd_table[new_fd] = Some(file);
    new_fd as isize
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
