use polyhal::debug_console::DebugConsole;

use super::{File, Kstat, S_IFBLK, S_IFCHR, S_IFREG, S_IFSOCK};
use crate::sync::UPSafeCell;
use crate::task::{current_task, suspend_current_and_run_next};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static STDOUT_BUSY: AtomicBool = AtomicBool::new(false);
static STDOUT_OWNER: AtomicI32 = AtomicI32::new(0);

fn stdout_acquire() {
    let pid = current_task().unwrap().getpid() as i32;
    loop {
        let owner = STDOUT_OWNER.load(Ordering::Acquire);
        if owner == pid {
            return;
        }
        if owner == 0
            && STDOUT_OWNER
                .compare_exchange(0, pid, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            STDOUT_BUSY.store(false, Ordering::Release);
            return;
        }
        suspend_current_and_run_next();
    }
}

pub fn stdout_release_if_owner() {
    if let Some(task) = current_task() {
        let pid = task.getpid() as i32;
        if STDOUT_OWNER
            .compare_exchange(pid, 0, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            STDOUT_BUSY.store(false, Ordering::Release);
        } else if STDOUT_OWNER.load(Ordering::Acquire) == 0 {
            STDOUT_BUSY.store(false, Ordering::Release);
        }
    }
}

pub fn stdout_release_pid(pid: usize) {
    if STDOUT_OWNER
        .compare_exchange(pid as i32, 0, Ordering::Release, Ordering::Relaxed)
        .is_ok()
    {
        STDOUT_BUSY.store(false, Ordering::Release);
    } else if STDOUT_OWNER.load(Ordering::Acquire) == 0 {
        STDOUT_BUSY.store(false, Ordering::Release);
    }
}

pub struct Stdin;

pub struct Stdout;

impl File for Stdin {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, user_buf: &mut [u8]) -> isize {
        if user_buf.is_empty() {
            return 0;
        }
        loop {
            if let Some(c) = DebugConsole::getchar() {
                user_buf[0] = c;
                return 1;
            }
            stdout_release_if_owner();
            suspend_current_and_run_next();
        }
    }
    fn write(&self, _user_buf: &mut [u8]) -> isize {
        panic!("Cannot write to stdin!");
    }
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFCHR)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl File for Stdout {
    fn readable(&self) -> bool {
        false
    }
    fn writable(&self) -> bool {
        true
    }
    fn read(&self, _user_buf: &mut [u8]) -> isize {
        panic!("Cannot read from stdout!");
    }
    fn write(&self, user_buf: &mut [u8]) -> isize {
        stdout_acquire();
        while STDOUT_BUSY.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        for byte in user_buf.iter() {
            DebugConsole::putchar(*byte);
        }
        STDOUT_BUSY.store(false, Ordering::Release);
        // Do NOT release stdout here; keep ownership until the process yields.
        // This prevents interleaved output when userspace splits a logical
        // printf into multiple write() calls (e.g. loongarch64 musl).
        user_buf.len() as isize
    }
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFCHR)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct Urandom;

impl File for Urandom {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, buf: &mut [u8]) -> isize {
        let mut state: u32 =
            0x92f4a3b7u32.wrapping_add(polyhal::time::Time::now().to_msec() as u32);
        for b in buf.iter_mut() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *b = state as u8;
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
        }
        buf.len() as isize
    }
    fn write(&self, _buf: &mut [u8]) -> isize {
        0
    }
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFCHR)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct DevNull;

impl File for DevNull {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        true
    }
    fn read(&self, _buf: &mut [u8]) -> isize {
        0
    }
    fn write(&self, buf: &mut [u8]) -> isize {
        buf.len() as isize
    }
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFCHR)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct DevZero {
    readable: bool,
    writable: bool,
}

impl DevZero {
    pub fn new(readable: bool, writable: bool) -> Self {
        Self { readable, writable }
    }
}

impl File for DevZero {
    fn readable(&self) -> bool {
        self.readable
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn read(&self, buf: &mut [u8]) -> isize {
        for b in buf.iter_mut() {
            *b = 0;
        }
        buf.len() as isize
    }
    fn write(&self, buf: &mut [u8]) -> isize {
        buf.len() as isize
    }
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFCHR)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct DevBlock {
    index: usize,
}

impl DevBlock {
    pub fn new(index: usize) -> Self {
        Self { index }
    }

    pub fn capacity_bytes(&self) -> u64 {
        crate::drivers::block::get_block_device(self.index)
            .and_then(|dev| dev.capacity_bytes())
            .unwrap_or(512 * 1024 * 1024)
    }
}

impl File for DevBlock {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        true
    }
    fn read(&self, _buf: &mut [u8]) -> isize {
        -22
    }
    fn write(&self, _buf: &mut [u8]) -> isize {
        -22
    }
    fn stat(&self) -> Kstat {
        let mut st = Kstat::new(S_IFBLK | 0o660);
        st.st_rdev = 0xfe00 + self.index as u64;
        st.st_size = self.capacity_bytes() as i64;
        st
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct NullSocket;

impl File for NullSocket {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        true
    }
    fn read(&self, _buf: &mut [u8]) -> isize {
        -107
    }
    fn write(&self, _buf: &mut [u8]) -> isize {
        -107
    }
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFSOCK)
    }
    fn poll(&self) -> (bool, bool, bool) {
        (false, false, true)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct TmpFileInner {
    data: Vec<u8>,
    offset: usize,
    append: bool,
}

pub struct TmpFile {
    inner: UPSafeCell<TmpFileInner>,
}

impl TmpFile {
    pub fn new() -> Self {
        Self {
            inner: unsafe {
                UPSafeCell::new(TmpFileInner {
                    data: Vec::new(),
                    offset: 0,
                    append: false,
                })
            },
        }
    }

    fn write_at_inner(inner: &mut TmpFileInner, offset: usize, buf: &[u8]) -> isize {
        let Some(end) = offset.checked_add(buf.len()) else {
            return -27;
        };
        if end > inner.data.len() {
            inner.data.resize(end, 0);
        }
        inner.data[offset..end].copy_from_slice(buf);
        buf.len() as isize
    }
}

impl File for TmpFile {
    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, buf: &mut [u8]) -> isize {
        let mut inner = self.inner.exclusive_access();
        let available = inner.data.len().saturating_sub(inner.offset);
        let n = available.min(buf.len());
        if n > 0 {
            let start = inner.offset;
            let end = start + n;
            buf[..n].copy_from_slice(&inner.data[start..end]);
            inner.offset = end;
        }
        n as isize
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        let inner = self.inner.exclusive_access();
        let available = inner.data.len().saturating_sub(offset);
        let n = available.min(buf.len());
        if n > 0 {
            buf[..n].copy_from_slice(&inner.data[offset..offset + n]);
        }
        n as isize
    }

    fn write(&self, buf: &mut [u8]) -> isize {
        let mut inner = self.inner.exclusive_access();
        let offset = if inner.append {
            inner.data.len()
        } else {
            inner.offset
        };
        let n = Self::write_at_inner(&mut inner, offset, buf);
        if n > 0 {
            inner.offset = offset + n as usize;
        }
        n
    }

    fn write_at(&self, offset: usize, buf: &mut [u8]) -> isize {
        let mut inner = self.inner.exclusive_access();
        Self::write_at_inner(&mut inner, offset, buf)
    }

    fn stat(&self) -> Kstat {
        let inner = self.inner.exclusive_access();
        let mut st = Kstat::new(S_IFREG | 0o600);
        st.st_size = inner.data.len() as i64;
        st.st_blksize = 4096;
        st.st_blocks = ((inner.data.len() as u64) + 511) / 512;
        st
    }

    fn seek(&self, offset: usize) {
        self.inner.exclusive_access().offset = offset;
    }

    fn get_offset(&self) -> usize {
        self.inner.exclusive_access().offset
    }

    fn get_flags(&self) -> usize {
        let inner = self.inner.exclusive_access();
        let mut flags = 0x8000usize | 0x2;
        if inner.append {
            flags |= 0x400;
        }
        flags
    }

    fn set_flags(&self, flags: usize) -> bool {
        self.inner.exclusive_access().append = (flags & 0x400) != 0;
        true
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
