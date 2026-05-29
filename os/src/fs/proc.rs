use super::{File, Kstat};
use crate::sync::UPSafeCell;
use crate::task::PID2TCB;
use alloc::string::String;
use alloc::vec::Vec;

pub struct ProcDirFile {
    offset: UPSafeCell<usize>,
}

impl ProcDirFile {
    pub fn new() -> Self {
        Self {
            offset: unsafe { UPSafeCell::new(0) },
        }
    }
}

impl File for ProcDirFile {
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
        let mut st = Kstat::new(0x4000);
        st.st_mode = 0o555 | 0x4000;
        st
    }
    fn getdents64(&self, buf: &mut [u8]) -> isize {
        const DIRENT64_FIXED_SIZE: usize = 8 + 8 + 2 + 1;
        let mut inner = self.offset.exclusive_access();

        let mut pids: Vec<usize> = PID2TCB.exclusive_access().keys().copied().collect();
        pids.sort();

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

            let name_with_null = alloc::format!("{}\0", name);
            let copy_len = name_with_null.len().min(record_size - DIRENT64_FIXED_SIZE);
            record[19..19 + copy_len].copy_from_slice(&name_with_null.as_bytes()[..copy_len]);

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

pub struct ProcPidStatFile {
    pid: usize,
    offset: UPSafeCell<usize>,
}

impl ProcPidStatFile {
    pub fn new(pid: usize) -> Self {
        Self {
            pid,
            offset: unsafe { UPSafeCell::new(0) },
        }
    }

    fn build_content(&self) -> String {
        use alloc::format;
        if let Some(task) = crate::task::pid2task(self.pid) {
            let inner = task.inner_exclusive_access();
            let state = match inner.task_status {
                crate::task::TaskStatus::Running => 'R',
                crate::task::TaskStatus::Zombie => 'Z',
                _ => 'S',
            };
            let cmd = if let Some(f) = inner.fd_table.get(0).and_then(|x| x.as_ref()) {
                f.path().unwrap_or_else(|| String::from("?"))
            } else {
                String::from("initproc")
            };
            let ppid = inner
                .parent
                .as_ref()
                .and_then(|w| w.upgrade())
                .map(|p| p.getpid())
                .unwrap_or(0);
            format!("{} ({}) {} {} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
                self.pid, cmd, state, ppid)
        } else {
            String::new()
        }
    }
}

impl File for ProcPidStatFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, buf: &mut [u8]) -> isize {
        let mut off = self.offset.exclusive_access();
        let content = self.build_content();
        let bytes = content.as_bytes();
        if *off >= bytes.len() {
            return 0;
        }
        let len = buf.len().min(bytes.len() - *off);
        buf[..len].copy_from_slice(&bytes[*off..*off + len]);
        *off += len;
        len as isize
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

pub struct ProcMountsFile {
    offset: UPSafeCell<usize>,
    content: String,
}

impl ProcMountsFile {
    pub fn new() -> Self {
        let mut content = String::new();
        let table = crate::fs::vfs::MOUNT_TABLE.exclusive_access();
        for m in table.iter() {
            let device = "none";
            let fstype = m.mnt_root.super_block.fs_type;
            let mp = &m.mnt_point.path;
            content.push_str(&alloc::format!("{device} {mp} {fstype} rw,relatime 0 0\n"));
        }
        Self {
            offset: unsafe { UPSafeCell::new(0) },
            content,
        }
    }
}

impl File for ProcMountsFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, buf: &mut [u8]) -> isize {
        let mut offset = self.offset.exclusive_access();
        let bytes = self.content.as_bytes();
        if *offset >= bytes.len() {
            return 0;
        }
        let len = buf.len().min(bytes.len() - *offset);
        buf[..len].copy_from_slice(&bytes[*offset..*offset + len]);
        *offset += len;
        len as isize
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

pub struct ProcMeminfoFile {
    offset: UPSafeCell<usize>,
    content: String,
}

impl ProcMeminfoFile {
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
            offset: unsafe { UPSafeCell::new(0) },
            content,
        }
    }
}

impl File for ProcMeminfoFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, buf: &mut [u8]) -> isize {
        let mut offset = self.offset.exclusive_access();
        let bytes = self.content.as_bytes();
        if *offset >= bytes.len() {
            return 0;
        }
        let len = buf.len().min(bytes.len() - *offset);
        buf[..len].copy_from_slice(&bytes[*offset..*offset + len]);
        *offset += len;
        len as isize
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

pub struct DevRtcFile;

impl File for DevRtcFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, _buf: &mut [u8]) -> isize {
        0
    }
    fn write(&self, _buf: &mut [u8]) -> isize {
        -1
    }
    fn stat(&self) -> Kstat {
        let mut st = Kstat::new(0x2000);
        st.st_mode = 0o444 | 0x2000;
        st.st_rdev = 0x0a00;
        st
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
