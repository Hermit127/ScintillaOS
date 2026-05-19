use polyhal::debug_console::DebugConsole;

use super::{File, Kstat, S_IFCHR};
use crate::task::suspend_current_and_run_next;

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
        assert_eq!(user_buf.len(), 1);
        let c: u8;
        loop {
            if let Some(ch) = DebugConsole::getchar() {
                c = ch;
                break;
            }
            suspend_current_and_run_next();
        }
        user_buf[0] = c as u8;
        1
    }
    fn write(&self, _user_buf: &mut [u8]) -> isize {
        panic!("Cannot write to stdin!");
    }
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFCHR)
    }
    fn as_any(&self) -> &dyn core::any::Any { self }
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
        // for buffer in user_buf.buffers.iter() {
        //     print!("{}", core::str::from_utf8(*buffer).unwrap());
        // }
        print!("{}", core::str::from_utf8(user_buf).unwrap());
        user_buf.len() as isize
    }
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFCHR)
    }
    fn as_any(&self) -> &dyn core::any::Any { self }
}

pub struct Urandom;

impl File for Urandom {
    fn readable(&self) -> bool { true }
    fn writable(&self) -> bool { false }
    fn read(&self, buf: &mut [u8]) -> isize {
        let mut state: u32 = 0x92f4a3b7u32.wrapping_add(polyhal::time::Time::now().to_msec() as u32);
        for b in buf.iter_mut() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *b = state as u8;
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
        }
        buf.len() as isize
    }
    fn write(&self, _buf: &mut [u8]) -> isize { 0 }
    fn stat(&self) -> Kstat { Kstat::new(S_IFCHR) }
    fn as_any(&self) -> &dyn core::any::Any { self }
}

pub struct DevNull;

impl File for DevNull {
    fn readable(&self) -> bool { true }
    fn writable(&self) -> bool { true }
    fn read(&self, _buf: &mut [u8]) -> isize { 0 }
    fn write(&self, buf: &mut [u8]) -> isize { buf.len() as isize }
    fn stat(&self) -> Kstat { Kstat::new(S_IFCHR) }
    fn as_any(&self) -> &dyn core::any::Any { self }
}
