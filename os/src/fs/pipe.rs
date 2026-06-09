use super::{File, Kstat, S_IFIFO};
use crate::sync::UPSafeCell;
use alloc::sync::{Arc, Weak};

use crate::task::{current_task, suspend_current_and_run_next, wake_poll_wait_tasks};
use crate::task::has_pending_signals;

use crate::fs::stdio::stdout_release_if_owner;

fn current_task_killed() -> bool {
    current_task()
        .and_then(|task| task.try_inner_borrow().map(|inner| inner.killed))
        .unwrap_or(false)
}

pub struct Pipe {
    readable: bool,
    writable: bool,
    buffer: Arc<UPSafeCell<PipeRingBuffer>>,
}

impl Pipe {
    pub fn read_end_with_buffer(buffer: Arc<UPSafeCell<PipeRingBuffer>>) -> Self {
        Self {
            readable: true,
            writable: false,
            buffer,
        }
    }
    pub fn write_end_with_buffer(buffer: Arc<UPSafeCell<PipeRingBuffer>>) -> Self {
        Self {
            readable: false,
            writable: true,
            buffer,
        }
    }
}

const RING_BUFFER_SIZE: usize = 4096;

#[derive(Copy, Clone, PartialEq)]
enum RingBufferStatus {
    Full,
    Empty,
    Normal,
}

pub struct PipeRingBuffer {
    arr: [u8; RING_BUFFER_SIZE],
    head: usize,
    tail: usize,
    status: RingBufferStatus,
    read_open: usize,
    write_open: usize,
    write_end: Option<Weak<Pipe>>,
    read_end: Option<Weak<Pipe>>,
}

impl PipeRingBuffer {
    pub fn new() -> Self {
        Self {
            arr: [0; RING_BUFFER_SIZE],
            head: 0,
            tail: 0,
            status: RingBufferStatus::Empty,
            read_open: 1,
            write_open: 1,
            write_end: None,
            read_end: None,
        }
    }
    pub fn set_write_end(&mut self, write_end: &Arc<Pipe>) {
        self.write_end = Some(Arc::downgrade(write_end));
    }
    pub fn set_read_end(&mut self, read_end: &Arc<Pipe>) {
        self.read_end = Some(Arc::downgrade(read_end));
    }
    pub fn write_byte(&mut self, byte: u8) {
        self.status = RingBufferStatus::Normal;
        self.arr[self.tail] = byte;
        self.tail = (self.tail + 1) % RING_BUFFER_SIZE;
        if self.tail == self.head {
            self.status = RingBufferStatus::Full;
        }
    }
    pub fn read_byte(&mut self) -> u8 {
        self.status = RingBufferStatus::Normal;
        let c = self.arr[self.head];
        self.head = (self.head + 1) % RING_BUFFER_SIZE;
        if self.head == self.tail {
            self.status = RingBufferStatus::Empty;
        }
        c
    }
    pub fn available_read(&self) -> usize {
        if self.status == RingBufferStatus::Empty {
            0
        } else if self.tail > self.head {
            self.tail - self.head
        } else {
            self.tail + RING_BUFFER_SIZE - self.head
        }
    }
    pub fn available_write(&self) -> usize {
        if self.status == RingBufferStatus::Full {
            0
        } else {
            RING_BUFFER_SIZE - self.available_read()
        }
    }
    pub fn all_write_ends_closed(&self) -> bool {
        self.write_end.as_ref().unwrap().upgrade().is_none() && self.write_open == 0
    }
    pub fn all_read_ends_closed(&self) -> bool {
        self.read_end.as_ref().map_or(true, |r| r.upgrade().is_none()) && self.read_open == 0
    }
    pub fn add_read_end(&mut self) {
        self.read_open += 1;
    }
    pub fn add_write_end(&mut self) {
        self.write_open += 1;
    }
    pub fn close_read_end(&mut self) {
        if self.read_open > 0 {
            self.read_open -= 1;
        }
    }
    pub fn close_write_end(&mut self) {
        if self.write_open > 0 {
            self.write_open -= 1;
        }
    }
}

/// Return (read_end, write_end)
pub fn make_pipe() -> (Arc<Pipe>, Arc<Pipe>) {
    let buffer = Arc::new(unsafe { UPSafeCell::new_named(PipeRingBuffer::new(), "PipeRingBuffer") });
    let read_end = Arc::new(Pipe::read_end_with_buffer(buffer.clone()));
    let write_end = Arc::new(Pipe::write_end_with_buffer(buffer.clone()));
    buffer.exclusive_access().set_read_end(&read_end);
    (read_end, write_end)
}

impl File for Pipe {
    fn readable(&self) -> bool {
        self.readable
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn read(&self, buf: &mut [u8]) -> isize {
        assert!(self.readable());
        let want_to_read = buf.len();
        if want_to_read == 0 {
            return 0;
        }
        let mut buf_iter = buf.into_iter();
        let mut already_read = 0usize;
        loop {
            if current_task_killed() {
                return -4;
            }
            let (loop_read, write_end_closed) = {
                let ring_buffer = self.buffer.exclusive_access();
                let n = ring_buffer.available_read();
                let closed = n == 0 && ring_buffer.all_write_ends_closed();
                (n, closed)
            };
            if loop_read == 0 {
                if write_end_closed {
                    return already_read as isize;
                }
                if already_read > 0 {
                    return already_read as isize;
                }
                if has_pending_signals() {
                    return already_read as isize;
                }
                stdout_release_if_owner();
                suspend_current_and_run_next();
                continue;
            }
            let mut ring_buffer = self.buffer.exclusive_access();
            let mut made_progress = false;
            for _ in 0..loop_read {
                if let Some(byte_ref) = buf_iter.next() {
                    *byte_ref = ring_buffer.read_byte();
                    already_read += 1;
                    made_progress = true;
                    if already_read == want_to_read {
                        drop(ring_buffer);
                        wake_poll_wait_tasks();
                        return want_to_read as isize;
                    }
                } else {
                    drop(ring_buffer);
                    if made_progress {
                        wake_poll_wait_tasks();
                    }
                    return already_read as isize;
                }
            }
            drop(ring_buffer);
            if made_progress {
                wake_poll_wait_tasks();
            }
        }
    }
    fn write(&self, buf: &mut [u8]) -> isize {
        assert!(self.writable());
        let mut already_write = 0usize;
        // Check if read end is closed → EPIPE
        if self.buffer.exclusive_access().all_read_ends_closed() {
            return -32;
        }
        let want_to_write = buf.len();
        if want_to_write == 0 {
            return 0;
        }
        let mut buf_iter = buf.into_iter();
        loop {
            if current_task_killed() {
                return -4;
            }
            let (loop_write, read_end_closed) = {
                let ring_buffer = self.buffer.exclusive_access();
                (ring_buffer.available_write(), ring_buffer.all_read_ends_closed())
            };
            if read_end_closed {
                return -32;
            }
            if loop_write == 0 {
                return already_write as isize;
            }
            let mut ring_buffer = self.buffer.exclusive_access();
            let mut made_progress = false;
            for _ in 0..loop_write {
                if let Some(byte_ref) = buf_iter.next() {
                    ring_buffer.write_byte(*byte_ref);
                    already_write += 1;
                    made_progress = true;
                    if already_write == want_to_write {
                        drop(ring_buffer);
                        wake_poll_wait_tasks();
                        return want_to_write as isize;
                    }
                } else {
                    drop(ring_buffer);
                    if made_progress {
                        wake_poll_wait_tasks();
                    }
                    return already_write as isize;
                }
            }
            drop(ring_buffer);
            if made_progress {
                wake_poll_wait_tasks();
            }
        }
    }
    fn poll(&self) -> (bool, bool, bool) {
        let ring_buffer = self.buffer.exclusive_access();
        if self.readable {
            let has_data = ring_buffer.available_read() > 0;
            let write_closed = ring_buffer.all_write_ends_closed();
            (has_data || write_closed, false, false)
        } else {
            let has_space = ring_buffer.available_write() > 0;
            let read_closed = ring_buffer.all_read_ends_closed();
            (false, has_space, read_closed)
        }
    }
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFIFO)
    }
    fn dup_ref(&self) {
        let mut ring_buffer = self.buffer.exclusive_access();
        if self.readable {
            ring_buffer.add_read_end();
        }
        if self.writable {
            ring_buffer.add_write_end();
        }
    }
    fn close_ref(&self) {
        let mut ring_buffer = self.buffer.exclusive_access();
        let mut changed = false;
        if self.readable {
            ring_buffer.close_read_end();
            changed = true;
        }
        if self.writable {
            ring_buffer.close_write_end();
            changed = true;
        }
        drop(ring_buffer);
        if changed {
            wake_poll_wait_tasks();
        }
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
