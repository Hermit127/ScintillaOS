// 这里负责操作系统的文件管理和进程通信：实现匿名管道、环形缓冲区以及管道 fd 的读写阻塞语义。
use super::{File, Kstat, S_IFIFO};
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::task::{
    block_current_and_run_next, current_task, wake_pipe_read_tasks, wake_pipe_write_tasks,
    wake_poll_wait_tasks, BlockReason,
};

use crate::fs::stdio::stdout_release_if_owner;

// 检查当前任务是否已经被信号标记为 killed，管道阻塞点会用它提前退出。
// 执行时尝试借用当前任务状态，能读到 killed 就返回它，当前没有任务或借用失败时按未 killed 处理。
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
    // 创建管道读端，和写端共享同一个环形缓冲区。
    // 执行时保存共享缓冲区，把 readable 设为 true、writable 设为 false，形成只读端点。
    pub fn read_end_with_buffer(buffer: Arc<UPSafeCell<PipeRingBuffer>>) -> Self {
        Self {
            readable: true,
            writable: false,
            buffer,
        }
    }
    // 创建管道写端，和读端共享同一个环形缓冲区。
    // 执行时保存共享缓冲区，把 readable 设为 false、writable 设为 true，形成只写端点。
    pub fn write_end_with_buffer(buffer: Arc<UPSafeCell<PipeRingBuffer>>) -> Self {
        Self {
            readable: false,
            writable: true,
            buffer,
        }
    }
    // 用缓冲区地址当等待队列 key，让同一根管道的读写端能互相唤醒。
    // 执行时取共享缓冲区 Arc 的裸指针地址，作为这根管道稳定的等待队列编号。
    fn wait_key(&self) -> usize {
        Arc::as_ptr(&self.buffer) as usize
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
    // 初始化空的环形缓冲区，默认各有一个读端和写端。
    // 执行时分配固定大小 Vec，head/tail 都放到 0，并把读端和写端引用数初始化为 1。
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
    // 往环形缓冲区写入尽可能多的数据，必要时绕回开头。
    // 执行时先按剩余空间截断写入长度，再分成尾部和开头两段拷贝，最后更新 tail 和状态。
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
    // 从环形缓冲区读出尽可能多的数据，必要时从开头续读。
    // 执行时先按已有数据截断读取长度，再分成尾部和开头两段拷贝，最后更新 head 和状态。
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
    // 计算当前还能读多少字节。
    // 执行时根据 Empty/Normal/Full 状态和 head/tail 位置计算环形缓冲区里的有效数据量。
    pub fn available_read(&self) -> usize {
        if self.status == RingBufferStatus::Empty {
            0
        } else if self.tail > self.head {
            self.tail - self.head
        } else {
            self.tail + RING_BUFFER_SIZE - self.head
        }
    }
    // 计算当前还能写多少字节。
    // 执行时如果已经 Full 就返回 0，否则用总容量减去当前可读字节数。
    pub fn available_write(&self) -> usize {
        if self.status == RingBufferStatus::Full {
            0
        } else {
            RING_BUFFER_SIZE - self.available_read()
        }
    }
    // 判断所有写端是否都已经关闭。
    // 执行时直接检查 write_open 是否为 0，用来让读端返回 EOF。
    pub fn all_write_ends_closed(&self) -> bool {
        self.write_open == 0
    }
    // 判断所有读端是否都已经关闭。
    // 执行时直接检查 read_open 是否为 0，用来让写端返回 EPIPE。
    pub fn all_read_ends_closed(&self) -> bool {
        self.read_open == 0
    }
    // 复制读端 fd 时增加读端计数。
    // 执行时把 read_open 加一，表示又有一个 fd 持有这个管道读端。
    pub fn add_read_end(&mut self) {
        self.read_open += 1;
    }
    // 复制写端 fd 时增加写端计数。
    // 执行时把 write_open 加一，表示又有一个 fd 持有这个管道写端。
    pub fn add_write_end(&mut self) {
        self.write_open += 1;
    }
    // 关闭一个读端，并返回是否已经没有读端。
    // 执行时在计数非零时递减 read_open，并返回递减后是否已经归零。
    pub fn close_read_end(&mut self) -> bool {
        if self.read_open > 0 {
            self.read_open -= 1;
        }
        self.read_open == 0
    }
    // 关闭一个写端，并返回是否已经没有写端。
    // 执行时在计数非零时递减 write_open，并返回递减后是否已经归零。
    pub fn close_write_end(&mut self) -> bool {
        if self.write_open > 0 {
            self.write_open -= 1;
        }
        self.write_open == 0
    }
}

// 创建一根匿名管道，返回读端和写端。
// 执行时先创建共享 PipeRingBuffer，再分别构造读端和写端，最后把两个 Arc 一起返回给 pipe2。
pub fn make_pipe() -> (Arc<Pipe>, Arc<Pipe>) {
    let buffer =
        Arc::new(unsafe { UPSafeCell::new_named(PipeRingBuffer::new(), "PipeRingBuffer") });
    let read_end = Arc::new(Pipe::read_end_with_buffer(buffer.clone()));
    let write_end = Arc::new(Pipe::write_end_with_buffer(buffer.clone()));
    (read_end, write_end)
}

impl File for Pipe {
    // 管道端点是否允许读。
    // 执行时直接返回端点创建时保存的 readable 标志，不访问缓冲区。
    fn readable(&self) -> bool {
        self.readable
    }
    // 管道端点是否允许写。
    // 执行时直接返回端点创建时保存的 writable 标志，不访问缓冲区。
    fn writable(&self) -> bool {
        self.writable
    }
    // 管道读：没数据时阻塞，有部分数据则先返回部分结果。
    // 执行时循环检查 killed、可读字节数和写端关闭状态；无数据就阻塞，有数据就读出并唤醒写端。
    fn read(&self, buf: &mut [u8]) -> isize {
        assert!(self.readable());
        let want_to_read = buf.len();
        if want_to_read == 0 {
            return 0;
        }
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
                if already_read > 0 {
                    return already_read as isize;
                }
                if write_end_closed {
                    return already_read as isize;
                }
                stdout_release_if_owner();
                block_current_and_run_next(BlockReason::PipeRead(self.wait_key()));
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
    // 管道写：缓冲区满时阻塞，读端全关时返回 EPIPE。
    // 执行时循环检查 killed、读端关闭和可写空间；没空间就阻塞，写入后唤醒读端和 poll 等待者。
    fn write(&self, buf: &mut [u8]) -> isize {
        assert!(self.writable());
        let want_to_write = buf.len();
        if want_to_write == 0 {
            return 0;
        }
        let mut already_write = 0usize;
        loop {
            if current_task_killed() {
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
                return -32;
            }
            if loop_write == 0 {
                stdout_release_if_owner();
                block_current_and_run_next(BlockReason::PipeWrite(self.wait_key()));
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
    // 给 poll/select 判断当前读写端是否马上可用。
    // 执行时读取环形缓冲区状态；读端看数据或 EOF，写端看空间或读端关闭。
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
    // 管道在 stat 里表现为 FIFO。
    // 执行时直接构造 S_IFIFO 类型的 Kstat，不需要访问底层文件系统。
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFIFO)
    }
    // fd 复制时同步增加读端或写端引用数。
    // 执行时独占管道缓冲区，根据当前端点类型递增 read_open 或 write_open。
    fn dup_ref(&self) {
        let mut ring_buffer = self.buffer.exclusive_access();
        if self.readable {
            ring_buffer.add_read_end();
        }
        if self.writable {
            ring_buffer.add_write_end();
        }
    }
    // fd 关闭时减少端点引用，并唤醒可能等 EOF/EPIPE 的任务。
    // 执行时递减当前端点计数；如果最后一个读端或写端关闭，就唤醒另一端等待队列。
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
    // 给 fd 层做具体类型判断。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
