// 这里负责操作系统的文件管理和伪文件接口：实现少量 /proc 和 /dev/rtc 伪文件，给用户态程序查询系统状态。
use super::{File, Kstat};
use crate::sync::VfsLock;
use crate::task::{online_cpu_mask, task_snapshot};
use alloc::string::String;
use alloc::vec::Vec;

const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const SYS_ROOT_ENTRIES: [(&str, u8); 2] = [("block", DT_DIR), ("dev", DT_DIR)];
const SYS_BLOCK_ENTRIES: [(&str, u8); 2] = [("vda", DT_DIR), ("vdb", DT_DIR)];
const SYS_BLOCK_DEVICE_ENTRIES: [(&str, u8); 2] = [("stat", DT_REG), ("uevent", DT_REG)];
const SYS_DEV_ENTRIES: [(&str, u8); 1] = [("block", DT_DIR)];
const SYS_DEV_BLOCK_ENTRIES: [(&str, u8); 2] = [("254:0", DT_DIR), ("254:1", DT_DIR)];
const SYS_DEV_BLOCK_DEVICE_ENTRIES: [(&str, u8); 1] = [("uevent", DT_REG)];

/// Advance one opened pseudo-file's cursor while copying from an already
/// prepared byte slice.  The caller must prepare the content before taking
/// this lock: formatting, task lookup and allocation are not allowed while a
/// VFS lock is held.
fn read_with_offset(offset: &VfsLock<usize>, content: &[u8], buf: &mut [u8]) -> isize {
    let mut offset = offset.exclusive_access();
    if *offset >= content.len() {
        return 0;
    }
    let len = buf.len().min(content.len() - *offset);
    buf[..len].copy_from_slice(&content[*offset..*offset + len]);
    *offset += len;
    len as isize
}

pub struct ProcDirFile {
    offset: VfsLock<usize>,
}

impl ProcDirFile {
    // 打开 /proc 目录时创建一个带独立遍历偏移的目录文件。
    // 执行时只初始化 offset 为 0，让每个打开的 /proc 目录都有自己的 getdents 进度。
    pub fn new() -> Self {
        Self {
            offset: VfsLock::new_named(0, "ProcDirOffset"),
        }
    }
}

impl File for ProcDirFile {
    // /proc 目录允许读目录项。
    // 执行时直接返回 true，表示这个目录对象可以被 getdents64 消费。
    fn readable(&self) -> bool {
        true
    }
    // /proc 目录不允许写。
    // 执行时直接返回 false，避免 write 系统调用把目录当普通文件写。
    fn writable(&self) -> bool {
        false
    }
    // 目录不能当普通文件读。
    // 执行时直接返回 -1，提醒调用者应该用 getdents64 读取目录项。
    fn read(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    // 目录不能当普通文件写。
    // 执行时直接返回 -1，因为 /proc 目录本身不支持写入。
    fn write(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    // 这里明确告诉 VFS：这是目录。
    // 执行时直接返回 true，让 fstatat/openat 等路径把它识别成目录。
    fn is_dir(&self) -> bool {
        true
    }
    // 返回 /proc 目录的基本 stat。
    // 执行时构造目录类型的 Kstat，并把权限设置成只读可执行的 0555。
    fn stat(&self) -> Kstat {
        let mut st = Kstat::new(0x4000);
        st.st_mode = 0o555 | 0x4000;
        st
    }
    // 生成 .、.. 和当前进程 pid 目录项。
    // 执行时先从 PID2TCB 收集并排序 pid，再按 Linux dirent64 布局从当前 offset 写入用户缓冲区。
    fn getdents64(&self, buf: &mut [u8]) -> isize {
        const DIRENT64_FIXED_SIZE: usize = 8 + 8 + 2 + 1;
        // Snapshot PIDs before taking this proc-file object's lock: PID
        // registry is outermost in the SMP order.
        let mut pids: Vec<usize> = task_snapshot().iter().map(|task| task.getpid()).collect();
        pids.sort();

        let mut inner = self.offset.exclusive_access();

        let mut entries: Vec<(String, u32)> = Vec::new();
        if *inner == 0 {
            entries.push((String::from("."), 2));
            entries.push((String::from(".."), 2));
        }
        for &pid in &pids {
            entries.push((alloc::format!("{}", pid), pid as u32));
        }

        let mut written = 0usize;
        let mut idx = *inner;

        while idx < entries.len() {
            let (name, inode) = &entries[idx];
            let name_bytes = name.as_bytes();
            let record_size =
                crate::fs::inode::OSInode::align_up(DIRENT64_FIXED_SIZE + name_bytes.len() + 1, 8);
            if written + record_size > buf.len() {
                if written == 0 {
                    return -1;
                }
                break;
            }
            let record = &mut buf[written..written + record_size];
            record.fill(0);
            record[0..8].copy_from_slice(&(*inode as u64).to_le_bytes());
            let d_off = (idx as i64) + 1;
            record[8..16].copy_from_slice(&d_off.to_le_bytes());
            record[16..18].copy_from_slice(&(record_size as u16).to_le_bytes());

            // `record` was zero-filled above, so copying the name bytes leaves
            // the mandatory trailing NUL in place without allocating a
            // temporary `String` while the cursor lock is held.
            record[19..19 + name_bytes.len()].copy_from_slice(name_bytes);

            written += record_size;
            idx += 1;
        }
        *inner = idx;
        written as isize
    }
    // 给 fd 层做具体类型判断。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct ProcPidStatFile {
    pid: usize,
    offset: VfsLock<usize>,
}

impl ProcPidStatFile {
    // 为 /proc/<pid>/stat 准备一个独立的读取偏移。
    // 执行时保存目标 pid，并把文本读取 offset 初始化为 0。
    pub fn new(pid: usize) -> Self {
        Self {
            pid,
            offset: VfsLock::new_named(0, "ProcPidStatOffset"),
        }
    }

    // 临时拼出一行 Linux stat 兼容文本，够 ps/top 类程序识别。
    // 执行时按 pid 查任务，读取状态、父 pid 和一个可显示命令名，拼成 /proc/<pid>/stat 文本。
    fn build_content(&self) -> String {
        use alloc::format;
        if let Some(task) = crate::task::pid2task(self.pid) {
            // Take only cheap task-state snapshots under TaskLock.  In
            // particular, `File::path` may enter VFS, so use the cloned Arc
            // only after dropping the TCB lock.
            let (state, stdin, parent, pgid, sid, utime, stime) = {
                let inner = task.inner_exclusive_access();
                (
                    match inner.task_status {
                        crate::task::TaskStatus::Running => 'R',
                        crate::task::TaskStatus::Zombie => 'Z',
                        _ => 'S',
                    },
                    inner.fd_table.first().and_then(|file| file.as_ref()).cloned(),
                    inner.parent.clone(),
                    inner.pgid,
                    inner.sid,
                    inner.utime.max(1),
                    inner.stime,
                )
            };
            let cmd = stdin
                .and_then(|file| file.path())
                .unwrap_or_else(|| String::from("initproc"));
            let ppid = parent
                .and_then(|parent| parent.upgrade())
                .map(|parent| parent.getpid())
                .unwrap_or(0);
            format!("{} ({}) {} {} {} {} 0 0 0 0 0 0 0 {} {} 0 0 20 0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
                self.pid, cmd, state, ppid, pgid, sid, utime, stime)
        } else {
            String::new()
        }
    }
}

impl File for ProcPidStatFile {
    // /proc/<pid>/stat 可以读。
    // 执行时直接返回 true，表示这个伪文件支持 read。
    fn readable(&self) -> bool {
        true
    }
    // /proc/<pid>/stat 不允许写。
    // 执行时直接返回 false，表示这个伪文件是只读的。
    fn writable(&self) -> bool {
        false
    }
    // 按普通文本文件的方式分段读 stat 内容。
    // 执行时重新生成 stat 文本，从当前 offset 拷贝一段到 buf，并推进 offset。
    fn read(&self, buf: &mut [u8]) -> isize {
        let content = self.build_content();
        read_with_offset(&self.offset, content.as_bytes(), buf)
    }
    // 这个伪文件只读。
    // 执行时直接返回 -1，因为 /proc/<pid>/stat 不接受写入。
    fn write(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    // stat 大小按当前生成出来的文本长度计算。
    // 执行时生成一次内容，用文本长度填 st_size，并设置成普通只读文件。
    fn stat(&self) -> Kstat {
        let mut st = Kstat::new(super::S_IFREG);
        st.st_size = self.build_content().len() as i64;
        st.st_mode = super::S_IFREG | 0o444;
        st
    }
    // 给 fd 层做具体类型判断。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct ProcPidStatusFile {
    pid: usize,
    offset: VfsLock<usize>,
}

impl ProcPidStatusFile {
    pub fn new(pid: usize) -> Self {
        Self {
            pid,
            offset: VfsLock::new_named(0, "ProcPidStatusOffset"),
        }
    }

    fn build_content(&self) -> String {
        use alloc::format;
        if let Some(task) = crate::task::pid2task(self.pid) {
            let (state, parent, tgid, uid, euid, suid, gid, egid, sgid) = {
                let inner = task.inner_exclusive_access();
                (
                    match inner.task_status {
                        crate::task::TaskStatus::Running => "R (running)",
                        crate::task::TaskStatus::Zombie => "Z (zombie)",
                        _ => "S (sleeping)",
                    },
                    inner.parent.clone(),
                    inner.tgid,
                    inner.uid,
                    inner.euid,
                    inner.suid,
                    inner.gid,
                    inner.egid,
                    inner.sgid,
                )
            };
            let ppid = parent
                .and_then(|parent| parent.upgrade())
                .map(|parent| parent.getpid())
                .unwrap_or(0);
            let allowed = task.allowed_cpus() & online_cpu_mask();
            format!(
                "Name:\tinitproc\nState:\t{}\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nUid:\t{} {} {} {}\nGid:\t{} {} {} {}\nVmLck:\t       0 kB\nVmSwap:\t       0 kB\nThreads:\t1\nCpus_allowed:\t{:x}\nMems_allowed:\t1\n",
                state,
                tgid,
                self.pid,
                ppid,
                uid,
                euid,
                suid,
                euid,
                gid,
                egid,
                sgid,
                egid,
                allowed,
            )
        } else {
            String::new()
        }
    }
}

impl File for ProcPidStatusFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, buf: &mut [u8]) -> isize {
        let content = self.build_content();
        read_with_offset(&self.offset, content.as_bytes(), buf)
    }
    fn write(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    fn stat(&self) -> Kstat {
        let mut st = Kstat::new(super::S_IFREG);
        st.st_size = self.build_content().len() as i64;
        st.st_mode = super::S_IFREG | 0o444;
        st
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// Snapshot of the harts which completed scheduler initialization when
/// `/proc/cpuinfo` was opened.  This makes the default one-core QEMU boot and
/// an explicit SMP boot accurately distinguishable to userspace.
pub struct ProcCpuinfoFile {
    offset: VfsLock<usize>,
    content: String,
}

impl ProcCpuinfoFile {
    pub fn new() -> Self {
        let online = online_cpu_mask();
        let mut content = String::new();
        for hart in 0..core::mem::size_of::<usize>() * 8 {
            if online & (1usize << hart) == 0 {
                continue;
            }
            #[cfg(target_arch = "riscv64")]
            {
                use alloc::fmt::Write;
                let _ = writeln!(
                    content,
                    "processor\t: {hart}\nhart\t\t: {hart}\nisa\t\t: rv64imafdc\nmmu\t\t: sv39\n"
                );
            }
            #[cfg(target_arch = "loongarch64")]
            {
                use alloc::fmt::Write;
                let _ = writeln!(
                    content,
                    "processor\t: {hart}\nisa\t\t: loongarch64\n"
                );
            }
        }
        Self {
            offset: VfsLock::new_named(0, "ProcCpuinfoOffset"),
            content,
        }
    }
}

impl File for ProcCpuinfoFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, buf: &mut [u8]) -> isize {
        read_with_offset(&self.offset, self.content.as_bytes(), buf)
    }
    fn write(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    fn stat(&self) -> Kstat {
        let mut st = Kstat::new(super::S_IFREG);
        st.st_size = self.content.len() as i64;
        st.st_mode = super::S_IFREG | 0o444;
        st
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct ProcMountsFile {
    offset: VfsLock<usize>,
    content: String,
}

impl ProcMountsFile {
    // 打开 /proc/mounts 时截一份当前挂载表文本。
    // 执行时遍历挂载表，把每个挂载点格式化成一行文本，并保存独立读取 offset。
    pub fn new() -> Self {
        const PREFIX: &str = "none ";
        const SUFFIX: &str = " rw,relatime 0 0\n";

        // MOUNT_TABLE may change between a sizing pass and a formatting pass.
        // Retry rather than growing `content` with the VFS lock held.
        let content = loop {
            let required = {
                let table = crate::fs::vfs::MOUNT_TABLE.exclusive_access();
                table
                    .iter()
                    .map(|mount| {
                        PREFIX.len()
                            + mount.mnt_point.path.len()
                            + 1
                            + mount.mnt_root.super_block.fs_type.len()
                            + SUFFIX.len()
                    })
                    .sum()
            };
            let mut content = String::with_capacity(required);
            let complete = {
                let table = crate::fs::vfs::MOUNT_TABLE.exclusive_access();
                let exact_size: usize = table
                    .iter()
                    .map(|mount| {
                        PREFIX.len()
                            + mount.mnt_point.path.len()
                            + 1
                            + mount.mnt_root.super_block.fs_type.len()
                            + SUFFIX.len()
                    })
                    .sum();
                if exact_size > content.capacity() {
                    false
                } else {
                    for mount in table.iter() {
                        content.push_str(PREFIX);
                        content.push_str(&mount.mnt_point.path);
                        content.push(' ');
                        content.push_str(mount.mnt_root.super_block.fs_type);
                        content.push_str(SUFFIX);
                    }
                    true
                }
            };
            if complete {
                break content;
            }
        };
        Self {
            offset: VfsLock::new_named(0, "ProcMountsOffset"),
            content,
        }
    }
}

impl File for ProcMountsFile {
    // /proc/mounts 可以读。
    // 执行时直接返回 true，表示可以读取打开时保存的挂载表快照。
    fn readable(&self) -> bool {
        true
    }
    // /proc/mounts 不允许写。
    // 执行时直接返回 false，表示挂载表伪文件只读。
    fn writable(&self) -> bool {
        false
    }
    // 从打开时保存的挂载表文本里按 offset 读。
    // 执行时从 content 的当前 offset 拷贝一段到 buf，读到末尾后返回 0。
    fn read(&self, buf: &mut [u8]) -> isize {
        read_with_offset(&self.offset, self.content.as_bytes(), buf)
    }
    // 这个伪文件只读。
    // 执行时直接返回 -1，因为 /proc/mounts 不支持写入。
    fn write(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    // 返回 /proc/mounts 的普通只读文件 stat。
    // 执行时用保存的 content 长度填 st_size，并设置成普通只读文件。
    fn stat(&self) -> Kstat {
        let mut st = Kstat::new(super::S_IFREG);
        st.st_size = self.content.len() as i64;
        st.st_mode = super::S_IFREG | 0o444;
        st
    }
    // 给 fd 层做具体类型判断。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct ProcMeminfoFile {
    offset: VfsLock<usize>,
    content: String,
}

impl ProcMeminfoFile {
    // 打开 /proc/meminfo 时按当前页帧统计生成文本。
    // 执行时读取总页帧和空闲页帧数量，换算成 kB 后生成 meminfo 文本快照。
    pub fn new() -> Self {
        let page_size = crate::config::PAGE_SIZE;
        let total_pages = crate::mm::total_frames();
        let available_pages = crate::mm::available_frames();
        let total_kb = (total_pages * page_size) / 1024;
        let free_kb = (available_pages * page_size) / 1024;
        let content = alloc::format!(
            "MemTotal:       {} kB\nMemFree:        {} kB\nMemAvailable:   {} kB\n\
             Buffers:        0 kB\nCached:         0 kB\nSwapCached:     0 kB\n\
             SwapTotal:      0 kB\nSwapFree:       0 kB\n",
            total_kb,
            free_kb,
            free_kb
        );
        Self {
            offset: VfsLock::new_named(0, "ProcMeminfoOffset"),
            content,
        }
    }
}

impl File for ProcMeminfoFile {
    // /proc/meminfo 可以读。
    // 执行时直接返回 true，表示可以读取打开时保存的内存信息文本。
    fn readable(&self) -> bool {
        true
    }
    // /proc/meminfo 不允许写。
    // 执行时直接返回 false，表示内存信息伪文件只读。
    fn writable(&self) -> bool {
        false
    }
    // 从打开时生成的 meminfo 文本里按 offset 读。
    // 执行时从 content 的当前 offset 拷贝一段到 buf，读完后推进 offset。
    fn read(&self, buf: &mut [u8]) -> isize {
        read_with_offset(&self.offset, self.content.as_bytes(), buf)
    }
    // 这个伪文件只读。
    // 执行时直接返回 -1，因为 /proc/meminfo 不接受写入。
    fn write(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    // 返回 /proc/meminfo 的普通只读文件 stat。
    // 执行时用保存的 content 长度填 st_size，并设置成普通只读文件。
    fn stat(&self) -> Kstat {
        let mut st = Kstat::new(super::S_IFREG);
        st.st_size = self.content.len() as i64;
        st.st_mode = super::S_IFREG | 0o444;
        st
    }
    // 给 fd 层做具体类型判断。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct ProcStaticFile {
    offset: VfsLock<usize>,
    content: &'static str,
}

impl ProcStaticFile {
    pub fn new(content: &'static str) -> Self {
        Self {
            offset: VfsLock::new_named(0, "ProcStaticOffset"),
            content,
        }
    }
}

impl File for ProcStaticFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, buf: &mut [u8]) -> isize {
        read_with_offset(&self.offset, self.content.as_bytes(), buf)
    }
    fn write(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    fn stat(&self) -> Kstat {
        let mut st = Kstat::new(super::S_IFREG);
        st.st_size = self.content.len() as i64;
        st.st_mode = super::S_IFREG | 0o444;
        st
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct PseudoDirFile {
    offset: VfsLock<usize>,
    entries: &'static [(&'static str, u8)],
}

impl PseudoDirFile {
    pub fn new(entries: &'static [(&'static str, u8)]) -> Self {
        Self {
            offset: VfsLock::new_named(0, "PseudoDirOffset"),
            entries,
        }
    }

    pub fn sys_root() -> Self {
        Self::new(&SYS_ROOT_ENTRIES)
    }

    pub fn sys_block() -> Self {
        Self::new(&SYS_BLOCK_ENTRIES)
    }

    pub fn sys_block_device() -> Self {
        Self::new(&SYS_BLOCK_DEVICE_ENTRIES)
    }

    pub fn sys_dev() -> Self {
        Self::new(&SYS_DEV_ENTRIES)
    }

    pub fn sys_dev_block() -> Self {
        Self::new(&SYS_DEV_BLOCK_ENTRIES)
    }

    pub fn sys_dev_block_device() -> Self {
        Self::new(&SYS_DEV_BLOCK_DEVICE_ENTRIES)
    }
}

impl File for PseudoDirFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    fn write(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    fn is_dir(&self) -> bool {
        true
    }
    fn stat(&self) -> Kstat {
        let mut st = Kstat::new(super::S_IFDIR | 0o555);
        st.st_nlink = 2 + self.entries.len() as u32;
        st
    }
    fn getdents64(&self, buf: &mut [u8]) -> isize {
        const DIRENT64_FIXED_SIZE: usize = 8 + 8 + 2 + 1;
        let mut entries: Vec<(&str, u8)> = Vec::new();
        entries.push((".", DT_DIR));
        entries.push(("..", DT_DIR));
        for entry in self.entries {
            entries.push(*entry);
        }

        let mut inner = self.offset.exclusive_access();
        let mut written = 0usize;
        let mut idx = *inner;

        while idx < entries.len() {
            let (name, de_type) = entries[idx];
            let name_bytes = name.as_bytes();
            let record_size =
                crate::fs::inode::OSInode::align_up(DIRENT64_FIXED_SIZE + name_bytes.len() + 1, 8);
            if written + record_size > buf.len() {
                if written == 0 {
                    return -1;
                }
                break;
            }

            let record = &mut buf[written..written + record_size];
            record.fill(0);
            record[0..8].copy_from_slice(&((idx + 1) as u64).to_le_bytes());
            record[8..16].copy_from_slice(&((idx as i64) + 1).to_le_bytes());
            record[16..18].copy_from_slice(&(record_size as u16).to_le_bytes());
            record[18] = de_type;
            record[DIRENT64_FIXED_SIZE..DIRENT64_FIXED_SIZE + name_bytes.len()]
                .copy_from_slice(name_bytes);
            record[DIRENT64_FIXED_SIZE + name_bytes.len()] = 0;

            written += record_size;
            idx += 1;
        }

        *inner = idx;
        written as isize
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct SysBlockStatFile {
    offset: VfsLock<usize>,
    content: String,
}

impl SysBlockStatFile {
    pub fn new() -> Self {
        let sectors = crate::fs::block_written_sectors();
        let io_ticks = sectors.max(1);
        let content = alloc::format!("0 0 0 0 0 0 {} 0 0 {} 0 0 0 0 0 0 0\n", sectors, io_ticks);
        Self {
            offset: VfsLock::new_named(0, "SysBlockStatOffset"),
            content,
        }
    }
}

impl File for SysBlockStatFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, buf: &mut [u8]) -> isize {
        read_with_offset(&self.offset, self.content.as_bytes(), buf)
    }
    fn write(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    fn stat(&self) -> Kstat {
        let mut st = Kstat::new(super::S_IFREG | 0o444);
        st.st_size = self.content.len() as i64;
        st
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct DevRtcFile;

impl File for DevRtcFile {
    // /dev/rtc 允许读，不过当前实现只返回 EOF。
    // 执行时直接返回 true，让依赖 /dev/rtc 可读性的程序能继续运行。
    fn readable(&self) -> bool {
        true
    }
    // /dev/rtc 不允许写。
    // 执行时直接返回 false，当前 RTC 伪设备不接受写请求。
    fn writable(&self) -> bool {
        false
    }
    // 暂时不给真实 RTC 数据，直接返回 EOF。
    // 执行时不填充 buf，直接返回 0，表现成已经读到文件末尾。
    fn read(&self, _buf: &mut [u8]) -> isize {
        0
    }
    // RTC 设备当前只读。
    // 执行时直接返回 -1，表示当前没有实现 RTC 写入控制。
    fn write(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    // 返回 /dev/rtc 的字符设备 stat。
    // 执行时构造字符设备 Kstat，填入只读权限和 rtc 的设备号。
    fn stat(&self) -> Kstat {
        let mut st = Kstat::new(0x2000);
        st.st_mode = 0o444 | 0x2000;
        st.st_rdev = 0x0a00;
        st
    }
    // 给 fd 层做具体类型判断。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
