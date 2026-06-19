// 这里负责操作系统的文件管理：实现普通 ext4 文件的 inode 封装，以及 open/mkdir/unlink/rename/symlink 等文件操作。
use super::vfs::{
    vfs_create, vfs_resolve_parent_and_name, vfs_resolve_path, vfs_resolve_with_fallback, Dentry,
    DirEntry,
};
use super::{File, Kstat, S_IFDIR, S_IFREG};
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::ffi::CString;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;
use lazy_static::*;
use lwext4_rust::bindings::*;

const EXT4_BLOCK_SIZE: u32 = 4096;

#[derive(Clone, Copy, Default)]
struct CachedTimes {
    atime: Option<i64>,
    mtime: Option<i64>,
    ctime: Option<i64>,
}

lazy_static! {
    static ref PATH_TIME_CACHE: UPSafeCell<BTreeMap<String, CachedTimes>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
}

// 记录某个路径上用户态刚改过的时间戳，弥补底层 ext4 元数据刷新不及时的问题。
// 执行时取得全局时间缓存，按路径找到或创建条目，再只更新调用者传入的时间字段。
pub fn update_path_cached_times(
    fs_path: &str,
    atime: Option<i64>,
    mtime: Option<i64>,
    ctime: Option<i64>,
) {
    let mut cache = PATH_TIME_CACHE.exclusive_access();
    let entry = cache
        .entry(String::from(fs_path))
        .or_insert_with(CachedTimes::default);
    if let Some(atime) = atime {
        entry.atime = Some(atime);
    }
    if let Some(mtime) = mtime {
        entry.mtime = Some(mtime);
    }
    if let Some(ctime) = ctime {
        entry.ctime = Some(ctime);
    }
}

// 从路径时间缓存里取一份快照，没记录过就返回空的默认值。
// 执行时独占访问缓存，按路径复制 CachedTimes；没有记录时返回全 None 的默认值。
fn path_cached_times(fs_path: &str) -> CachedTimes {
    PATH_TIME_CACHE
        .exclusive_access()
        .get(fs_path)
        .copied()
        .unwrap_or_default()
}

// ==================== OSInode ====================

pub struct OSInode {
    readable: bool,
    writable: bool,
    dentry: Arc<Dentry>,
    inner: UPSafeCell<OSInodeInner>,
}

unsafe impl Send for OSInode {}
unsafe impl Sync for OSInode {}

pub struct OSInodeInner {
    offset: usize,
    file_handle: Option<ext4_file>,
    pub lock_type: i16,
    append: bool,
    cached_stat: Option<Kstat>,
}

impl Drop for OSInode {
    // OSInode 释放时顺手关掉 lwext4 的文件句柄，避免句柄留在 C 库里。
    // 执行时独占 inner，如果里面还有 lwext4 file_handle，就调用 ext4_fclose 交还给 C 库。
    fn drop(&mut self) {
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut f) = inner.file_handle {
            unsafe {
                ext4_fclose(f);
            }
        }
    }
}

impl OSInode {
    // 取出这个 inode 对应的目录项，调用者一般用它继续做路径或父目录操作。
    // 执行时直接返回内部 dentry 的引用，不增加 offset，也不访问底层 ext4。
    pub fn get_dentry(&self) -> &Arc<Dentry> {
        &self.dentry
    }

    // 查询当前文件锁类型，主要给 fcntl/flock 一类系统调用使用。
    // 执行时独占 inner 后读取 lock_type，返回给 fcntl 的锁查询路径。
    pub fn get_lock_type(&self) -> i16 {
        self.inner.exclusive_access().lock_type
    }

    // 更新文件锁类型；这里不做冲突判断，冲突检查放在系统调用层。
    // 执行时独占 inner 后直接写 lock_type，不在 inode 层做锁冲突仲裁。
    pub fn set_lock_type(&self, t: i16) {
        self.inner.exclusive_access().lock_type = t;
    }

    // 创建一个打开状态的 OSInode，append 模式下初始偏移直接放到文件尾。
    // 执行时根据 append 决定初始 offset，随后初始化读写权限、懒打开句柄、锁状态和 stat 缓存。
    pub fn new(readable: bool, writable: bool, append: bool, dentry: Arc<Dentry>) -> Self {
        let offset = if append {
            dentry
                .super_block
                .get_inode_info(&dentry.fs_path)
                .map(|i| i.size as usize)
                .unwrap_or(0)
        } else {
            0
        };
        Self {
            readable,
            writable,
            dentry,
            inner: unsafe {
                UPSafeCell::new_named(
                    OSInodeInner {
                        offset,
                        file_handle: None,
                        lock_type: 2,
                        append,
                        cached_stat: None,
                    },
                    "OSInodeInner",
                )
            },
        }
    }

    // 和 get_dentry 一样返回目录项，保留这个名字是为了贴合旧调用点。
    // 执行时直接返回内部 dentry 引用，行为和 get_dentry 一样。
    pub fn dentry(&self) -> &Arc<Dentry> {
        &self.dentry
    }

    // 懒打开底层 lwext4 句柄；只有需要 C 库顺序读写时才真正 fopen。
    // 执行时把 fs_path 和打开标志转成 C 字符串，调用 ext4_fopen，成功后把句柄游标同步到当前 offset。
    fn open_handle(&self) {
        let fs_path = &self.dentry.fs_path;
        let c_path = match CString::new(fs_path.as_bytes()) {
            Ok(s) => s,
            Err(_) => return,
        };
        // 底层文件已经由 VFS 创建好，这里只根据读写模式选择 lwext4 的打开标志。
        let flags = if self.writable { "r+b" } else { "rb" };
        let c_flags = match CString::new(flags) {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut file: ext4_file = unsafe { core::mem::zeroed() };
        let rc = unsafe {
            ext4_fopen(
                &mut file,
                c_path.as_ptr() as *const core::ffi::c_char,
                c_flags.as_ptr() as *const core::ffi::c_char,
            )
        };
        if rc == EOK as i32 {
            let mut inner = self.inner.exclusive_access();
            // lwext4 句柄有自己的游标，打开后要先同步到内核记录的 offset。
            let offset = inner.offset;
            unsafe {
                ext4_fseek(&mut file, offset as i64, SEEK_SET as u32);
            }
            inner.file_handle = Some(file);
        }
    }

    // 从当前偏移开始把文件内容全部读出来，常用于读取小文件或链接目标。
    // 执行时按当前 offset 循环调用 super_block.read_at，每次推进 offset 并追加到 Vec，直到读不到数据。
    pub fn read_all(&self) -> alloc::vec::Vec<u8> {
        let mut inner = self.inner.exclusive_access();
        let size = self
            .dentry
            .super_block
            .get_inode_info(&self.dentry.fs_path)
            .map(|i| i.size as usize)
            .unwrap_or(0);
        let mut out = if size > 0 {
            alloc::vec::Vec::with_capacity(size)
        } else {
            alloc::vec::Vec::new()
        };
        let mut buffer = [0u8; 4096];
        loop {
            let len =
                self.dentry
                    .super_block
                    .read_at(&self.dentry.fs_path, inner.offset, &mut buffer);
            if len == 0 {
                break;
            }
            inner.offset += len;
            out.extend_from_slice(&buffer[..len]);
        }
        out
    }

    // 组装 stat 结果，并把非目录文件的结果缓存起来减少重复查 ext4。
    // 执行时优先使用 cached_stat；缓存未命中时读取 ext4 inode 信息，合并路径时间缓存后构造 Kstat。
    fn stat_inner(&self) -> Kstat {
        {
            let mut inner = self.inner.exclusive_access();
            if let Some(mut st) = inner.cached_stat {
                if let Some(ref mut file) = inner.file_handle {
                    st.st_ino = file.inode as u64;
                    st.st_size = unsafe { ext4_fsize(file) as i64 };
                    st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
                    inner.cached_stat = Some(st);
                }
                return st;
            }
        }
        let info = self.dentry.super_block.get_inode_info(&self.dentry.fs_path);
        let st = match info {
            Some(info) => {
                let file_type = if self.dentry.is_dir || info.is_dir {
                    S_IFDIR
                } else {
                    info.mode & 0o170000
                };
                let file_type = if file_type == 0 { S_IFREG } else { file_type };
                let mut st = Kstat::new(file_type | (info.mode & 0o7777));
                st.st_ino = info.inode_num as u64;
                st.st_size = info.size as i64;
                st.st_blksize = EXT4_BLOCK_SIZE;
                st.st_blocks = info.blocks_count;
                st.st_atime_sec = info.atime as i64;
                st.st_mtime_sec = info.mtime as i64;
                st.st_ctime_sec = info.ctime as i64;
                let cached_times = path_cached_times(&self.dentry.fs_path);
                let now_sec = (polyhal::time::Time::now().to_msec() / 1000) as i64;
                st.st_atime_sec = cached_times
                    .atime
                    .unwrap_or_else(|| st.st_atime_sec.min(now_sec));
                st.st_mtime_sec = cached_times
                    .mtime
                    .unwrap_or_else(|| st.st_mtime_sec.min(now_sec));
                st.st_ctime_sec = cached_times
                    .ctime
                    .unwrap_or_else(|| st.st_ctime_sec.min(now_sec));
                st
            }
            None => {
                if self.dentry.is_dir {
                    Kstat::new(S_IFDIR | 0o755)
                } else {
                    Kstat::new(S_IFREG | 0o644)
                }
            }
        };
        if !self.dentry.is_dir {
            self.inner.exclusive_access().cached_stat = Some(st);
        }
        st
    }

    // 写入或截断之后同步缓存里的文件大小，避免 fstat 读到旧值。
    // 执行时只在 cached_stat 已经存在时更新 st_size 和 st_blocks，避免下一次 fstat 读到旧大小。
    pub fn update_cached_size(&self, size: i64) {
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut st) = inner.cached_stat {
            st.st_size = size;
            st.st_blocks = ((size.max(0) as u64) + 511) / 512;
        }
    }

    // 用户显式改 atime/mtime/ctime 后，同步到已缓存的 stat 结构里。
    // 执行时只修改调用者提供的时间字段，没传入的 atime/mtime/ctime 保持原缓存值。
    pub fn update_cached_times(&self, atime: Option<i64>, mtime: Option<i64>, ctime: Option<i64>) {
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut st) = inner.cached_stat {
            if let Some(atime) = atime {
                st.st_atime_sec = atime;
            }
            if let Some(mtime) = mtime {
                st.st_mtime_sec = mtime;
            }
            if let Some(ctime) = ctime {
                st.st_ctime_sec = ctime;
            }
        }
    }

    // 把长度按 align 向上取整，getdents64 的记录长度会用到。
    // 执行时用位运算把 value 向上补齐到 align 的整数倍，要求 align 是 2 的幂。
    pub fn align_up(value: usize, align: usize) -> usize {
        (value + align - 1) & !(align - 1)
    }
}

// ==================== OpenFlags ====================

bitflags! {
    pub struct OpenFlags: u32 {
        const RDONLY = 0;
        const WRONLY = 1 << 0;
        const RDWR = 1 << 1;
        const CREATE = 0x40;
        const EXCL = 0x80;
        const TRUNC = 0x200;
        const APPEND = 0x400;
        const DIRECTORY = 0x10000;
    }
}

impl OpenFlags {
    // 把 Linux open flags 修改成内核当前认识的那部分。
    // 执行时丢弃当前没有定义的位，只保留 OpenFlags 里声明过的标志。
    pub fn from_raw(flags: u32) -> Self {
        Self::from_bits_truncate(flags)
    }

    // 是否带 O_CREAT。
    // 执行时检查 CREATE 位是否存在，用来决定 open 是否允许创建新文件。
    pub fn create_requested(&self) -> bool {
        self.contains(Self::CREATE)
    }

    // 是否要求文件必须不存在，也就是 O_EXCL。
    // 执行时检查 EXCL 位是否存在，用来和 O_CREAT 组合出独占创建语义。
    pub fn excl_requested(&self) -> bool {
        self.contains(Self::EXCL)
    }

    // 是否要求打开时清空普通文件。
    // 执行时检查 TRUNC 位是否存在，用来决定成功打开后是否截断文件。
    pub fn trunc_requested(&self) -> bool {
        self.contains(Self::TRUNC)
    }

    // 是否要求目标必须是目录。
    // 执行时检查 DIRECTORY 位是否存在，用来拒绝非目录目标。
    pub fn directory_requested(&self) -> bool {
        self.contains(Self::DIRECTORY)
    }

    // 根据只读、只写、读写标志换成 File trait 里的读写布尔值。
    // 执行时优先识别 WRONLY/RDWR，否则按默认只读处理，返回 readable/writable 两个布尔值。
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

// ==================== File Operations ====================

// 当前任务的有效用户是不是 root，权限兜底时用。
// 执行时读取当前任务 euid，当前没有任务时按非 root 处理。
fn current_euid_is_root() -> bool {
    crate::task::current_task()
        .map(|task| task.inner_exclusive_access().euid == 0)
        .unwrap_or(false)
}

// root 打开文件时补上必要权限，兼容测例里比较宽松的 root 行为。
// 执行时只在当前 euid 为 root 时读取 inode mode，把缺少的权限位 OR 进去并写回 ext4。
fn root_make_accessible(dentry: &Arc<Dentry>, bits: u32) {
    if !current_euid_is_root() {
        return;
    }
    if let Some(info) = dentry.super_block.get_inode_info(&dentry.fs_path) {
        let new_mode = info.mode | (bits & 0o777);
        if new_mode != info.mode {
            dentry.super_block.set_mode(&dentry.fs_path, new_mode);
        }
    }
}

// 从根目录开始打开文件，是 open_file_at 的简化入口。
// 执行时把 base 固定为 None、mode 固定为 0666，再交给 open_file_new 统一处理。
pub fn open_file(path: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    open_file_new(None, path, flags, 0o666)
}

// 按给定起点打开文件，支持相对路径和创建模式。
// 执行时保留调用者传入的 base、flags 和 mode，直接转给 open_file_new 处理完整 open 语义。
pub fn open_file_at(
    base: Option<Arc<Dentry>>,
    path: &str,
    flags: OpenFlags,
    mode: u32,
) -> Option<Arc<OSInode>> {
    open_file_new(base, path, flags, mode)
}

// open 的主体逻辑：解析路径、处理创建/截断/目录要求，最后生成 OSInode。
// 执行时先确定解析起点，再按 O_CREAT/已有文件/新建文件三类路径处理，最后按读写标志构造 OSInode。
fn open_file_new(
    base: Option<Arc<Dentry>>,
    path: &str,
    flags: OpenFlags,
    mode: u32,
) -> Option<Arc<OSInode>> {
    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let want_dir = flags.directory_requested();

    // 如果带 O_CREAT，先区分目标已经存在和需要创建两条路径。
    if flags.create_requested() {
        if want_dir {
            return None;
        }

        // 目标已经存在时遵守 O_EXCL/O_TRUNC 语义，不重新创建 inode。
        if let Some(dentry) =
            vfs_resolve_with_fallback(&start, path).and_then(|d| follow_symlink(&start, d, 0))
        {
            if !dentry.is_dir {
                if flags.excl_requested() {
                    return None;
                }
                if flags.trunc_requested()
                    || flags.contains(OpenFlags::WRONLY)
                    || flags.contains(OpenFlags::RDWR)
                {
                    root_make_accessible(&dentry, 0o666);
                }
                if flags.trunc_requested() {
                    let truncated = dentry.super_block.truncate_file(&dentry.fs_path, 0);
                    if !truncated {
                        return None;
                    }
                }
                let (readable, writable) = flags.read_write();
                let append = flags.contains(OpenFlags::APPEND);
                return Some(Arc::new(OSInode::new(readable, writable, append, dentry)));
            }
        }

        // 目标不存在时先解析父目录，再在父目录里创建普通文件。
        let (parent, name) = vfs_resolve_parent_and_name(&start, path)?;
        root_make_accessible(&parent, 0o777);
        let dentry = vfs_create(&parent, &name, false, mode)?;
        let (readable, writable) = flags.read_write();
        return Some(Arc::new(OSInode::new(
            readable,
            writable,
            flags.contains(OpenFlags::APPEND),
            dentry,
        )));
    }

    // 非创建模式只接受已有路径，并把符号链接跟随到最终目标。
    let dentry = follow_symlink(&start, vfs_resolve_with_fallback(&start, path)?, 0)?;
    if want_dir && !dentry.is_dir {
        return None;
    }

    // root 以写方式打开时补上必要权限，兼容官方测例里较宽松的 root 行为。
    if !dentry.is_dir
        && (flags.trunc_requested()
            || flags.contains(OpenFlags::WRONLY)
            || flags.contains(OpenFlags::RDWR))
    {
        root_make_accessible(&dentry, 0o666);
    }

    if flags.trunc_requested() && !dentry.is_dir {
        let truncated = dentry.super_block.truncate_file(&dentry.fs_path, 0);
        if !truncated {
            return None;
        }
    }

    let (readable, writable) = if dentry.is_dir {
        (false, false)
    } else {
        flags.read_write()
    };
    let append = !dentry.is_dir && flags.contains(OpenFlags::APPEND);
    Some(Arc::new(OSInode::new(readable, writable, append, dentry)))
}

// 跟随符号链接，最多递归 8 层，防止链接环把内核绕进去。
// 执行时先排除目录和普通文件，再用 ext4_readlink 读出目标路径，按绝对或相对路径继续递归解析。
pub fn follow_symlink(
    start: &Arc<Dentry>,
    dentry: Arc<Dentry>,
    depth: usize,
) -> Option<Arc<Dentry>> {
    if depth >= 8 {
        return None;
    }
    if dentry.is_dir {
        return Some(dentry);
    }
    if !dentry.is_link {
        return Some(dentry);
    }
    let info = dentry.super_block.get_inode_info(&dentry.fs_path)?;
    if !info.is_link {
        return Some(dentry);
    }

    let mut target_buf = [0u8; 512];
    let mut rcnt: usize = 0;
    let c_path = alloc::ffi::CString::new(dentry.fs_path.as_bytes()).ok()?;
    let rc = unsafe {
        lwext4_rust::bindings::ext4_readlink(
            c_path.as_ptr() as *const core::ffi::c_char,
            target_buf.as_mut_ptr() as *mut core::ffi::c_char,
            target_buf.len(),
            &mut rcnt,
        )
    };
    if rc != 0 || rcnt == 0 {
        return None;
    }
    let target = core::str::from_utf8(&target_buf[..rcnt])
        .ok()?
        .trim_matches('\0');
    if target.is_empty() {
        return None;
    }

    let base = if target.starts_with('/') {
        crate::task::current_task()
            .map(|task| task.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone())
    } else {
        dentry.parent.clone().unwrap_or_else(|| start.clone())
    };
    let next = vfs_resolve_with_fallback(&base, target)?;
    follow_symlink(&base, next, depth + 1)
}

// 创建目录；如果目录已经存在就按成功处理，贴近 mkdir 的常见使用方式。
// 执行时先检查目标是否已是目录；不存在时解析父目录和名字，再调用 vfs_create 创建目录。
pub fn mkdir_at(base: Option<Arc<Dentry>>, path: &str) -> bool {
    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());

    if let Some(existing) = vfs_resolve_path(&start, path) {
        return existing.is_dir;
    }

    let (parent, name) = match vfs_resolve_parent_and_name(&start, path) {
        Some(x) => x,
        None => return false,
    };

    root_make_accessible(&parent, 0o777);
    vfs_create(&parent, &name, true, 0o777).is_some()
}

// 删除文件或目录，并同步清理父目录的 dentry 缓存。
// 执行时先解析目标并确认文件/目录类型匹配，再调用底层 unlink，成功后从父目录缓存移除该 dentry。
pub fn unlink_at(base: Option<Arc<Dentry>>, path: &str, is_dir: bool) -> bool {
    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let dentry = match vfs_resolve_path(&start, path) {
        Some(d) => d,
        None => return false,
    };

    if dentry.is_dir != is_dir {
        return false;
    }

    let parent = match dentry.parent.clone() {
        Some(p) => p,
        None => return false,
    };

    root_make_accessible(&parent, 0o777);
    root_make_accessible(&dentry, 0o777);

    let ok = if is_dir {
        dentry.super_block.unlink_dir(&dentry.fs_path)
    } else {
        dentry.super_block.unlink_file(&dentry.fs_path)
    };

    if ok {
        let mut children = parent.children.exclusive_access();
        if let Some(list) = children.as_mut() {
            list.retain(|c| c.name != dentry.name);
        }
    }
    ok
}

// ==================== File Trait for OSInode ====================

impl File for OSInode {
    // File trait 查询：这个打开实例是否允许读。
    // 执行时直接返回打开时保存的 readable 标志，不访问底层 ext4。
    fn readable(&self) -> bool {
        self.readable
    }

    // File trait 查询：这个打开实例是否允许写。
    // 执行时直接返回打开时保存的 writable 标志，不修改 inode 状态。
    fn writable(&self) -> bool {
        self.writable
    }

    // 顺序读文件，并推进当前 offset。
    // 执行时按当前 offset 读取；只读文件走 VFS read_at，可写文件懒打开 lwext4 句柄后循环 fread。
    fn read(&self, buf: &mut [u8]) -> isize {
        // 只读打开的文件直接走 VFS read_at，避免额外维护 lwext4 顺序句柄。
        if !self.writable {
            let mut inner = self.inner.exclusive_access();
            let rw_count = self
                .dentry
                .super_block
                .read_at(&self.dentry.fs_path, inner.offset, buf);
            inner.offset += rw_count;
            return rw_count as isize;
        }
        // 可写文件可能会复用底层顺序句柄，第一次读写时再懒打开。
        let need_open = {
            let inner = self.inner.exclusive_access();
            inner.file_handle.is_none()
        };
        if need_open {
            self.open_handle();
        }
        let mut inner = self.inner.exclusive_access();
        let off = inner.offset;
        if let Some(ref mut file) = inner.file_handle {
            // 读取前把 lwext4 句柄游标同步到内核记录的 offset。
            let size = unsafe { ext4_fsize(file) as usize };
            if off >= size {
                return 0;
            }
            let max_read = buf.len().min(size - off);
            unsafe {
                ext4_fseek(file, off as i64, SEEK_SET as u32);
            }
            let mut total = 0usize;
            while total < max_read {
                let mut rw_count: usize = 0;
                let rc = unsafe {
                    ext4_fread(
                        file,
                        buf[total..max_read].as_mut_ptr() as *mut core::ffi::c_void,
                        max_read - total,
                        &mut rw_count,
                    )
                };
                if rc != EOK as i32 || rw_count == 0 {
                    break;
                }
                total += rw_count;
            }
            inner.offset = off + total;
            return total as isize;
        }
        -1
    }

    // 从指定偏移读取，不改变当前 offset。
    // 执行时按指定 offset 读取；可写文件走 lwext4 句柄定位读取，只读文件走 VFS read_at。
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        if !self.writable {
            return self
                .dentry
                .super_block
                .read_at(&self.dentry.fs_path, offset, buf) as isize;
        }
        let need_open = {
            let inner = self.inner.exclusive_access();
            inner.file_handle.is_none()
        };
        if need_open {
            self.open_handle();
        }
        let mut inner = self.inner.exclusive_access();
        if let Some(ref mut file) = inner.file_handle {
            let size = unsafe { ext4_fsize(file) as usize };
            if offset >= size {
                return 0;
            }
            let max_read = buf.len().min(size - offset);
            unsafe {
                ext4_fseek(file, offset as i64, SEEK_SET as u32);
            }
            let mut total = 0usize;
            while total < max_read {
                let mut rw_count: usize = 0;
                let rc = unsafe {
                    ext4_fread(
                        file,
                        buf[total..max_read].as_mut_ptr() as *mut core::ffi::c_void,
                        max_read - total,
                        &mut rw_count,
                    )
                };
                if rc != EOK as i32 || rw_count == 0 {
                    break;
                }
                total += rw_count;
            }
            return total as isize;
        }
        -1
    }

    // 从指定偏移写入，不改变当前 offset；缓存的 size 会随写入结果更新。
    // 执行时先把 lwext4 句柄 seek 到目标 offset 循环 fwrite，写完恢复原 offset 并更新 stat 缓存。
    fn write_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        if !self.writable {
            return -1;
        }
        // 定点写不应该改变当前 offset，所以写完后要恢复底层句柄游标。
        let need_open = {
            let inner = self.inner.exclusive_access();
            inner.file_handle.is_none()
        };
        if need_open {
            self.open_handle();
        }
        let mut inner = self.inner.exclusive_access();
        let saved_offset = inner.offset;
        if let Some(ref mut file) = inner.file_handle {
            unsafe {
                ext4_fseek(file, offset as i64, SEEK_SET as u32);
            }
            let mut total = 0usize;
            while total < buf.len() {
                let mut rw_count: usize = 0;
                let rc = unsafe {
                    ext4_fwrite(
                        file,
                        buf[total..].as_ptr() as *const core::ffi::c_void,
                        buf.len() - total,
                        &mut rw_count,
                    )
                };
                if rc != EOK as i32 || rw_count == 0 {
                    break;
                }
                total += rw_count;
            }
            unsafe {
                ext4_fseek(file, saved_offset as i64, SEEK_SET as u32);
            }
            let new_size = unsafe { ext4_fsize(file) as i64 };
            if let Some(ref mut st) = inner.cached_stat {
                st.st_size = new_size;
                st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
            }
            return total as isize;
        }
        // 如果底层句柄没有成功建立，就退回 VFS 的路径写接口。
        drop(inner);
        let written = self
            .dentry
            .super_block
            .write_at(&self.dentry.fs_path, offset, buf);
        if written > 0 {
            let new_size = (offset + written) as i64;
            self.update_cached_size(new_size);
        }
        written as isize
    }

    // 顺序写文件；append 模式每次都先跳到文件尾。
    // 执行时懒打开 lwext4 句柄，按 append 或当前 offset 定位后循环 fwrite，并推进 offset 与缓存大小。
    fn write(&self, buf: &mut [u8]) -> isize {
        // 顺序写需要底层句柄保存写入状态，append 模式还要每次重新取文件尾。
        let need_open = {
            let inner = self.inner.exclusive_access();
            inner.file_handle.is_none()
        };
        if need_open {
            self.open_handle();
        }
        let mut inner = self.inner.exclusive_access();
        let append = inner.append;
        let cur_offset = inner.offset;
        if let Some(ref mut file) = inner.file_handle {
            let off = if append {
                unsafe { ext4_fsize(file) as usize }
            } else {
                cur_offset
            };
            unsafe {
                ext4_fseek(file, off as i64, SEEK_SET as u32);
            }
            let mut total = 0usize;
            while total < buf.len() {
                let mut rw_count: usize = 0;
                let rc = unsafe {
                    ext4_fwrite(
                        file,
                        buf[total..].as_ptr() as *const core::ffi::c_void,
                        buf.len() - total,
                        &mut rw_count,
                    )
                };
                if rc != EOK as i32 || rw_count == 0 {
                    break;
                }
                total += rw_count;
            }
            let new_size = unsafe { ext4_fsize(file) as i64 };
            inner.offset = off + total;
            if let Some(ref mut st) = inner.cached_stat {
                st.st_size = new_size;
                st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
            }
            return total as isize;
        }
        let off = if inner.append {
            // 没有底层句柄时也要按 append 语义，从当前文件大小开始写。
            self.dentry
                .super_block
                .get_inode_info(&self.dentry.fs_path)
                .map(|i| i.size as usize)
                .unwrap_or(inner.offset)
        } else {
            inner.offset
        };
        drop(inner);
        let rw_count = self
            .dentry
            .super_block
            .write_at(&self.dentry.fs_path, off, buf);
        let mut inner = self.inner.exclusive_access();
        inner.offset = off + rw_count;
        if let Some(ref mut st) = inner.cached_stat {
            st.st_size = (off + rw_count) as i64;
            st.st_blocks = ((st.st_size.max(0) as u64) + 511) / 512;
        }
        rw_count as isize
    }

    // 返回当前文件的 stat 信息。
    // 执行时调用 stat_inner，优先使用缓存，必要时读取 ext4 inode 元数据。
    fn stat(&self) -> Kstat {
        self.stat_inner()
    }

    // 判断这个 inode 是否代表目录。
    // 执行时直接返回 dentry 记录的 is_dir 标志。
    fn is_dir(&self) -> bool {
        self.dentry.is_dir
    }

    // 返回 VFS 视角下的路径，给 /proc 或调试类接口使用。
    // 执行时 clone 一份 dentry.path 并包成 Some 返回。
    fn path(&self) -> Option<String> {
        Some(self.dentry.path.clone())
    }

    // 按 Linux getdents64 的布局把目录项写进用户缓冲区。
    // 执行时按当前 offset 遍历目录项，逐条写入 dirent64 记录，缓冲区放不下时保留新的 offset。
    fn getdents64(&self, buf: &mut [u8]) -> isize {
        const DIRENT64_FIXED_SIZE: usize = 8 + 8 + 2 + 1;
        let mut inner = self.inner.exclusive_access();

        let entries: Vec<DirEntry> = self
            .dentry
            .super_block
            .dir_get_entries(&self.dentry.fs_path);

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

    // 修改当前 offset，同时同步已经打开的 lwext4 文件句柄位置。
    // 执行时先更新 inner.offset；如果 file_handle 已经打开，再调用 ext4_fseek 同步 C 库句柄。
    fn seek(&self, offset: usize) {
        let mut inner = self.inner.exclusive_access();
        inner.offset = offset;
        if let Some(ref mut file) = inner.file_handle {
            unsafe {
                ext4_fseek(file, offset as i64, SEEK_SET as u32);
            }
        }
    }

    // 读取当前 offset。
    // 执行时独占 inner 并返回 offset 字段，不访问底层文件系统。
    fn get_offset(&self) -> usize {
        self.inner.exclusive_access().offset
    }

    // 把当前打开状态还原成用户态能看到的 open flags。
    // 执行时根据 readable/writable 和 append 状态拼出 O_LARGEFILE、O_RDWR/O_WRONLY、O_APPEND 等位。
    fn get_flags(&self) -> usize {
        let inner = self.inner.exclusive_access();
        let mut flags = 0x8000usize;
        if self.readable && self.writable {
            flags |= OpenFlags::RDWR.bits() as usize;
        } else if self.writable {
            flags |= OpenFlags::WRONLY.bits() as usize;
        }
        if inner.append {
            flags |= OpenFlags::APPEND.bits() as usize;
        }
        flags
    }

    // 目前只允许动态修改 append 标志。
    // 执行时只读取传入 flags 里的 O_APPEND 位，并写回 inner.append。
    fn set_flags(&self, flags: usize) -> bool {
        let mut inner = self.inner.exclusive_access();
        inner.append = (flags & OpenFlags::APPEND.bits() as usize) != 0;
        true
    }

    // 给 fd 层做 downcast 用，判断具体文件类型。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ==================== rename_at ====================

// 重命名文件或目录，成功后把旧父目录和新父目录的 dentry 缓存都清掉对应项。
// 执行时解析旧目标和新父目录，拼出两个 ext4 路径调用 ext4_frename，成功后清理两边父目录缓存。
pub fn rename_at(
    old_base: Option<Arc<Dentry>>,
    old_path: &str,
    new_base: Option<Arc<Dentry>>,
    new_path: &str,
) -> bool {
    use alloc::ffi::CString;
    use lwext4_rust::bindings::{ext4_frename, EOK};

    let start = old_base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let old_dentry = match vfs_resolve_path(&start, old_path) {
        Some(d) => d,
        None => return false,
    };

    let old_cpath = match CString::new(old_dentry.fs_path.as_bytes()) {
        Ok(s) => s,
        Err(_) => return false,
    };

    let new_start = new_base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let (new_parent, new_name) = match vfs_resolve_parent_and_name(&new_start, new_path) {
        Some(x) => x,
        None => return false,
    };

    let new_fs_path = if new_parent.fs_path == "/" {
        alloc::format!("/{}", new_name)
    } else if new_parent.fs_path.ends_with('/') {
        alloc::format!("{}{}", new_parent.fs_path, new_name)
    } else {
        alloc::format!("{}/{}", new_parent.fs_path, new_name)
    };

    let new_cpath = match CString::new(new_fs_path.as_bytes()) {
        Ok(s) => s,
        Err(_) => return false,
    };

    let rc = unsafe {
        ext4_frename(
            old_cpath.as_ptr() as *const core::ffi::c_char,
            new_cpath.as_ptr() as *const core::ffi::c_char,
        )
    };

    if rc == EOK as i32 {
        if let Some(old_parent) = old_dentry.parent.clone() {
            let mut children = old_parent.children.exclusive_access();
            if let Some(list) = children.as_mut() {
                list.retain(|c| c.name != old_dentry.name);
            }
        }
        let mut children = new_parent.children.exclusive_access();
        if let Some(list) = children.as_mut() {
            list.retain(|c| c.name != new_name);
        }
        true
    } else {
        false
    }
}

// ==================== symlink_at ====================

// 创建符号链接，target 原样写进链接内容，linkpath 走 VFS 解析父目录。
// 执行时解析 linkpath 的父目录，拼出新链接的 ext4 路径，再把 target 和 link path 传给 ext4_fsymlink。
pub fn symlink_at(base: Option<Arc<Dentry>>, target: &str, linkpath: &str) -> bool {
    use alloc::ffi::CString;
    use lwext4_rust::bindings::{ext4_fsymlink, EOK};

    let start = base.unwrap_or_else(|| crate::fs::vfs::ROOT_DENTRY.clone());
    let (parent, name) = match vfs_resolve_parent_and_name(&start, linkpath) {
        Some(x) => x,
        None => return false,
    };

    let link_fs_path = if parent.fs_path == "/" {
        alloc::format!("/{}", name)
    } else if parent.fs_path.ends_with('/') {
        alloc::format!("{}{}", parent.fs_path, name)
    } else {
        alloc::format!("{}/{}", parent.fs_path, name)
    };

    let link_cpath = match CString::new(link_fs_path.as_bytes()) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let target_cpath = match CString::new(target) {
        Ok(s) => s,
        Err(_) => return false,
    };

    let rc = unsafe {
        ext4_fsymlink(
            target_cpath.as_ptr() as *const core::ffi::c_char,
            link_cpath.as_ptr() as *const core::ffi::c_char,
        )
    };

    rc == EOK as i32
}
