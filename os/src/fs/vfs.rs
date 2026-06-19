// 这里负责操作系统的文件管理：实现 VFS 的目录项缓存、路径解析、挂载表和底层 ext4 操作。
use crate::sync::UPSafeCell;
use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_char;
use lazy_static::*;

use super::block_device_stream::{BlockDeviceStream, BlockDeviceStreamOp, Ext4Wrapper};
use lwext4_rust::bindings::*;
use lwext4_rust::Ext4BlockWrapper;

// ==================== DirEntry (local) ====================

pub struct DirEntry {
    pub inode: u32,
    name_bytes: Vec<u8>,
    pub de_type: u8,
}

impl DirEntry {
    // 把 ext4 目录项里的原始名字字节转成 Rust 字符串。
    // 执行时用 from_utf8_lossy 兼容非 UTF-8 字节，生成给 getdents64 使用的文件名。
    pub fn get_name(&self) -> String {
        String::from_utf8_lossy(&self.name_bytes).into_owned()
    }
    // 返回目录项类型，后面组装 getdents64 时会用到。
    // 执行时直接返回 ext4 目录项里记录的 de_type。
    pub fn get_de_type(&self) -> u8 {
        self.de_type
    }
}

// ==================== InodeInfo ====================

pub struct InodeInfo {
    pub inode_num: u32,
    pub is_dir: bool,
    pub is_link: bool,
    pub mode: u32,
    pub size: u64,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    pub blocks_count: u64,
    pub links_count: u32,
    pub uid: u32,
    pub gid: u32,
}

// ==================== VfsSuperBlock ====================

pub enum FsInner {
    Ext4(Ext4Wrapper),
}

pub struct VfsSuperBlock {
    pub fs_type: &'static str,
    pub fs_data: UPSafeCell<FsInner>,
}

impl VfsSuperBlock {
    // 在默认 lwext4 挂载名前创建一个 ext4 超级块。
    // 执行时把挂载名固定成 /mp/，再转给 new_ext4_at 走统一初始化流程。
    pub fn new_ext4(
        blk_device: Arc<dyn crate::drivers::BlockDevice>,
        fs_type: &'static str,
    ) -> Option<Arc<Self>> {
        Self::new_ext4_at(blk_device, fs_type, "/mp/")
    }

    // 用指定的 lwext4 挂载名前创建 ext4 超级块，测试盘挂载会走这里。
    // 执行时把块设备包装成 BlockDeviceStream，创建 lwext4 wrapper，再放进 VfsSuperBlock 的受控单元里。
    pub fn new_ext4_at(
        blk_device: Arc<dyn crate::drivers::BlockDevice>,
        fs_type: &'static str,
        mount_point: &str,
    ) -> Option<Arc<Self>> {
        const FALLBACK_DISK_SIZE: u64 = 512 * 1024 * 1024;
        let total_size = blk_device.capacity_bytes().unwrap_or(FALLBACK_DISK_SIZE);
        let stream = BlockDeviceStream {
            inner: blk_device,
            pos: 0,
            total_size,
        };
        let ext4 =
            Ext4BlockWrapper::<BlockDeviceStreamOp>::new_with_name(stream, "ext4_fs", mount_point)
                .ok()?;
        Some(Arc::new(VfsSuperBlock {
            fs_type,
            fs_data: unsafe {
                UPSafeCell::new_named(FsInner::Ext4(Ext4Wrapper(ext4)), "VfsFsData")
            },
        }))
    }

    // 独占访问底层文件系统对象，并把真正操作收束在闭包里。
    // 执行时先拿到 fs_data 的独占借用，再把 Ext4Wrapper 交给闭包，避免外层长期保存底层引用。
    fn with_fs<R>(&self, f: impl FnOnce(&Ext4Wrapper) -> R) -> R {
        let data = self.fs_data.exclusive_access();
        match &*data {
            FsInner::Ext4(wrapper) => f(wrapper),
        }
    }

    // 强制卸载 lwext4，主要给出错或关机场景兜底。
    // 执行时独占底层 wrapper 并调用 lwext4_umount；这是 FFI 资源释放点，所以必须在锁内完成。
    pub fn force_umount(&self) {
        let mut data = self.fs_data.exclusive_access();
        if let FsInner::Ext4(ref mut wrapper) = *data {
            unsafe {
                wrapper.0.lwext4_umount().ok();
            }
        }
    }

    // ---------- path-based ext4 operations ----------

    // 读取目录下的 ext4 目录项，最多取一批防止坏盘拖死内核。
    // 执行时把路径转成 C 字符串，用 ext4_dir_open/entry_next 遍历目录，收集完后关闭目录句柄。
    pub fn dir_get_entries(&self, fs_path: &str) -> Vec<DirEntry> {
        const MAX_DIR_ENTRIES: usize = 4096;
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let mut d: ext4_dir = unsafe { core::mem::zeroed() };
            let mut entries = Vec::new();

            unsafe {
                let rc = ext4_dir_open(&mut d, c_path.as_ptr() as *const c_char);
                if rc != EOK as i32 {
                    return entries;
                }
                loop {
                    if entries.len() >= MAX_DIR_ENTRIES {
                        break;
                    }
                    let de = ext4_dir_entry_next(&mut d);
                    if de.is_null() {
                        break;
                    }
                    let dentry = &*de;
                    let name_len = (dentry.name_length as usize).min(dentry.name.len());
                    let name_bytes = dentry.name[..name_len].to_vec();
                    entries.push(DirEntry {
                        inode: dentry.inode,
                        name_bytes,
                        de_type: dentry.inode_type,
                    });
                }
                ext4_dir_close(&mut d);
            }
            entries
        })
    }

    // 从 ext4 查询 inode 的模式、大小、时间戳和属主信息。
    // 执行时先读取 mode/owner/time，再按目录、普通文件、符号链接分别打开对应句柄取 inode 和大小。
    pub fn get_inode_info(&self, fs_path: &str) -> Option<InodeInfo> {
        self.with_fs(|_fs| {
            let mut mode: u32 = 0;
            let c_path2 = CString::new(fs_path).ok()?;
            let _mode_rc = unsafe { ext4_mode_get(c_path2.as_ptr() as *const c_char, &mut mode) };

            let mut uid: u32 = 0;
            let mut gid: u32 = 0;
            let c_path_owner = CString::new(fs_path).ok()?;
            unsafe {
                ext4_owner_get(c_path_owner.as_ptr() as *const c_char, &mut uid, &mut gid);
            }

            let mut atime: u32 = 0;
            let mut mtime: u32 = 0;
            let mut ctime: u32 = 0;
            let c_path3 = CString::new(fs_path).ok()?;
            unsafe {
                ext4_atime_get(c_path3.as_ptr() as *const c_char, &mut atime);
                ext4_mtime_get(c_path3.as_ptr() as *const c_char, &mut mtime);
                ext4_ctime_get(c_path3.as_ptr() as *const c_char, &mut ctime);
            }

            let file_type = mode & 0o170000;
            let is_link = file_type == 0o120000;
            let is_dir_mode = file_type == 0o040000;

            let (inode_num, size, is_dir) = if is_dir_mode {
                let mut dir: ext4_dir = unsafe { core::mem::zeroed() };
                let c_path_dir = CString::new(fs_path).ok()?;
                let dir_rc =
                    unsafe { ext4_dir_open(&mut dir, c_path_dir.as_ptr() as *const c_char) };
                if dir_rc == EOK as i32 {
                    let inode_num = dir.f.inode;
                    let size = dir.f.fsize;
                    unsafe {
                        ext4_dir_close(&mut dir);
                    }
                    mode = (mode & 0o7777) | 0o040000;
                    (inode_num, size, true)
                } else {
                    return None;
                }
            } else {
                let mut file: ext4_file = unsafe { core::mem::zeroed() };
                let c_path_file = CString::new(fs_path).ok()?;
                let flags = CString::new("rb").ok()?;
                let file_rc = unsafe {
                    ext4_fopen(
                        &mut file,
                        c_path_file.as_ptr() as *const c_char,
                        flags.as_ptr() as *const c_char,
                    )
                };

                if file_rc == EOK as i32 {
                    let inode_num = file.inode;
                    let size = unsafe { ext4_fsize(&mut file) };
                    unsafe {
                        ext4_fclose(&mut file);
                    }
                    (inode_num, size, false)
                } else if is_link {
                    // 符号链接只需要确认存在和记录路径，不把链接内容当普通文件大小处理。
                    (0, 0, false)
                } else {
                    let mut dir: ext4_dir = unsafe { core::mem::zeroed() };
                    let c_path_dir = CString::new(fs_path).ok()?;
                    let dir_rc =
                        unsafe { ext4_dir_open(&mut dir, c_path_dir.as_ptr() as *const c_char) };
                    if dir_rc != EOK as i32 {
                        return None;
                    }
                    let inode_num = dir.f.inode;
                    let size = dir.f.fsize;
                    unsafe {
                        ext4_dir_close(&mut dir);
                    }
                    mode = (mode & 0o7777) | 0o040000;
                    (inode_num, size, true)
                }
            };

            Some(InodeInfo {
                inode_num,
                is_dir,
                is_link,
                mode,
                size,
                atime,
                mtime,
                ctime,
                blocks_count: 0,
                links_count: 0,
                uid,
                gid,
            })
        })
    }

    // 按路径打开文件并从指定偏移读取，读完立即关闭底层句柄。
    // 执行时把路径和 rb 标志转成 C 字符串，fopen 后检查文件大小，再 seek 到 offset 循环 fread。
    pub fn read_at(&self, fs_path: &str, offset: usize, buf: &mut [u8]) -> usize {
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return 0,
            };
            let c_path = c_path.into_raw();
            let flags = CString::new("rb")
                .ok()
                .unwrap_or_else(|| CString::new("").unwrap());
            let flags_ptr = flags.into_raw();

            let mut file: ext4_file = unsafe { core::mem::zeroed() };
            let rc = unsafe { ext4_fopen(&mut file, c_path, flags_ptr) };
            unsafe {
                drop(CString::from_raw(c_path));
                drop(CString::from_raw(flags_ptr));
            }
            if rc != EOK as i32 {
                return 0;
            }

            let file_size = unsafe { ext4_fsize(&mut file) as usize };
            if offset >= file_size {
                unsafe {
                    ext4_fclose(&mut file);
                }
                return 0;
            }
            let max_read = core::cmp::min(buf.len(), file_size - offset);

            unsafe {
                ext4_fseek(&mut file, offset as i64, SEEK_SET as u32);
            }

            let mut total = 0usize;
            while total < max_read {
                let chunk_len = core::cmp::min(128, max_read - total);
                let mut rw_count: usize = 0;
                let read_rc = unsafe {
                    ext4_fread(
                        &mut file,
                        buf[total..total + chunk_len].as_mut_ptr() as *mut core::ffi::c_void,
                        chunk_len,
                        &mut rw_count,
                    )
                };
                if read_rc != EOK as i32 || rw_count == 0 {
                    break;
                }
                total += rw_count;
                if rw_count < chunk_len {
                    break;
                }
            }
            unsafe {
                ext4_fclose(&mut file);
            }

            total
        })
    }

    // 对外暴露的路径写入口，实际写入逻辑放到 raw 版本里。
    // 执行时不自己处理 FFI，只把参数转给 write_at_raw，保留一个稳定的 VFS 写入口。
    pub fn write_at(&self, fs_path: &str, offset: usize, buf: &[u8]) -> usize {
        self.write_at_raw(fs_path, offset, buf)
    }

    // 按路径打开文件并从指定偏移写入，写完立即关闭底层句柄。
    // 执行时把路径和 r+ 标志转成 C 字符串，fopen 后 seek 到 offset，调用 fwrite 后关闭句柄。
    fn write_at_raw(&self, fs_path: &str, offset: usize, buf: &[u8]) -> usize {
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return 0,
            };
            let c_path = c_path.into_raw();
            let flags = CString::new("r+")
                .ok()
                .unwrap_or_else(|| CString::new("").unwrap());
            let flags_ptr = flags.into_raw();

            let mut file: ext4_file = unsafe { core::mem::zeroed() };
            let rc = unsafe { ext4_fopen(&mut file, c_path, flags_ptr) };
            unsafe {
                drop(CString::from_raw(c_path));
                drop(CString::from_raw(flags_ptr));
            }
            if rc != EOK as i32 {
                return 0;
            }

            unsafe {
                ext4_fseek(&mut file, offset as i64, SEEK_SET as u32);
            }

            let mut rw_count: usize = 0;
            let write_rc = unsafe {
                ext4_fwrite(
                    &mut file,
                    buf.as_ptr() as *const core::ffi::c_void,
                    buf.len(),
                    &mut rw_count,
                )
            };
            unsafe {
                ext4_fclose(&mut file);
            }

            if write_rc != EOK as i32 {
                return 0;
            }
            rw_count
        })
    }

    // 在指定父目录下创建普通文件或目录，返回新 inode 号。
    // 执行时先拼出完整 ext4 路径；目录走 ext4_dir_mk，普通文件走 ext4_fopen(\"wb\") 并取 inode。
    pub fn create_file(&self, parent_path: &str, name: &str, is_dir: bool) -> Option<u32> {
        let full_path = if parent_path == "/" {
            format!("/{}", name)
        } else if parent_path.ends_with('/') {
            format!("{}{}", parent_path, name)
        } else {
            format!("{}/{}", parent_path, name)
        };

        self.with_fs(|_fs| {
            if is_dir {
                let c_path = CString::new(full_path.as_bytes()).ok()?;
                let rc = unsafe { ext4_dir_mk(c_path.as_ptr() as *const c_char) };
                if rc == EOK as i32 {
                    return Some(0);
                }
                None
            } else {
                let c_path = CString::new(full_path.as_bytes()).ok()?;
                let c_path = c_path.into_raw();
                let flags = CString::new("wb").ok()?;
                let flags_ptr = flags.into_raw();

                let mut file: ext4_file = unsafe { core::mem::zeroed() };
                let rc = unsafe { ext4_fopen(&mut file, c_path, flags_ptr) };
                unsafe {
                    drop(CString::from_raw(c_path));
                    drop(CString::from_raw(flags_ptr));
                }
                if rc == EOK as i32 {
                    let inode = file.inode;
                    unsafe {
                        ext4_fclose(&mut file);
                    }
                    Some(inode)
                } else {
                    None
                }
            }
        })
    }

    // 把普通文件截断到指定大小。
    // 执行时以 r+ 打开目标文件，调用 ext4_ftruncate 设置大小，最后关闭句柄并返回是否成功。
    pub fn truncate_file(&self, fs_path: &str, size: u64) -> bool {
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return false,
            };
            let c_path = c_path.into_raw();
            let flags = CString::new("r+")
                .ok()
                .unwrap_or_else(|| CString::new("").unwrap());
            let flags_ptr = flags.into_raw();

            let mut file: ext4_file = unsafe { core::mem::zeroed() };
            let rc = unsafe { ext4_fopen(&mut file, c_path, flags_ptr) };
            unsafe {
                drop(CString::from_raw(c_path));
                drop(CString::from_raw(flags_ptr));
            }
            if rc != EOK as i32 {
                return false;
            }

            let ok = unsafe { ext4_ftruncate(&mut file, size) } == EOK as i32;
            unsafe {
                ext4_fclose(&mut file);
            }
            ok
        })
    }

    // 删除普通文件或符号链接。
    // 执行时把路径转成 C 字符串并调用 ext4_fremove，返回底层删除是否成功。
    pub fn unlink_file(&self, fs_path: &str) -> bool {
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return false,
            };
            unsafe { ext4_fremove(c_path.as_ptr() as *const c_char) == EOK as i32 }
        })
    }

    // 按底层 ext4 路径删除目录；调用者负责确认目标确实应该按目录删除。
    // 执行时把路径转成 C 字符串并调用 ext4_dir_rm，返回底层目录删除是否成功。
    pub fn unlink_dir(&self, fs_path: &str) -> bool {
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return false,
            };
            unsafe { ext4_dir_rm(c_path.as_ptr() as *const c_char) == EOK as i32 }
        })
    }

    // 更新文件访问时间。
    // 执行时把路径转成 C 字符串，成功后调用 ext4_atime_set 写入 atime。
    pub fn set_atime(&self, fs_path: &str, atime: u32) {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe {
                    ext4_atime_set(c_path.as_ptr() as *const c_char, atime);
                }
            }
        })
    }

    // 更新文件修改时间。
    // 执行时把路径转成 C 字符串，成功后调用 ext4_mtime_set 写入 mtime。
    pub fn set_mtime(&self, fs_path: &str, mtime: u32) {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe {
                    ext4_mtime_set(c_path.as_ptr() as *const c_char, mtime);
                }
            }
        })
    }

    // 更新 inode 状态变化时间。
    // 执行时把路径转成 C 字符串，成功后调用 ext4_ctime_set 写入 ctime。
    pub fn set_ctime(&self, fs_path: &str, ctime: u32) {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe {
                    ext4_ctime_set(c_path.as_ptr() as *const c_char, ctime);
                }
            }
        })
    }

    // 写回文件类型和权限位。
    // 执行时把路径转成 C 字符串，调用 ext4_mode_set 写入 mode，并把底层返回码传回调用者。
    pub fn set_mode(&self, fs_path: &str, mode: u32) -> i32 {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                let r = unsafe { ext4_mode_set(c_path.as_ptr() as *const c_char, mode) };
                r
            } else {
                -1
            }
        })
    }

    // 写回文件属主和属组。
    // 执行时把路径转成 C 字符串，调用 ext4_owner_set 写入 uid/gid，并返回底层结果。
    pub fn set_owner(&self, fs_path: &str, uid: u32, gid: u32) -> i32 {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                let r = unsafe { ext4_owner_set(c_path.as_ptr() as *const c_char, uid, gid) };
                r
            } else {
                -1
            }
        })
    }

    // 刷新指定路径相关的 ext4 缓存。
    // 执行时把路径转成 C 字符串，调用 ext4_cache_flush 刷新该路径相关缓存。
    pub fn fsync(&self, fs_path: &str) -> i32 {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe { ext4_cache_flush(c_path.as_ptr() as *const c_char) }
            } else {
                -1
            }
        })
    }

    // 刷新整个文件系统缓存。
    // 执行时用根路径调用 ext4_cache_flush，让 lwext4 尽量把全局缓存写回块设备。
    pub fn sync_all(&self) -> i32 {
        self.with_fs(|_fs| {
            let root = CString::new("/").unwrap();
            unsafe { ext4_cache_flush(root.as_ptr() as *const c_char) }
        })
    }

    // 读取 lwext4 当前挂载统计。
    // 执行时进入 with_fs 闭包，直接从 Ext4BlockWrapper 读取 mount stats。
    pub fn get_fs_stats(&self) -> ext4_mount_stats {
        self.with_fs(|wrapper| wrapper.0.get_mount_stats())
    }
}

// ==================== Dentry ====================

pub struct Dentry {
    pub name: String,
    pub path: String,
    pub fs_path: String,
    pub inode_num: u32,
    pub is_dir: bool,
    pub is_link: bool,
    pub parent: Option<Arc<Dentry>>,
    pub super_block: Arc<VfsSuperBlock>,
    pub children: UPSafeCell<Option<Vec<Arc<Dentry>>>>,
}

impl Dentry {
    // 创建根目录 dentry。
    // 执行时把 VFS 路径和底层路径都设为 /，inode 号固定为根目录 inode 2，并初始化空 children 缓存。
    fn new_root(sb: Arc<VfsSuperBlock>) -> Arc<Self> {
        Arc::new(Dentry {
            name: String::from("/"),
            path: String::from("/"),
            fs_path: String::from("/"),
            inode_num: 2,
            is_dir: true,
            is_link: false,
            parent: None,
            super_block: sb,
            children: unsafe { UPSafeCell::new(None) },
        })
    }

    // 创建挂载点背后的新根 dentry，VFS 路径和底层 fs 路径分开记录。
    // 执行时把 VFS 路径设为挂载点路径，把 fs_path 设为 lwext4 挂载名，作为新文件系统的根 dentry。
    fn new_mount_root(sb: Arc<VfsSuperBlock>, mnt_path: String, lwext4_mount: &str) -> Arc<Self> {
        Arc::new(Dentry {
            name: String::from("/"),
            path: mnt_path,
            fs_path: String::from(lwext4_mount),
            inode_num: 2,
            is_dir: true,
            is_link: false,
            parent: None,
            super_block: sb,
            children: unsafe { UPSafeCell::new(None) },
        })
    }

    // 按父目录和名字创建子 dentry，同时拼好 VFS 路径和底层路径。
    // 执行时根据父目录分别拼 VFS path 和底层 fs_path，并继承父目录的超级块和缓存结构。
    fn new_child(
        parent: &Arc<Dentry>,
        name: &str,
        inode_num: u32,
        is_dir: bool,
        is_link: bool,
    ) -> Arc<Self> {
        let path = if parent.path == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", parent.path, name)
        };
        let fs_path = if parent.fs_path == "/" {
            format!("/{}", name)
        } else if parent.fs_path.ends_with('/') {
            format!("{}{}", parent.fs_path, name)
        } else {
            format!("{}/{}", parent.fs_path, name)
        };
        Arc::new(Dentry {
            name: String::from(name),
            path,
            fs_path,
            inode_num,
            is_dir,
            is_link,
            parent: Some(parent.clone()),
            super_block: parent.super_block.clone(),
            children: unsafe { UPSafeCell::new(None) },
        })
    }
}

// ==================== Mount ====================

pub struct Mount {
    pub mnt_point: Arc<Dentry>,
    pub mnt_root: Arc<Dentry>,
}

lazy_static! {
    pub static ref MOUNT_TABLE: UPSafeCell<Vec<Mount>> = unsafe { UPSafeCell::new(Vec::new()) };
}

// 如果这个 dentry 是挂载点，就切到挂载后的根目录。
// 执行时遍历挂载表，按指针或路径匹配挂载点，命中就返回挂载后的根 dentry。
fn lookup_mount(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    let table = MOUNT_TABLE.exclusive_access();
    for m in table.iter() {
        if Arc::ptr_eq(&m.mnt_point, dentry) || m.mnt_point.path == dentry.path {
            return Some(m.mnt_root.clone());
        }
    }
    None
}

// 如果这个 dentry 是某个挂载文件系统的根，返回它挂到的原始挂载点。
// 执行时遍历挂载表，按指针或路径匹配 mnt_root，命中就返回原始挂载点 dentry。
fn is_mount_root(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    let table = MOUNT_TABLE.exclusive_access();
    for m in table.iter() {
        if Arc::ptr_eq(&m.mnt_root, dentry) || m.mnt_root.path == dentry.path {
            return Some(m.mnt_point.clone());
        }
    }
    None
}

// ==================== Path Resolution ====================

// 取父目录，遇到挂载根时要先跳回挂载点再往上走。
// 执行时先判断当前 dentry 是否是挂载根，是就返回挂载点的父目录，否则直接返回自己的 parent。
fn vfs_parent(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    if let Some(mnt_point) = is_mount_root(dentry) {
        return mnt_point.parent.clone();
    }
    dentry.parent.clone()
}

// 在父目录下查找一个名字，优先走 dentry 缓存，查不到再问底层 ext4。
// 执行时先处理 . 和 ..，再查 children 缓存；缓存未命中时查询 ext4 inode 并把新 dentry 放回缓存。
pub fn vfs_lookup(parent: &Arc<Dentry>, name: &str) -> Option<Arc<Dentry>> {
    // "." 和 ".." 不需要访问底层文件系统；处理 ".." 时还要守住当前任务的 root 边界。
    match name {
        "." => return Some(parent.clone()),
        ".." => {
            if let Some(task) = crate::task::current_task() {
                let root = task.inner_exclusive_access().root.clone();
                if Arc::ptr_eq(parent, &root) || parent.path == root.path {
                    return Some(parent.clone());
                }
            }
            return vfs_parent(parent).or_else(|| Some(parent.clone()));
        }
        _ => {}
    }

    // 先查父目录的 dentry 缓存，命中后还要检查它是否被挂载点覆盖。
    {
        let children = parent.children.exclusive_access();
        if let Some(list) = children.as_ref() {
            if let Some(cached) = list.iter().find(|c| c.name == name) {
                if let Some(mnt_root) = lookup_mount(cached) {
                    return Some(mnt_root);
                }
                return Some(cached.clone());
            }
        }
    }

    // 缓存未命中时拼出底层 ext4 路径，再查询 inode 信息。
    let child_fs_path = if parent.fs_path == "/" {
        format!("/{}", name)
    } else if parent.fs_path.ends_with('/') {
        format!("{}{}", parent.fs_path, name)
    } else {
        format!("{}/{}", parent.fs_path, name)
    };

    let info = parent.super_block.get_inode_info(&child_fs_path)?;
    let (child_inode, is_dir, is_link) = (info.inode_num, info.is_dir, info.is_link);

    let child = Dentry::new_child(parent, name, child_inode, is_dir, is_link);

    // 新查到的 dentry 写回父目录缓存，后续路径解析就不用再访问 ext4。
    {
        let mut children = parent.children.exclusive_access();
        if let Some(list) = children.as_mut() {
            list.push(child.clone());
        } else {
            *children = Some(vec![child.clone()]);
        }
    }

    if let Some(mnt_root) = lookup_mount(&child) {
        return Some(mnt_root);
    }

    Some(child)
}

// 从给定起点解析绝对或相对路径。
// 执行时先确定绝对路径或相对路径起点，再逐段调用 vfs_lookup，最后处理挂载点覆盖。
pub fn vfs_resolve_path(start: &Arc<Dentry>, path: &str) -> Option<Arc<Dentry>> {
    let path = path.trim_matches('\0');
    if path.is_empty() {
        return None;
    }

    // 绝对路径从当前任务的 root 开始解析，这样 chroot 后不会越过新根。
    let is_abs = path.starts_with('/');
    let mut current = if is_abs {
        crate::task::current_task()
            .map(|t| t.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| ROOT_DENTRY.clone())
    } else {
        start.clone()
    };

    // 按路径分量逐段查找，每一段都交给 vfs_lookup 处理缓存、挂载点和父目录。
    for component in path.split('/') {
        match component {
            "" | "." => continue,
            _ => {
                current = vfs_lookup(&current, component)?;
            }
        }
    }

    lookup_mount(&current).or(Some(current))
}

// 解析路径；普通命令名找不到时再尝试 /bin/<name>。
// 执行时先按原路径解析；若是裸命令名且找不到，再拼成 /bin/<name> 解析一次。
pub fn vfs_resolve_with_fallback(start: &Arc<Dentry>, path: &str) -> Option<Arc<Dentry>> {
    if let Some(d) = vfs_resolve_path(start, path) {
        return Some(d);
    }
    let bare = path.trim_matches('\0').trim_start_matches("./");
    if !bare.starts_with('/') && !bare.contains('/') && bare != "." && bare != ".." {
        let bin_path = format!("/bin/{}", bare);
        if let Some(d) = vfs_resolve_path(start, &bin_path) {
            return Some(d);
        }
    }
    None
}

// 把路径拆成已存在的父目录 dentry 和最后一个文件名。
// 执行时先按绝对/相对路径确定起点，再解析除最后一段外的父目录，最后返回父目录和文件名。
pub fn vfs_resolve_parent_and_name(
    start: &Arc<Dentry>,
    path: &str,
) -> Option<(Arc<Dentry>, String)> {
    let path = path.trim_matches('\0');
    if path.is_empty() {
        return None;
    }

    let is_abs = path.starts_with('/');
    let mut current = if is_abs {
        crate::task::current_task()
            .map(|t| t.inner_exclusive_access().root.clone())
            .unwrap_or_else(|| ROOT_DENTRY.clone())
    } else {
        start.clone()
    };

    let components: Vec<&str> = path
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    if components.is_empty() {
        return None;
    }

    let filename = String::from(*components.last().unwrap());
    for i in 0..components.len() - 1 {
        current = vfs_lookup(&current, components[i])?;
    }

    Some((current, filename))
}

// ==================== File Creation ====================

// 在 VFS 中创建文件或目录，并同步更新父目录的 dentry 缓存。
// 执行时先让底层 ext4 创建 inode，再创建子 dentry、应用 umask 权限，并插入父目录缓存。
pub fn vfs_create(
    parent: &Arc<Dentry>,
    name: &str,
    is_dir: bool,
    mode: u32,
) -> Option<Arc<Dentry>> {
    // 先让底层文件系统真正创建 inode，VFS dentry 只是它的缓存视图。
    let inode_num = parent
        .super_block
        .create_file(&parent.fs_path, name, is_dir)?;

    let child = Dentry::new_child(parent, name, inode_num, is_dir, false);

    // 创建权限要应用当前进程 umask，再写回到底层 ext4 inode。
    let umask = crate::task::current_task()
        .map(|t| t.inner_exclusive_access().umask)
        .unwrap_or(0o22);
    let file_type = if is_dir { 0o040000 } else { 0o100000 };
    let effective_mode = file_type | (mode & !umask & 0o7777);
    let fs_path = child.fs_path.clone();
    child.super_block.set_mode(&fs_path, effective_mode);

    // 父目录缓存同步插入新 dentry，避免刚创建后马上查找失败。
    {
        let mut children = parent.children.exclusive_access();
        if let Some(list) = children.as_mut() {
            list.push(child.clone());
        } else {
            *children = Some(vec![child.clone()]);
        }
    }

    Some(child)
}

// ==================== Mount Operations ====================

// 把一个超级块挂到指定 dentry 上。
// 执行时先检查非根挂载点是否已被挂载，再创建挂载根 dentry 并追加到 MOUNT_TABLE。
pub fn vfs_mount(mnt_point: Arc<Dentry>, sb: Arc<VfsSuperBlock>, fs_path: &str) -> bool {
    let path = mnt_point.path.clone();
    {
        let table = MOUNT_TABLE.exclusive_access();
        if table
            .iter()
            .any(|m| m.mnt_point.path == path && path != "/")
        {
            return false;
        }
    }

    let mnt_root = Dentry::new_mount_root(sb, mnt_point.path.clone(), fs_path);

    MOUNT_TABLE.exclusive_access().push(Mount {
        mnt_point: mnt_point.clone(),
        mnt_root: mnt_root.clone(),
    });

    true
}

// 从挂载表里卸掉指定挂载点。
// 执行时按挂载点路径在 MOUNT_TABLE 中查找记录，找到就删除并返回 true。
pub fn vfs_umount(mnt_point: &Arc<Dentry>) -> bool {
    let path = &mnt_point.path;
    let mut table = MOUNT_TABLE.exclusive_access();
    if let Some(idx) = table.iter().position(|m| &m.mnt_point.path == path) {
        table.remove(idx);
        true
    } else {
        false
    }
}

// ==================== Root Filesystem ====================

use crate::drivers::{BLOCK_DEVICE, ROOT_BLOCK_DEVICE};

lazy_static! {
    pub static ref ROOT_SB: Arc<VfsSuperBlock> = {
        VfsSuperBlock::new_ext4(ROOT_BLOCK_DEVICE.clone(), "ext4")
            .expect("Failed to mount root ext4 filesystem")
    };
    pub static ref ROOT_DENTRY: Arc<Dentry> = Dentry::new_root(ROOT_SB.clone());
}

// 初始化根文件系统挂载表。
// 执行时把 ROOT_DENTRY 同时作为挂载点和挂载根插入 MOUNT_TABLE。
pub fn mount_root() {
    let root = ROOT_DENTRY.clone();
    MOUNT_TABLE.exclusive_access().push(Mount {
        mnt_point: root.clone(),
        mnt_root: root,
    });
}

// 把测试盘挂到指定目录，比赛的双盘环境会用到。
// 执行时用测试块设备创建 ext4 超级块，成功后把它挂到调用者传入的 dentry 上。
pub fn mount_test_disk_at(mnt_point: Arc<Dentry>) -> bool {
    let test_sb = match VfsSuperBlock::new_ext4(BLOCK_DEVICE.clone(), "ext4") {
        Some(sb) => sb,
        None => return false,
    };
    vfs_mount(mnt_point, test_sb, "/__mount_vda/")
}

// 打印 /bin 下的用户程序列表，启动时调试用。
// 执行时读取根文件系统 /bin 目录项，过滤 .、.. 和空名后逐行打印。
pub fn list_apps() {
    println!("/**** APPS ****");
    ROOT_SB
        .dir_get_entries("/bin")
        .iter()
        .filter(|e| {
            let name = e.get_name();
            name != "." && name != ".." && !name.is_empty()
        })
        .for_each(|e| println!("{}", e.get_name()));
    println!("**************/");
}
