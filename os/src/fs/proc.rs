use super::{File, Kstat};
use crate::fs::vfs::MOUNT_TABLE;
use crate::sync::UPSafeCell;
use alloc::string::String;

pub struct ProcMountsFile {
    offset: UPSafeCell<usize>,
    content: String,
}

impl ProcMountsFile {
    pub fn new() -> Self {
        let mut content = String::new();
        let table = MOUNT_TABLE.exclusive_access();
        for m in table.iter() {
            let device = "none";
            let fstype = m.mnt_root.super_block.fs_type;
            let mp = &m.mnt_point.path;
            content.push_str(&alloc::format!(
                "{device} {mp} {fstype} rw,relatime 0 0\n"
            ));
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
