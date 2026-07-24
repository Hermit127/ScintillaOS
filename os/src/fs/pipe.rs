use super::{File, Kstat, S_IFIFO};
use crate::sync::DeviceLock;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::task::{
    block_current_and_run_next_if, current_add_signal, current_task, has_interrupting_signal,
    wake_pipe_read_tasks, wake_pipe_write_tasks, wake_poll_wait_tasks, BlockReason, SignalFlags,
};

use crate::fs::stdio::stdout_release_if_owner;
use core::sync::atomic::{AtomicBool, Ordering};

const O_NONBLOCK: usize = 0x800;

fn current_task_interrupted() -> bool {
    current_task()
        .and_then(|task| {
            task.try_inner_borrow()
                .map(|inner| has_interrupting_signal(&inner))
        })
        .unwrap_or(false)
}

pub struct Pipe {
    readable: bool,
    writable: bool,
    // The ring is shared by both pipe endpoints and may be touched by
    // different harts.  Keep the device-domain lock local to the pipe rather
    // than making the whole VFS path contend on one global lock.
    buffer: Arc<DeviceLock<PipeRingBuffer>>,
    nonblock: AtomicBool,
}

impl Pipe {
    pub fn read_end_with_buffer(buffer: Arc<DeviceLock<PipeRingBuffer>>) -> Self {
        Self {
            readable: true,
            writable: false,
            buffer,
            nonblock: AtomicBool::new(false),
        }
    }
    pub fn write_end_with_buffer(buffer: Arc<DeviceLock<PipeRingBuffer>>) -> Self {
        Self {
            readable: false,
            writable: true,
            buffer,
            nonblock: AtomicBool::new(false),
        }
    }
    fn wait_key(&self) -> usize {
        Arc::as_ptr(&self.buffer) as usize
    }
    fn is_nonblock(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }
}

const RING_BUFFER_SIZE: usize = 8192;

#[derive(Copy, Clone, PartialEq)]
enum RingBufferStatus {
    Full,
    Empty,
    Normal,
}

pub struct PipeRingBuffer {
    arr: Vec<u8>,
    head: usize,
    tail: usize,
    status: RingBufferStatus,
    read_open: usize,
    write_open: usize,
}

impl PipeRingBuffer {
    pub fn new() -> Self {
        Self {
            arr: vec![0; RING_BUFFER_SIZE],
            head: 0,
            tail: 0,
            status: RingBufferStatus::Empty,
            read_open: 1,
            write_open: 1,
        }
    }
    pub fn write_buf(&mut self, src: &[u8]) -> usize {
        let count = src.len().min(self.available_write());
        if count == 0 {
            return 0;
        }
        let first = count.min(RING_BUFFER_SIZE - self.tail);
        self.arr[self.tail..self.tail + first].copy_from_slice(&src[..first]);
        self.tail = (self.tail + first) % RING_BUFFER_SIZE;
        if count > first {
            let second = count - first;
            self.arr[..second].copy_from_slice(&src[first..count]);
            self.tail = second;
        }
        self.status = if self.tail == self.head {
            RingBufferStatus::Full
        } else {
            RingBufferStatus::Normal
        };
        count
    }
    pub fn read_buf(&mut self, dst: &mut [u8]) -> usize {
        let count = dst.len().min(self.available_read());
        if count == 0 {
            return 0;
        }
        let first = count.min(RING_BUFFER_SIZE - self.head);
        dst[..first].copy_from_slice(&self.arr[self.head..self.head + first]);
        self.head = (self.head + first) % RING_BUFFER_SIZE;
        if count > first {
            let second = count - first;
            dst[first..count].copy_from_slice(&self.arr[..second]);
            self.head = second;
        }
        self.status = if self.head == self.tail {
            RingBufferStatus::Empty
        } else {
            RingBufferStatus::Normal
        };
        count
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
        self.write_open == 0
    }
    pub fn all_read_ends_closed(&self) -> bool {
        self.read_open == 0
    }
    pub fn add_read_end(&mut self) {
        self.read_open += 1;
    }
    pub fn add_write_end(&mut self) {
        self.write_open += 1;
    }
    pub fn close_read_end(&mut self) -> bool {
        if self.read_open > 0 {
            self.read_open -= 1;
        }
        self.read_open == 0
    }
    pub fn close_write_end(&mut self) -> bool {
        if self.write_open > 0 {
            self.write_open -= 1;
        }
        self.write_open == 0
    }
}

/// Return (read_end, write_end)
pub fn make_pipe() -> (Arc<Pipe>, Arc<Pipe>) {
    let buffer = Arc::new(DeviceLock::new_named(
        PipeRingBuffer::new(),
        "PipeRingBuffer",
    ));
    let read_end = Arc::new(Pipe::read_end_with_buffer(buffer.clone()));
    let write_end = Arc::new(Pipe::write_end_with_buffer(buffer.clone()));
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
        let mut already_read = 0usize;
        loop {
            if current_task_interrupted() {
                return -4;
            }
            let (loop_read, write_end_closed) = {
                let ring_buffer = self.buffer.exclusive_access();
                let n = ring_buffer.available_read();
                let closed = n == 0 && ring_buffer.all_write_ends_closed();
                (n, closed)
            };
            if loop_read == 0 {
                if already_read > 0 {
                    return already_read as isize;
                }
                if write_end_closed {
                    return already_read as isize;
                }
                if self.is_nonblock() {
                    return -11;
                }
                stdout_release_if_owner();
                let key = self.wait_key();
                block_current_and_run_next_if(BlockReason::PipeRead(key), || {
                    let ring_buffer = self.buffer.exclusive_access();
                    ring_buffer.available_read() == 0
                        && !ring_buffer.all_write_ends_closed()
                });
                continue;
            }
            let mut ring_buffer = self.buffer.exclusive_access();
            let n = ring_buffer.read_buf(&mut buf[already_read..want_to_read]);
            already_read += n;
            drop(ring_buffer);
            if n > 0 {
                wake_pipe_write_tasks(self.wait_key());
                wake_poll_wait_tasks();
            }
            if already_read == want_to_read || already_read > 0 {
                return already_read as isize;
            }
        }
    }
    fn write(&self, buf: &mut [u8]) -> isize {
        assert!(self.writable());
        let want_to_write = buf.len();
        if want_to_write == 0 {
            return 0;
        }
        let mut already_write = 0usize;
        loop {
            if current_task_interrupted() {
                return -4;
            }
            let (loop_write, read_end_closed) = {
                let ring_buffer = self.buffer.exclusive_access();
                (
                    ring_buffer.available_write(),
                    ring_buffer.all_read_ends_closed(),
                )
            };
            if read_end_closed {
                current_add_signal(SignalFlags::SIGPIPE);
                return -32;
            }
            if loop_write == 0 {
                if self.is_nonblock() {
                    return -11;
                }
                stdout_release_if_owner();
                let key = self.wait_key();
                block_current_and_run_next_if(BlockReason::PipeWrite(key), || {
                    let ring_buffer = self.buffer.exclusive_access();
                    ring_buffer.available_write() == 0
                        && !ring_buffer.all_read_ends_closed()
                });
                continue;
            }
            let mut ring_buffer = self.buffer.exclusive_access();
            let n = ring_buffer.write_buf(&buf[already_write..want_to_write]);
            already_write += n;
            drop(ring_buffer);
            if n > 0 {
                wake_pipe_read_tasks(self.wait_key());
                wake_poll_wait_tasks();
            }
            if already_write == want_to_write {
                return want_to_write as isize;
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
    fn get_flags(&self) -> usize {
        let mut flags = 0x8000usize;
        if self.writable {
            flags |= 1;
        }
        if self.is_nonblock() {
            flags |= O_NONBLOCK;
        }
        flags
    }
    fn set_flags(&self, flags: usize) -> bool {
        self.nonblock
            .store((flags & O_NONBLOCK) != 0, Ordering::Relaxed);
        true
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
        let mut last_read_closed = false;
        let mut last_write_closed = false;
        if self.readable {
            last_read_closed = ring_buffer.close_read_end();
        }
        if self.writable {
            last_write_closed = ring_buffer.close_write_end();
        }
        drop(ring_buffer);
        if last_read_closed || last_write_closed {
            let key = self.wait_key();
            if last_read_closed {
                wake_pipe_write_tasks(key);
            }
            if last_write_closed {
                wake_pipe_read_tasks(key);
            }
            wake_poll_wait_tasks();
        }
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
