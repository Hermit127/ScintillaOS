use crate::sync::{DentryLock, FsAuxLock, FsGlobalGuard, FsGlobalLock, FsGuard, FsLock, VfsLock};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ffi::c_char;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "mount-watchdog")]
use core::sync::atomic::{AtomicBool, AtomicUsize};
use lazy_static::*;

use super::block_device_stream::{BlockDeviceStream, BlockDeviceStreamOp, Ext4Wrapper};
use lwext4_rust::bindings::*;
use lwext4_rust::Ext4BlockWrapper;

const PATH_OWNER_CACHE_MAX_ENTRIES: usize = 1024;
const ROOT_LWEXT4_MOUNT: &str = "/__rootfs/";
const ROOT_LWEXT4_BIN: &str = "/__rootfs/bin";
// The first SMP release has no mount hotplug requirement.  Reserving the
// small mount table up front keeps Vec growth (and therefore allocation) out
// of VfsLock critical sections.
const MOUNT_TABLE_CAPACITY: usize = 16;

lazy_static! {
    // lwext4 keeps process-global mount/cache state internally.  Per-volume
    // locks are insufficient in the dual-disk SMP configuration: operations
    // on rootfs and the test disk must not enter lwext4 concurrently.
    static ref LWEXT4_GLOBAL: FsGlobalLock<()> =
        FsGlobalLock::new_named((), "LwExt4Global");
    static ref PATH_OWNER_CACHE: VfsLock<BTreeMap<String, (u32, u32)>> =
        VfsLock::new_named(BTreeMap::new(), "VfsPathOwnerCache");
}

#[cfg(feature = "mount-watchdog")]
static MOUNT_WATCHDOG_STARTED_US: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "mount-watchdog")]
static MOUNT_WATCHDOG_STAGE: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "mount-watchdog")]
static MOUNT_WATCHDOG_REPORTED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "mount-watchdog")]
pub(crate) fn mount_watchdog_begin() {
    MOUNT_WATCHDOG_STAGE.store(1, Ordering::Release);
    MOUNT_WATCHDOG_REPORTED.store(false, Ordering::Release);
    MOUNT_WATCHDOG_STARTED_US.store(
        polyhal::time::Time::now().to_usec() as u64,
        Ordering::Release,
    );
}

#[cfg(feature = "mount-watchdog")]
pub(crate) fn mount_watchdog_stage(stage: usize) {
    if MOUNT_WATCHDOG_STARTED_US.load(Ordering::Acquire) != 0 {
        MOUNT_WATCHDOG_STAGE.store(stage, Ordering::Release);
    }
}

#[cfg(feature = "mount-watchdog")]
pub(crate) fn mount_watchdog_finish() {
    MOUNT_WATCHDOG_STAGE.store(0, Ordering::Release);
    MOUNT_WATCHDOG_STARTED_US.store(0, Ordering::Release);
}

#[cfg(feature = "mount-watchdog")]
pub(crate) fn mount_watchdog_poll() {
    let started = MOUNT_WATCHDOG_STARTED_US.load(Ordering::Acquire);
    if started == 0 {
        return;
    }
    let now = polyhal::time::Time::now().to_usec() as u64;
    if now.saturating_sub(started) < 3_000_000
        || MOUNT_WATCHDOG_REPORTED.swap(true, Ordering::AcqRel)
    {
        return;
    }
    crate::println!(
        "[mount-watchdog] stalled stage={} global-owner={}",
        MOUNT_WATCHDOG_STAGE.load(Ordering::Acquire),
        LWEXT4_GLOBAL.owner(),
    );
}

pub(crate) struct VfsFsGuard<'a> {
    // Struct fields are dropped in declaration order. The local Ext4 lock was
    // acquired after the global lwext4 lock and therefore must be declared
    // first so it is released first.
    local: FsGuard<'a, FsInner>,
    _global: FsGlobalGuard<'a, ()>,
}

impl Deref for VfsFsGuard<'_> {
    type Target = FsInner;

    fn deref(&self) -> &Self::Target {
        &self.local
    }
}

impl DerefMut for VfsFsGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.local
    }
}

fn record_path_owner(fs_path: &str, uid: u32, gid: u32) {
    let mut cache = PATH_OWNER_CACHE.exclusive_access();
    if !cache.contains_key(fs_path) && cache.len() >= PATH_OWNER_CACHE_MAX_ENTRIES {
        cache.clear();
    }
    cache.insert(String::from(fs_path), (uid, gid));
}

fn cached_path_owner(fs_path: &str) -> Option<(u32, u32)> {
    PATH_OWNER_CACHE.exclusive_access().get(fs_path).copied()
}

// ==================== DirEntry (local) ====================

pub struct DirEntry {
    pub inode: u32,
    name_bytes: Vec<u8>,
    pub de_type: u8,
}

impl DirEntry {
    pub fn get_name(&self) -> String {
        String::from_utf8_lossy(&self.name_bytes).into_owned()
    }
    pub fn get_de_type(&self) -> u8 {
        // ext4 stores its own compact directory-entry type values. Linux
        // exposes a different DT_* encoding through getdents64.
        match self.de_type as u32 {
            EXT4_DE_FIFO => 1,
            EXT4_DE_CHRDEV => 2,
            EXT4_DE_DIR => 4,
            EXT4_DE_BLKDEV => 6,
            EXT4_DE_REG_FILE => 8,
            EXT4_DE_SYMLINK => 10,
            EXT4_DE_SOCK => 12,
            _ => 0,
        }
    }
}

// ==================== InodeInfo ====================

#[derive(Clone, Copy)]
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
    /// Ext4/lwext4 is not reentrant.  The first SMP revision serializes each
    /// mounted superblock explicitly, rather than relying on a global
    /// single-core cell.
    pub fs_data: FsLock<FsInner>,
}

impl VfsSuperBlock {
    /// Serialize direct lwext4 handle operations.  Callers that also protect
    /// dentry or open-file metadata must acquire that VFS lock first, then
    /// this guard, following `VfsLock -> FsLock`.
    pub(crate) fn lock_fs(&self) -> VfsFsGuard<'_> {
        let global = LWEXT4_GLOBAL.exclusive_access();
        let local = self.fs_data.exclusive_access();
        VfsFsGuard {
            local,
            _global: global,
        }
    }

    pub fn new_ext4(
        blk_device: Arc<dyn crate::drivers::BlockDevice>,
        fs_type: &'static str,
    ) -> Option<Arc<Self>> {
        Self::new_ext4_at(blk_device, fs_type, "/mp/")
    }

    pub fn new_ext4_at(
        blk_device: Arc<dyn crate::drivers::BlockDevice>,
        fs_type: &'static str,
        mount_point: &str,
    ) -> Option<Arc<Self>> {
        const FALLBACK_DISK_SIZE: u64 = 512 * 1024 * 1024;
        #[cfg(feature = "mount-watchdog")]
        mount_watchdog_stage(6);
        #[cfg(feature = "mount-trace")]
        crate::println!("[mount-trace] capacity-begin point={}", mount_point);
        let total_size = blk_device.capacity_bytes().unwrap_or(FALLBACK_DISK_SIZE);
        #[cfg(feature = "mount-watchdog")]
        mount_watchdog_stage(7);
        #[cfg(feature = "mount-trace")]
        crate::println!(
            "[mount-trace] capacity-done point={} size={}",
            mount_point,
            total_size
        );
        let stream = BlockDeviceStream {
            inner: blk_device,
            pos: 0,
            total_size,
            scratch: alloc::vec![0u8; 128 * 1024],
        };
        #[cfg(feature = "mount-watchdog")]
        mount_watchdog_stage(8);
        #[cfg(feature = "mount-trace")]
        crate::println!("[mount-trace] stream-ready point={}", mount_point);
        // lwext4 keeps mount/cache state globally, not per Ext4BlockWrapper.
        // Mount creation must use the same serialization as normal filesystem
        // operations, otherwise a second disk mount can race a rootfs access
        // on another hart.
        #[cfg(feature = "smp-diagnostics")]
        crate::println!(
            "[mount-diag] create-begin point={} size={}",
            mount_point,
            total_size
        );
        let ext4 = {
            #[cfg(feature = "mount-watchdog")]
            mount_watchdog_stage(9);
            #[cfg(feature = "mount-trace")]
            crate::println!("[mount-trace] lwext4-lock-begin point={}", mount_point);
            let _global = LWEXT4_GLOBAL.exclusive_access();
            #[cfg(feature = "mount-watchdog")]
            mount_watchdog_stage(10);
            #[cfg(feature = "mount-trace")]
            crate::println!("[mount-trace] lwext4-lock-done point={}", mount_point);
            #[cfg(feature = "smp-diagnostics")]
            crate::println!("[mount-diag] global-acquired point={}", mount_point);
            #[cfg(feature = "mount-trace")]
            crate::println!("[mount-trace] lwext4-create-begin point={}", mount_point);
            Ext4BlockWrapper::<BlockDeviceStreamOp>::new_with_name(stream, "ext4_fs", mount_point)
                .ok()?
        };
        #[cfg(feature = "mount-watchdog")]
        mount_watchdog_stage(11);
        #[cfg(feature = "mount-trace")]
        crate::println!("[mount-trace] lwext4-create-done point={}", mount_point);
        #[cfg(feature = "smp-diagnostics")]
        crate::println!("[mount-diag] create-done point={}", mount_point);
        Some(Arc::new(VfsSuperBlock {
            fs_type,
            fs_data: FsLock::new_named(FsInner::Ext4(Ext4Wrapper(ext4)), "VfsFsData"),
        }))
    }

    fn with_fs<R>(&self, f: impl FnOnce(&Ext4Wrapper) -> R) -> R {
        let data = self.lock_fs();
        match &*data {
            FsInner::Ext4(wrapper) => f(wrapper),
        }
    }

    pub fn force_umount(&self) {
        let mut data = self.lock_fs();
        if let FsInner::Ext4(ref mut wrapper) = *data {
            unsafe {
                wrapper.0.lwext4_umount().ok();
            }
        }
    }

    // ---------- path-based ext4 operations ----------

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

    fn get_inode_info_result(&self, fs_path: &str) -> Result<InodeInfo, i32> {
        // The owner cache is VFS metadata. Take this snapshot before
        // `with_fs` acquires the superblock FsLock, preserving the documented
        // `VfsLock -> FsLock` order.
        let cached_owner = cached_path_owner(fs_path);
        self.with_fs(|_fs| {
            let c_path = CString::new(fs_path).map_err(|_| EINVAL as i32)?;
            let mut raw_ino: u32 = 0;
            let mut raw_inode: ext4_inode = unsafe { core::mem::zeroed() };
            let raw_rc = unsafe {
                ext4_raw_inode_fill(
                    c_path.as_ptr() as *const c_char,
                    &mut raw_ino,
                    &mut raw_inode,
                )
            };
            #[cfg(feature = "buildstorm-diagnostics")]
            if fs_path.contains("/target/debug/")
                && raw_rc != EOK as i32
                && raw_rc != ENOENT as i32
            {
                crate::println!(
                    "[buildstorm-fs] inode-query-failed raw_rc={} path={}",
                    raw_rc,
                    fs_path,
                );
            }
            if raw_rc != EOK as i32 {
                return Err(raw_rc);
            }

            // ext4_raw_inode_fill resolves the path once and copies a coherent
            // on-disk inode snapshot. Decoding that snapshot avoids repeating
            // the same path walk for mode, owner, three timestamps and size.
            let mode = u16::from_le(raw_inode.mode) as u32;
            let file_type = mode & 0o170000;
            let is_dir = file_type == 0o040000;
            let is_link = file_type == 0o120000;
            let size_lo = u32::from_le(raw_inode.size_lo) as u64;
            let size = if file_type == 0o100000 {
                size_lo | ((u32::from_le(raw_inode.size_hi) as u64) << 32)
            } else {
                size_lo
            };
            let mut uid = u16::from_le(raw_inode.uid) as u32;
            let mut gid = u16::from_le(raw_inode.gid) as u32;
            if let Some((cached_uid, cached_gid)) = cached_owner {
                uid = cached_uid;
                gid = cached_gid;
            }

            Ok(InodeInfo {
                inode_num: raw_ino,
                is_dir,
                is_link,
                mode,
                size,
                atime: u32::from_le(raw_inode.access_time),
                mtime: u32::from_le(raw_inode.modification_time),
                ctime: u32::from_le(raw_inode.change_inode_time),
                blocks_count: u32::from_le(raw_inode.blocks_count_lo) as u64,
                links_count: u16::from_le(raw_inode.links_count) as u32,
                uid,
                gid,
            })
        })
    }

    pub fn get_inode_info(&self, fs_path: &str) -> Option<InodeInfo> {
        self.get_inode_info_result(fs_path).ok()
    }

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
                #[cfg(feature = "buildstorm-diagnostics")]
                if fs_path.contains("/target/debug/") {
                    crate::println!(
                        "[buildstorm-fs] fallback-write-open-failed rc={} off={} len={} path={}",
                        rc,
                        offset,
                        buf.len(),
                        fs_path,
                    );
                }
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
                let chunk_len = max_read - total;
                let mut rw_count: usize = 0;
                #[cfg(feature = "smp-diagnostics")]
                let watched_before = crate::mm::watched_frame_is_intact();
                let read_rc = unsafe {
                    ext4_fread(
                        &mut file,
                        buf[total..total + chunk_len].as_mut_ptr() as *mut core::ffi::c_void,
                        chunk_len,
                        &mut rw_count,
                    )
                };
                #[cfg(feature = "smp-diagnostics")]
                if watched_before && !crate::mm::watched_frame_is_intact() {
                    crate::println!(
                        "[frame-diag] watched page changed during ext4_fread path={} off={:#x} requested={} returned={} rc={}",
                        fs_path,
                        offset + total,
                        chunk_len,
                        rw_count,
                        read_rc,
                    );
                }
                // The C API reports its transfer size through an out pointer.
                // Never let an invalid value advance Rust's slice cursor past
                // the caller-provided buffer.
                if read_rc != EOK as i32 || rw_count == 0 || rw_count > chunk_len {
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

    pub fn write_at(&self, fs_path: &str, offset: usize, buf: &[u8], timestamp: u32) -> usize {
        self.write_at_raw(fs_path, offset, buf, timestamp)
    }

    fn write_at_raw(&self, fs_path: &str, offset: usize, buf: &[u8], timestamp: u32) -> usize {
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
                ext4_fwrite_with_times(
                    &mut file,
                    buf.as_ptr() as *const core::ffi::c_void,
                    buf.len(),
                    &mut rw_count,
                    timestamp,
                    timestamp,
                )
            };
            unsafe {
                ext4_fclose(&mut file);
            }

            if write_rc != EOK as i32 {
                #[cfg(feature = "buildstorm-diagnostics")]
                if fs_path.contains("/target/debug/") {
                    crate::println!(
                        "[buildstorm-fs] fallback-write-failed rc={} off={} len={} written={} path={}",
                        write_rc,
                        offset,
                        buf.len(),
                        rw_count,
                        fs_path,
                    );
                }
                return 0;
            }
            rw_count
        })
    }

    pub fn create_file(&self, parent_path: &str, name: &str, is_dir: bool) -> Option<u32> {
        let full_path = if parent_path == "/" {
            format!("/{}", name)
        } else if parent_path.ends_with('/') {
            format!("{}{}", parent_path, name)
        } else {
            format!("{}/{}", parent_path, name)
        };

        let (created, raw_rc) = self.with_fs(|_fs| {
            if is_dir {
                let c_path = match CString::new(full_path.as_bytes()) {
                    Ok(path) => path,
                    Err(_) => return (None, -(EINVAL as i32)),
                };
                let rc = unsafe { ext4_dir_mk(c_path.as_ptr() as *const c_char) };
                if rc == EOK as i32 {
                    let mut inode = 0u32;
                    let mut raw_inode: ext4_inode = unsafe { core::mem::zeroed() };
                    let query_rc = unsafe {
                        ext4_raw_inode_fill(
                            c_path.as_ptr() as *const c_char,
                            &mut inode,
                            &mut raw_inode,
                        )
                    };
                    if query_rc == EOK as i32 {
                        return (Some(inode), query_rc);
                    }
                    return (None, query_rc);
                }
                (None, rc)
            } else {
                let c_path = match CString::new(full_path.as_bytes()) {
                    Ok(path) => path,
                    Err(_) => return (None, -(EINVAL as i32)),
                };
                let c_path = c_path.into_raw();
                // Creation must not truncate a file installed by a racing
                // O_CREAT caller. The returned handle is closed immediately.
                let flags = match CString::new("ab") {
                    Ok(flags) => flags,
                    Err(_) => return (None, -(EINVAL as i32)),
                };
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
                    (Some(inode), rc)
                } else {
                    (None, rc)
                }
            }
        });
        #[cfg(feature = "buildstorm-diagnostics")]
        if created.is_none() && full_path.contains("/target/debug/") {
            crate::println!(
                "[buildstorm-fs] create-failed dir={} rc={} path={}",
                is_dir,
                raw_rc,
                full_path,
            );
        }
        created
    }

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
                #[cfg(feature = "buildstorm-diagnostics")]
                if fs_path.contains("/target/debug/") {
                    crate::println!(
                        "[buildstorm-fs] truncate-open-failed rc={} size={} path={}",
                        rc,
                        size,
                        fs_path,
                    );
                }
                return false;
            }

            let old_size = unsafe { ext4_fsize(&mut file) };
            let inode = file.inode;
            let (ok, operation_rc) = if size <= old_size {
                let rc = unsafe { ext4_ftruncate(&mut file, size) };
                (rc == EOK as i32, rc)
            } else {
                let zeros = [0u8; 4096];
                let mut pos = old_size as usize;
                let target = size as usize;
                let mut operation_rc = EOK as i32;
                unsafe {
                    ext4_fseek(&mut file, old_size as i64, SEEK_SET as u32);
                }
                let mut ok = true;
                while pos < target {
                    let chunk = (target - pos).min(zeros.len());
                    let mut written: usize = 0;
                    let rc = unsafe {
                        ext4_fwrite(
                            &mut file,
                            zeros[..chunk].as_ptr() as *const core::ffi::c_void,
                            chunk,
                            &mut written,
                        )
                    };
                    if rc != EOK as i32 || written == 0 {
                        operation_rc = if rc == EOK as i32 { EIO as i32 } else { rc };
                        ok = false;
                        break;
                    }
                    pos += written;
                }
                (ok, operation_rc)
            };
            unsafe {
                ext4_fclose(&mut file);
            }
            #[cfg(feature = "buildstorm-diagnostics")]
            if !ok && fs_path.contains("/target/debug/") {
                crate::println!(
                    "[buildstorm-fs] truncate-write-failed rc={} inode={} old_size={} size={} path={}",
                    operation_rc,
                    inode,
                    old_size,
                    size,
                    fs_path,
                );
            }
            ok
        })
    }

    pub fn unlink_file(&self, fs_path: &str) -> bool {
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return false,
            };
            unsafe { ext4_fremove(c_path.as_ptr() as *const c_char) == EOK as i32 }
        })
    }

    pub fn unlink_dir(&self, fs_path: &str) -> bool {
        self.with_fs(|_fs| {
            let c_path = match CString::new(fs_path) {
                Ok(s) => s,
                Err(_) => return false,
            };
            unsafe { ext4_dir_rm(c_path.as_ptr() as *const c_char) == EOK as i32 }
        })
    }

    pub fn set_atime(&self, fs_path: &str, atime: u32) {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe {
                    ext4_atime_set(c_path.as_ptr() as *const c_char, atime);
                }
            }
        })
    }

    pub fn set_mtime(&self, fs_path: &str, mtime: u32) {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe {
                    ext4_mtime_set(c_path.as_ptr() as *const c_char, mtime);
                }
            }
        })
    }

    pub fn set_ctime(&self, fs_path: &str, ctime: u32) {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe {
                    ext4_ctime_set(c_path.as_ptr() as *const c_char, ctime);
                }
            }
        })
    }

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

    pub fn set_owner(&self, fs_path: &str, uid: u32, gid: u32) -> i32 {
        let result = self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe { ext4_owner_set(c_path.as_ptr() as *const c_char, uid, gid) }
            } else {
                -1
            }
        });
        // The owner cache is VFS metadata (rank 8), so update it only after
        // with_fs has released the lwext4 global and per-volume locks.
        if result == EOK as i32 {
            record_path_owner(fs_path, uid, gid);
        }
        result
    }

    pub fn fsync(&self, fs_path: &str) -> i32 {
        self.with_fs(|_fs| {
            if let Ok(c_path) = CString::new(fs_path) {
                unsafe { ext4_cache_flush(c_path.as_ptr() as *const c_char) }
            } else {
                -1
            }
        })
    }

    pub fn sync_all(&self) -> i32 {
        self.with_fs(|_fs| {
            let root = CString::new("/").unwrap();
            unsafe { ext4_cache_flush(root.as_ptr() as *const c_char) }
        })
    }

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
    pub parent: Option<Weak<Dentry>>,
    pub super_block: Arc<VfsSuperBlock>,
    /// Cache updates use a two-phase protocol: create/reserve a replacement
    /// list before acquiring this lock, and never call Ext4 while it is held.
    pub children: DentryLock<Option<Vec<Arc<Dentry>>>>,
    /// Even values describe a stable cache generation. Odd values mean a
    /// directory mutation is updating the child cache.
    child_cache_epoch: AtomicU64,
    /// This is only an in-memory metadata cache.  Ext4 lookup happens after
    /// dropping the lock, then the result is stored in a second short phase.
    inode_info: DentryLock<Option<InodeInfo>>,
}

impl Dentry {
    fn new_root(sb: Arc<VfsSuperBlock>) -> Arc<Self> {
        Arc::new(Dentry {
            name: String::from("/"),
            path: String::from("/"),
            fs_path: String::from(ROOT_LWEXT4_MOUNT),
            inode_num: 2,
            is_dir: true,
            is_link: false,
            parent: None,
            super_block: sb,
            children: DentryLock::new_named(None, "DentryChildren"),
            child_cache_epoch: AtomicU64::new(0),
            inode_info: DentryLock::new_named(None, "DentryInodeInfo"),
        })
    }

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
            children: DentryLock::new_named(None, "DentryChildren"),
            child_cache_epoch: AtomicU64::new(0),
            inode_info: DentryLock::new_named(None, "DentryInodeInfo"),
        })
    }

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
            parent: Some(Arc::downgrade(parent)),
            super_block: parent.super_block.clone(),
            children: DentryLock::new_named(None, "DentryChildren"),
            child_cache_epoch: AtomicU64::new(0),
            inode_info: DentryLock::new_named(None, "DentryInodeInfo"),
        })
    }

    pub fn inode_info(&self) -> Option<InodeInfo> {
        let cached = { *self.inode_info.exclusive_access() };
        if let Some(info) = cached {
            return Some(info);
        }
        let info = self.super_block.get_inode_info(&self.fs_path)?;
        *self.inode_info.exclusive_access() = Some(info);
        Some(info)
    }

    pub fn refresh_inode_info(&self) -> Option<InodeInfo> {
        let info = self.super_block.get_inode_info(&self.fs_path)?;
        *self.inode_info.exclusive_access() = Some(info);
        Some(info)
    }

    pub fn set_cached_inode_info(&self, info: InodeInfo) {
        *self.inode_info.exclusive_access() = Some(info);
    }

    pub fn invalidate_inode_info(&self) {
        *self.inode_info.exclusive_access() = None;
    }

    pub fn update_cached_size(&self, size: u64) {
        if let Some(ref mut info) = *self.inode_info.exclusive_access() {
            info.size = size;
            info.blocks_count = (size + 511) / 512;
        }
    }

    pub fn update_cached_mode(&self, mode: u32) {
        if let Some(ref mut info) = *self.inode_info.exclusive_access() {
            info.mode = mode;
            let file_type = mode & 0o170000;
            info.is_dir = file_type == 0o040000;
            info.is_link = file_type == 0o120000;
        }
    }

    pub fn update_cached_owner(&self, uid: u32, gid: u32, mode: u32, ctime: u32) {
        if let Some(ref mut info) = *self.inode_info.exclusive_access() {
            info.uid = uid;
            info.gid = gid;
            info.mode = mode;
            info.ctime = ctime;
        }
    }

    pub fn update_cached_times(&self, atime: Option<u32>, mtime: Option<u32>, ctime: Option<u32>) {
        if let Some(ref mut info) = *self.inode_info.exclusive_access() {
            if let Some(atime) = atime {
                info.atime = atime;
            }
            if let Some(mtime) = mtime {
                info.mtime = mtime;
            }
            if let Some(ctime) = ctime {
                info.ctime = ctime;
            }
        }
    }
}

// ==================== Mount ====================

pub struct Mount {
    pub mnt_point: Arc<Dentry>,
    pub mnt_root: Arc<Dentry>,
}

lazy_static! {
    pub static ref MOUNT_TABLE: VfsLock<Vec<Mount>> =
        VfsLock::new_named(Vec::with_capacity(MOUNT_TABLE_CAPACITY), "VfsMountTable");
    static ref READONLY_MOUNT_PATHS: VfsLock<Vec<String>> =
        VfsLock::new_named(Vec::with_capacity(MOUNT_TABLE_CAPACITY), "VfsReadonlyMounts");
    // Quota accounting is updated both before and after direct lwext4
    // transactions.  It never calls back into VFS/Fs, so give it a dedicated
    // post-Fs class instead of pretending it is ordinary dentry metadata.
    static ref TMPFS_MOUNTS: FsAuxLock<Vec<TmpfsMount>> =
        FsAuxLock::new_named(Vec::with_capacity(MOUNT_TABLE_CAPACITY), "VfsTmpfsMounts");
}

struct TmpfsMount {
    path: String,
    limit: Option<u64>,
    used: u64,
    files: BTreeMap<String, u64>,
}

fn normalize_mount_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('\0').trim_end_matches('/');
    if trimmed.is_empty() {
        String::from("/")
    } else {
        String::from(trimmed)
    }
}

fn path_is_under_mount(path: &str, mount_path: &str) -> bool {
    if mount_path == "/" {
        return true;
    }
    path == mount_path
        || path
            .strip_prefix(mount_path)
            .map(|suffix| suffix.starts_with('/'))
            .unwrap_or(false)
}

pub fn vfs_set_readonly_mount(path: &str, readonly: bool) {
    let normalized = normalize_mount_path(path);
    let mut paths = READONLY_MOUNT_PATHS.exclusive_access();
    if readonly {
        if !paths.iter().any(|p| p == &normalized) {
            paths.push(normalized);
        }
    } else {
        paths.retain(|p| p != &normalized);
    }
}

pub fn vfs_path_is_readonly(path: &str) -> bool {
    let normalized = normalize_mount_path(path);
    READONLY_MOUNT_PATHS
        .exclusive_access()
        .iter()
        .any(|mount_path| path_is_under_mount(&normalized, mount_path))
}

pub fn vfs_dentry_is_readonly(dentry: &Arc<Dentry>) -> bool {
    vfs_path_is_readonly(&dentry.path)
}

pub fn vfs_set_tmpfs_mount(path: &str, limit: Option<u64>) {
    let normalized = normalize_mount_path(path);
    let mut mounts = TMPFS_MOUNTS.exclusive_access();
    if let Some(mount) = mounts.iter_mut().find(|mount| mount.path == normalized) {
        mount.limit = limit;
        mount.used = 0;
        mount.files.clear();
        return;
    }
    mounts.push(TmpfsMount {
        path: normalized,
        limit,
        used: 0,
        files: BTreeMap::new(),
    });
}

pub fn vfs_clear_tmpfs_mount(path: &str) {
    let normalized = normalize_mount_path(path);
    TMPFS_MOUNTS
        .exclusive_access()
        .retain(|mount| mount.path != normalized);
}

pub fn vfs_tmpfs_can_resize(path: &str, file_key: &str, old_size: u64, new_size: u64) -> bool {
    let normalized = normalize_mount_path(path);
    let mut mounts = TMPFS_MOUNTS.exclusive_access();
    let Some(mount) = mounts
        .iter_mut()
        .filter(|mount| path_is_under_mount(&normalized, &mount.path))
        .max_by_key(|mount| mount.path.len())
    else {
        return true;
    };

    if !mount.files.contains_key(file_key) {
        mount.used = mount.used.saturating_add(old_size);
        mount.files.insert(String::from(file_key), old_size);
    }

    let current = mount.files.get(file_key).copied().unwrap_or(old_size);
    if new_size <= current {
        return true;
    }
    let delta = new_size - current;
    mount
        .limit
        .map(|limit| mount.used.saturating_add(delta) <= limit)
        .unwrap_or(true)
}

pub fn vfs_tmpfs_commit_resize(path: &str, file_key: &str, old_size: u64, new_size: u64) {
    let normalized = normalize_mount_path(path);
    let mut mounts = TMPFS_MOUNTS.exclusive_access();
    let Some(mount) = mounts
        .iter_mut()
        .filter(|mount| path_is_under_mount(&normalized, &mount.path))
        .max_by_key(|mount| mount.path.len())
    else {
        return;
    };

    let current = mount.files.get(file_key).copied().unwrap_or(old_size);
    if new_size >= current {
        mount.used = mount.used.saturating_add(new_size - current);
    } else {
        mount.used = mount.used.saturating_sub(current - new_size);
    }
    mount.files.insert(String::from(file_key), new_size);
}

fn lookup_mount(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    let table = MOUNT_TABLE.exclusive_access();
    for m in table.iter() {
        if Arc::ptr_eq(&m.mnt_point, dentry) || m.mnt_point.path == dentry.path {
            return Some(m.mnt_root.clone());
        }
    }
    None
}

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

// Large build trees routinely contain hundreds of siblings. Grow hot directory
// caches outside DentryLock while keeping small directories inexpensive.
const DENTRY_CHILD_CACHE_INITIAL_CAPACITY: usize = 16;
const DENTRY_CHILD_CACHE_LIMIT: usize = 1024;

pub fn dentry_parent(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    dentry.parent.as_ref().and_then(Weak::upgrade)
}

fn vfs_parent(dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
    if let Some(mnt_point) = is_mount_root(dentry) {
        return dentry_parent(&mnt_point);
    }
    dentry_parent(dentry)
}

const MAX_SYMLINK_DEPTH: usize = 40;
const MAX_SYMLINK_TARGET_LEN: usize = 4096;

pub(crate) fn read_dentry_symlink_bytes(dentry: &Arc<Dentry>) -> Option<Vec<u8>> {
    let info = dentry.inode_info()?;
    if !info.is_link {
        return None;
    }
    let size = usize::try_from(info.size).ok()?;
    if size > MAX_SYMLINK_TARGET_LEN {
        return None;
    }

    let mut target_buf = alloc::vec![0u8; size.saturating_add(1).max(1)];
    let mut rcnt = 0usize;
    let c_path = CString::new(dentry.fs_path.as_bytes()).ok()?;
    let rc = {
        let _fs_lock = dentry.super_block.lock_fs();
        unsafe {
            ext4_readlink(
                c_path.as_ptr() as *const c_char,
                target_buf.as_mut_ptr() as *mut c_char,
                target_buf.len(),
                &mut rcnt,
            )
        }
    };
    if rc != 0 || rcnt == 0 || rcnt > target_buf.len() {
        return None;
    }

    target_buf.truncate(rcnt);
    while target_buf.last() == Some(&0) {
        target_buf.pop();
    }
    if target_buf.is_empty() {
        None
    } else {
        Some(target_buf)
    }
}

pub(crate) fn read_dentry_symlink(dentry: &Arc<Dentry>) -> Option<String> {
    String::from_utf8(read_dentry_symlink_bytes(dentry)?).ok()
}

fn begin_child_cache_mutation(parent: &Dentry) -> u64 {
    loop {
        let epoch = parent.child_cache_epoch.load(Ordering::Acquire);
        if epoch & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        let updating = epoch.wrapping_add(1);
        if parent
            .child_cache_epoch
            .compare_exchange_weak(epoch, updating, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return updating;
        }
    }
}

fn finish_child_cache_mutation(parent: &Dentry, updating: u64) {
    debug_assert_eq!(updating & 1, 1);
    parent
        .child_cache_epoch
        .store(updating.wrapping_add(1), Ordering::Release);
}

pub(crate) struct ChildCacheMutation {
    parent: Arc<Dentry>,
    updating: u64,
}

impl Drop for ChildCacheMutation {
    fn drop(&mut self) {
        finish_child_cache_mutation(&self.parent, self.updating);
    }
}

pub(crate) fn begin_cached_child_mutation(parent: &Arc<Dentry>) -> ChildCacheMutation {
    ChildCacheMutation {
        parent: parent.clone(),
        updating: begin_child_cache_mutation(parent),
    }
}

fn child_cache_epoch_matches(parent: &Dentry, expected_epoch: Option<u64>) -> bool {
    match expected_epoch {
        Some(epoch) => {
            epoch & 1 == 0 && parent.child_cache_epoch.load(Ordering::Acquire) == epoch
        }
        None => true,
    }
}

fn insert_positive_child(
    parent: &Arc<Dentry>,
    child: Arc<Dentry>,
    expected_epoch: Option<u64>,
) -> Option<Arc<Dentry>> {
    if !child_cache_epoch_matches(parent, expected_epoch) {
        return None;
    }

    // Reserve outside DentryLock. A second lookup handles a racing initializer.
    if parent.children.exclusive_access().is_none() {
        let mut initial = Vec::with_capacity(DENTRY_CHILD_CACHE_INITIAL_CAPACITY);
        initial.push(child.clone());
        let mut initial = Some(initial);
        let mut stale = false;
        let installed = {
            let mut children = parent.children.exclusive_access();
            if !child_cache_epoch_matches(parent, expected_epoch) {
                stale = true;
                false
            } else if children.is_none() {
                *children = initial.take();
                true
            } else {
                false
            }
        };
        drop(initial);
        if stale {
            return None;
        }
        if installed {
            return Some(child);
        }
    }

    enum InsertAction {
        Done(Arc<Dentry>),
        Grow(usize),
        Stale,
        Uncached,
    }

    loop {
        let mut retired_child = None;
        let action = {
            let mut children = parent.children.exclusive_access();
            if !child_cache_epoch_matches(parent, expected_epoch) {
                InsertAction::Stale
            } else {
                let list = children
                    .as_mut()
                    .expect("Dentry child cache initialized before use");
                match list
                    .binary_search_by(|cached| cached.name.as_str().cmp(child.name.as_str()))
                {
                    Ok(idx) => {
                        if list[idx].inode_num == child.inode_num {
                            InsertAction::Done(list[idx].clone())
                        } else {
                            retired_child =
                                Some(core::mem::replace(&mut list[idx], child.clone()));
                            InsertAction::Done(child.clone())
                        }
                    }
                    Err(idx) if list.len() < list.capacity() => {
                        list.insert(idx, child.clone());
                        InsertAction::Done(child.clone())
                    }
                    Err(_) if list.capacity() < DENTRY_CHILD_CACHE_LIMIT => {
                        let next = list
                            .capacity()
                            .max(DENTRY_CHILD_CACHE_INITIAL_CAPACITY)
                            .saturating_mul(2)
                            .min(DENTRY_CHILD_CACHE_LIMIT);
                        InsertAction::Grow(next)
                    }
                    Err(_) => {
                        if let Some(idx) = list
                            .iter()
                            .position(|cached| Arc::strong_count(cached) == 1)
                        {
                            retired_child = Some(list.remove(idx));
                            let idx = list
                                .binary_search_by(|cached| {
                                    cached.name.as_str().cmp(child.name.as_str())
                                })
                                .unwrap_or_else(|idx| idx);
                            list.insert(idx, child.clone());
                            InsertAction::Done(child.clone())
                        } else {
                            InsertAction::Uncached
                        }
                    }
                }
            }
        };
        drop(retired_child);

        let target_capacity = match action {
            InsertAction::Done(canonical) => return Some(canonical),
            InsertAction::Stale | InsertAction::Uncached => return None,
            InsertAction::Grow(target_capacity) => target_capacity,
        };

        let mut replacement = Some(Vec::with_capacity(target_capacity));
        let mut retired_cache = None;
        let mut stale = false;
        {
            let mut children = parent.children.exclusive_access();
            if !child_cache_epoch_matches(parent, expected_epoch) {
                stale = true;
            } else if let Some(list) = children.as_mut() {
                if list.len() == list.capacity() && list.capacity() < target_capacity {
                    let mut old = core::mem::replace(
                        list,
                        replacement
                            .take()
                            .expect("replacement child cache allocated before lock"),
                    );
                    list.append(&mut old);
                    retired_cache = Some(old);
                }
            }
        }
        drop(retired_cache);
        drop(replacement);
        if stale {
            return None;
        }
    }
}

pub(crate) fn cache_child_during_mutation(
    parent: &Arc<Dentry>,
    child: Arc<Dentry>,
) -> Arc<Dentry> {
    insert_positive_child(parent, child.clone(), None).unwrap_or(child)
}

pub(crate) fn invalidate_cached_child_during_mutation(parent: &Arc<Dentry>, name: &str) {
    let mut retired_positive = None;
    {
        let mut children = parent.children.exclusive_access();
        if let Some(list) = children.as_mut() {
            if let Ok(idx) = list.binary_search_by(|cached| cached.name.as_str().cmp(name)) {
                retired_positive = Some(list.remove(idx));
            }
        }
    }
    drop(retired_positive);
}

pub(crate) fn invalidate_cached_child(parent: &Arc<Dentry>, name: &str) {
    let _mutation = begin_cached_child_mutation(parent);
    invalidate_cached_child_during_mutation(parent, name);
}

pub(crate) fn mark_cached_child_absent(parent: &Arc<Dentry>, name: &str) {
    invalidate_cached_child(parent, name);
}

pub fn vfs_lookup(parent: &Arc<Dentry>, name: &str) -> Option<Arc<Dentry>> {
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

    let cache_epoch = parent.child_cache_epoch.load(Ordering::Acquire);
    if cache_epoch & 1 == 0 {
        let cached = {
            let children = parent.children.exclusive_access();
            children.as_ref().and_then(|list| {
                list.binary_search_by(|cached| cached.name.as_str().cmp(name))
                    .ok()
                    .map(|idx| list[idx].clone())
            })
        };
        if parent.child_cache_epoch.load(Ordering::Acquire) == cache_epoch {
            if let Some(cached) = cached {
                if let Some(mnt_root) = lookup_mount(&cached) {
                    return Some(mnt_root);
                }
                return Some(cached);
            }

        }
    }

    let lookup_epoch = parent.child_cache_epoch.load(Ordering::Acquire);
    let lookup_epoch = (lookup_epoch & 1 == 0).then_some(lookup_epoch);

    let child_fs_path = if parent.fs_path == "/" {
        format!("/{}", name)
    } else if parent.fs_path.ends_with('/') {
        format!("{}{}", parent.fs_path, name)
    } else {
        format!("{}/{}", parent.fs_path, name)
    };

    let info = match parent.super_block.get_inode_info_result(&child_fs_path) {
        Ok(info) => info,
        Err(_) => return None,
    };
    let (child_inode, is_dir, is_link) = (info.inode_num, info.is_dir, info.is_link);

    let child = Dentry::new_child(parent, name, child_inode, is_dir, is_link);
    child.set_cached_inode_info(info);

    let child = lookup_epoch
        .and_then(|epoch| insert_positive_child(parent, child.clone(), Some(epoch)))
        .unwrap_or(child);

    if let Some(mnt_root) = lookup_mount(&child) {
        return Some(mnt_root);
    }

    Some(child)
}

fn vfs_resolve_path_inner(
    start: &Arc<Dentry>,
    path: &str,
    follow_final_symlink: bool,
) -> Option<Arc<Dentry>> {
    let path = path.trim_matches('\0');
    if path.is_empty() {
        return None;
    }

    let is_abs = path.starts_with('/');
    let root = crate::task::current_task()
        .map(|t| t.inner_exclusive_access().root.clone())
        .unwrap_or_else(|| ROOT_DENTRY.clone());
    let initial = if is_abs { root.clone() } else { start.clone() };

    // Most build-system paths contain no symlink. Walk borrowed components
    // first and only allocate a rewrite queue when a symlink is encountered.
    let mut current = initial.clone();
    let mut borrowed_components = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .peekable();
    let mut needs_symlink_resolution = false;
    while let Some(component) = borrowed_components.next() {
        let next = vfs_lookup(&current, component)?;
        if next.is_link && (follow_final_symlink || borrowed_components.peek().is_some()) {
            needs_symlink_resolution = true;
            break;
        }
        current = next;
    }
    if !needs_symlink_resolution {
        return lookup_mount(&current).or(Some(current));
    }

    let mut current = initial;
    let mut components: VecDeque<String> = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(String::from)
        .collect();
    let mut symlink_depth = 0usize;

    while let Some(component) = components.pop_front() {
        let next = vfs_lookup(&current, component.as_str())?;
        if next.is_link && (follow_final_symlink || !components.is_empty()) {
            symlink_depth += 1;
            if symlink_depth > MAX_SYMLINK_DEPTH {
                return None;
            }

            let target = read_dentry_symlink(&next)?;
            if target.starts_with('/') {
                current = root.clone();
            }
            let mut target_components: Vec<String> = target
                .split('/')
                .filter(|part| !part.is_empty() && *part != ".")
                .map(String::from)
                .collect();
            while let Some(part) = target_components.pop() {
                components.push_front(part);
            }
            continue;
        }

        current = next;
    }

    lookup_mount(&current).or(Some(current))
}

pub fn vfs_resolve_path(start: &Arc<Dentry>, path: &str) -> Option<Arc<Dentry>> {
    vfs_resolve_path_inner(start, path, false)
}

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

pub fn vfs_resolve_parent_and_name(
    start: &Arc<Dentry>,
    path: &str,
) -> Option<(Arc<Dentry>, String)> {
    let path = path.trim_matches('\0');
    if path.is_empty() {
        return None;
    }

    let filename = path
        .rsplit('/')
        .find(|component| !component.is_empty() && *component != ".")?;
    let filename_offset = filename.as_ptr() as usize - path.as_ptr() as usize;
    let parent_path = &path[..filename_offset];
    let parent_path = if parent_path.is_empty() {
        if path.starts_with('/') { "/" } else { "." }
    } else {
        parent_path
    };
    let parent = vfs_resolve_path_inner(start, parent_path, true)?;
    if !parent.is_dir {
        return None;
    }

    Some((parent, String::from(filename)))
}

// ==================== File Creation ====================

pub fn vfs_create(
    parent: &Arc<Dentry>,
    name: &str,
    is_dir: bool,
    mode: u32,
) -> Option<Arc<Dentry>> {
    // Keep cached lookups non-authoritative from the physical create through
    // publication of the new canonical dentry.
    let _mutation = begin_cached_child_mutation(parent);
    vfs_create_during_mutation(parent, name, is_dir, mode)
}

pub(crate) fn vfs_create_during_mutation(
    parent: &Arc<Dentry>,
    name: &str,
    is_dir: bool,
    mode: u32,
) -> Option<Arc<Dentry>> {
    #[cfg(feature = "buildstorm-diagnostics")]
    if name == "invoked.timestamp" || name == "build-script-build-script-build" {
        crate::println!(
            "[buildstorm-fs] traced-vfs-create name={} dir={} mode={:#o} parent={}",
            name,
            is_dir,
            mode,
            parent.fs_path,
        );
    }
    let inode_num = parent
        .super_block
        .create_file(&parent.fs_path, name, is_dir)?;

    let child = Dentry::new_child(parent, name, inode_num, is_dir, false);

    let (uid, egid, umask) = crate::task::current_task()
        .map(|t| {
            let inner = t.inner_exclusive_access();
            (inner.euid, inner.egid, inner.umask)
        })
        .unwrap_or((0, 0, 0o22));
    let parent_info = parent.inode_info();
    let parent_sgid = parent_info
        .as_ref()
        .map(|info| (info.mode & 0o2000) != 0)
        .unwrap_or(false);
    let gid = if parent_sgid {
        parent_info.as_ref().map(|info| info.gid).unwrap_or(egid)
    } else {
        egid
    };
    let mode_type = mode & 0o170000;
    let file_type = if is_dir {
        0o040000
    } else if mode_type != 0 {
        mode_type
    } else {
        0o100000
    };
    let mut effective_perm = mode & !umask & 0o7777;
    if is_dir && parent_sgid {
        effective_perm |= 0o2000;
    }
    let effective_mode = file_type | effective_perm;
    let fs_path = child.fs_path.clone();
    child.super_block.set_owner(&fs_path, uid, gid);
    child.super_block.set_mode(&fs_path, effective_mode);
    if let Some(info) = child.super_block.get_inode_info(&fs_path) {
        child.set_cached_inode_info(info);
    }

    Some(cache_child_during_mutation(parent, child))
}

// ==================== Mount Operations ====================

pub fn vfs_mount(mnt_point: Arc<Dentry>, sb: Arc<VfsSuperBlock>, fs_path: &str) -> bool {
    let path = mnt_point.path.clone();
    {
        let table = MOUNT_TABLE.exclusive_access();
        if table
            .iter()
            .any(|m| m.mnt_point.path == path && path != "/")
            || table.len() == MOUNT_TABLE_CAPACITY
        {
            return false;
        }
    }

    let mnt_root = Dentry::new_mount_root(sb, mnt_point.path.clone(), fs_path);

    let mut table = MOUNT_TABLE.exclusive_access();
    // Recheck after building the new dentry: another hart may have mounted in
    // the interval.  The preallocated table must never grow under VfsLock.
    if table
        .iter()
        .any(|m| m.mnt_point.path == path && path != "/")
        || table.len() == MOUNT_TABLE_CAPACITY
    {
        return false;
    }
    table.push(Mount {
        mnt_point: mnt_point.clone(),
        mnt_root: mnt_root.clone(),
    });

    true
}

pub fn vfs_umount(mnt_point: &Arc<Dentry>) -> bool {
    let path = &mnt_point.path;
    // Destructing a Mount drops Arcs and may release filesystem state.  Move
    // it out before leaving the VFS domain rather than dropping it in-lock.
    let removed = {
        let mut table = MOUNT_TABLE.exclusive_access();
        table
            .iter()
            .position(|m| &m.mnt_point.path == path)
            .map(|idx| table.remove(idx))
    };
    removed.is_some()
}

// ==================== Root Filesystem ====================

use crate::drivers::{BLOCK_DEVICE, ROOT_BLOCK_DEVICE};

lazy_static! {
    pub static ref ROOT_SB: Arc<VfsSuperBlock> = {
        VfsSuperBlock::new_ext4_at(ROOT_BLOCK_DEVICE.clone(), "ext4", ROOT_LWEXT4_MOUNT)
            .expect("Failed to mount root ext4 filesystem")
    };
    pub static ref ROOT_DENTRY: Arc<Dentry> = Dentry::new_root(ROOT_SB.clone());
}

pub fn mount_root() {
    let root = ROOT_DENTRY.clone();
    MOUNT_TABLE.exclusive_access().push(Mount {
        mnt_point: root.clone(),
        mnt_root: root,
    });
}

/// Flush and unmount every ext4 volume immediately before powering off.
///
/// The mount table can contain multiple dentries backed by the same superblock,
/// so deduplicate the snapshot before invoking lwext4.  Locks are released
/// before flushing/unmounting to preserve the VFS lock order.
pub fn shutdown_filesystems() {
    let mut super_blocks: Vec<Arc<VfsSuperBlock>> = Vec::new();
    {
        let table = MOUNT_TABLE.exclusive_access();
        for mount in table.iter() {
            let sb = mount.mnt_root.super_block.clone();
            if !super_blocks.iter().any(|known| Arc::ptr_eq(known, &sb)) {
                super_blocks.push(sb);
            }
        }
    }

    if !super_blocks
        .iter()
        .any(|known| Arc::ptr_eq(known, &ROOT_SB))
    {
        super_blocks.push(ROOT_SB.clone());
    }

    for sb in &super_blocks {
        let _ = sb.sync_all();
    }
    // Unmount child/test filesystems before the root filesystem.
    for sb in super_blocks.iter().rev() {
        sb.force_umount();
    }
}

/// Mount the test disk (bus 0) at the specified mount point dentry.
pub fn mount_test_disk_at(mnt_point: Arc<Dentry>) -> bool {
    let test_sb = match VfsSuperBlock::new_ext4_at(BLOCK_DEVICE.clone(), "ext4", "/__mount_vda/") {
        Some(sb) => sb,
        None => return false,
    };
    vfs_mount(mnt_point, test_sb, "/__mount_vda/")
}

pub fn list_apps() {
    println!("/**** APPS ****");
    ROOT_SB
        .dir_get_entries(ROOT_LWEXT4_BIN)
        .iter()
        .filter(|e| {
            let name = e.get_name();
            name != "." && name != ".." && !name.is_empty()
        })
        .for_each(|e| println!("{}", e.get_name()));
    println!("**************/");
}
