use polyhal::debug_console::DebugConsole;

use super::{File, Kstat, S_IFCHR};
use crate::task::{current_task, suspend_current_and_run_next};
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
            return;
        }
        suspend_current_and_run_next();
    }
}

pub fn stdout_release_if_owner() {
    if let Some(task) = current_task() {
        let pid = task.getpid() as i32;
        STDOUT_OWNER
            .compare_exchange(pid, 0, Ordering::Release, Ordering::Relaxed)
            .ok();
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
        stdout_release_if_owner();
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

pub struct DevZero;

impl File for DevZero {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        true
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
