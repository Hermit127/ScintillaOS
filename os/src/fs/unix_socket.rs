use super::{File, Kstat, S_IFSOCK};
use crate::fs::stdio::stdout_release_if_owner;
use crate::sync::DeviceLock;
use crate::task::{
    block_current_and_run_next_if, current_add_signal, current_task, has_interrupting_signal,
    wake_pipe_read_tasks, wake_pipe_write_tasks, wake_poll_wait_tasks, BlockReason, SignalFlags,
};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

const O_NONBLOCK: usize = 0x800;
const UNIX_SOCKET_QUEUE_PACKETS: usize = 64;
const UNIX_SOCKET_QUEUE_BYTES: usize = 256 * 1024;
const UNIX_SOCKET_PACKET_BYTES: usize = 64 * 1024;
const MSG_PEEK: usize = 0x02;
const MSG_DONTWAIT: usize = 0x40;
const MSG_NOSIGNAL: usize = 0x4000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UnixSocketKind {
    Stream,
    SeqPacket,
}

struct UnixSocketRecord {
    data: Vec<u8>,
    offset: usize,
}

struct UnixSocketState {
    queues: [VecDeque<UnixSocketRecord>; 2],
    queued_bytes: [usize; 2],
    endpoint_refs: [usize; 2],
}

pub struct UnixSocketFile {
    state: Arc<DeviceLock<UnixSocketState>>,
    endpoint: usize,
    kind: UnixSocketKind,
    nonblock: AtomicBool,
}

fn current_task_interrupted() -> bool {
    current_task()
        .and_then(|task| {
            task.try_inner_borrow()
                .map(|inner| has_interrupting_signal(&inner))
        })
        .unwrap_or(false)
}

pub fn make_unix_socket_pair(
    kind: UnixSocketKind,
    nonblock: bool,
) -> (Arc<UnixSocketFile>, Arc<UnixSocketFile>) {
    let state = Arc::new(DeviceLock::new_named(
        UnixSocketState {
            queues: [
                VecDeque::with_capacity(UNIX_SOCKET_QUEUE_PACKETS),
                VecDeque::with_capacity(UNIX_SOCKET_QUEUE_PACKETS),
            ],
            queued_bytes: [0; 2],
            endpoint_refs: [1; 2],
        },
        "UnixSocket.pair",
    ));
    let first = Arc::new(UnixSocketFile {
        state: state.clone(),
        endpoint: 0,
        kind,
        nonblock: AtomicBool::new(nonblock),
    });
    let second = Arc::new(UnixSocketFile {
        state,
        endpoint: 1,
        kind,
        nonblock: AtomicBool::new(nonblock),
    });
    (first, second)
}

impl UnixSocketFile {
    pub fn socket_type_value(&self) -> i32 {
        match self.kind {
            UnixSocketKind::Stream => 1,
            UnixSocketKind::SeqPacket => 5,
        }
    }

    fn peer(&self) -> usize {
        self.endpoint ^ 1
    }

    fn queue_key(&self, queue: usize) -> usize {
        (Arc::as_ptr(&self.state) as usize) | queue
    }

    fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    fn copy_packet(data: &[u8]) -> Result<Vec<u8>, isize> {
        let mut packet = Vec::new();
        packet.try_reserve_exact(data.len()).map_err(|_| -12isize)?;
        packet.extend_from_slice(data);
        Ok(packet)
    }

    fn enqueue_record(&self, packet: Vec<u8>, dontwait: bool, suppress_sigpipe: bool) -> isize {
        let packet_len = packet.len();
        let destination = self.peer();
        let mut packet = Some(packet);
        loop {
            if current_task_interrupted() {
                return -4;
            }
            let result = {
                let mut state = self.state.exclusive_access();
                if state.endpoint_refs[destination] == 0 {
                    Err(-32)
                } else if state.queues[destination].len() >= UNIX_SOCKET_QUEUE_PACKETS
                    || state.queued_bytes[destination]
                        .saturating_add(packet_len)
                        > UNIX_SOCKET_QUEUE_BYTES
                {
                    Ok(false)
                } else {
                    state.queues[destination].push_back(UnixSocketRecord {
                        data: packet.take().unwrap(),
                        offset: 0,
                    });
                    state.queued_bytes[destination] += packet_len;
                    Ok(true)
                }
            };
            match result {
                Err(-32) => {
                    if !suppress_sigpipe {
                        current_add_signal(SignalFlags::SIGPIPE);
                    }
                    return -32;
                }
                Err(errno) => return errno,
                Ok(true) => {
                    wake_pipe_read_tasks(self.queue_key(destination));
                    wake_poll_wait_tasks();
                    return packet_len as isize;
                }
                Ok(false) if dontwait => return -11,
                Ok(false) => {
                    stdout_release_if_owner();
                    let key = self.queue_key(destination);
                    block_current_and_run_next_if(BlockReason::PipeWrite(key), || {
                        let state = self.state.exclusive_access();
                        state.endpoint_refs[destination] != 0
                            && (state.queues[destination].len() >= UNIX_SOCKET_QUEUE_PACKETS
                                || state.queued_bytes[destination]
                                    .saturating_add(packet_len)
                                    > UNIX_SOCKET_QUEUE_BYTES)
                    });
                }
            }
        }
    }

    fn read_seqpacket(&self, buf: &mut [u8], dontwait: bool, peek: bool) -> isize {
        enum ReadState {
            Record(usize, Option<UnixSocketRecord>),
            Closed,
            Empty,
        }

        loop {
            if current_task_interrupted() {
                return -4;
            }
            let result = {
                let mut state = self.state.exclusive_access();
                if let Some(record) = state.queues[self.endpoint].front() {
                    let count = buf.len().min(record.data.len());
                    buf[..count].copy_from_slice(&record.data[..count]);
                    if peek {
                        ReadState::Record(count, None)
                    } else {
                        let record = state.queues[self.endpoint].pop_front().unwrap();
                        state.queued_bytes[self.endpoint] = state.queued_bytes[self.endpoint]
                            .saturating_sub(record.data.len());
                        ReadState::Record(count, Some(record))
                    }
                } else if state.endpoint_refs[self.peer()] == 0 {
                    ReadState::Closed
                } else {
                    ReadState::Empty
                }
            };
            match result {
                ReadState::Record(count, retired) => {
                    let consumed = retired.is_some();
                    drop(retired);
                    if consumed {
                        wake_pipe_write_tasks(self.queue_key(self.endpoint));
                        wake_poll_wait_tasks();
                    }
                    return count as isize;
                }
                ReadState::Closed => return 0,
                ReadState::Empty if dontwait => return -11,
                ReadState::Empty => {
                    stdout_release_if_owner();
                    let key = self.queue_key(self.endpoint);
                    block_current_and_run_next_if(BlockReason::PipeRead(key), || {
                        let state = self.state.exclusive_access();
                        state.queues[self.endpoint].is_empty()
                            && state.endpoint_refs[self.peer()] != 0
                    });
                }
            }
        }
    }

    fn read_stream(&self, buf: &mut [u8], dontwait: bool, peek: bool) -> isize {
        enum ReadState {
            Data(usize, Option<UnixSocketRecord>),
            Closed,
            Empty,
        }

        let read_limit = buf.len().min(UNIX_SOCKET_PACKET_BYTES);

        loop {
            if current_task_interrupted() {
                return -4;
            }
            let result = {
                let mut state = self.state.exclusive_access();
                if state.queues[self.endpoint].front().is_some() {
                    let (count, exhausted) = {
                        let record = state.queues[self.endpoint].front_mut().unwrap();
                        let count = read_limit.min(record.data.len() - record.offset);
                        buf[..count]
                            .copy_from_slice(&record.data[record.offset..record.offset + count]);
                        if !peek {
                            record.offset += count;
                        }
                        (count, record.offset == record.data.len())
                    };
                    if !peek {
                        state.queued_bytes[self.endpoint] = state.queued_bytes[self.endpoint]
                            .saturating_sub(count);
                    }
                    let retired = if !peek && exhausted {
                        state.queues[self.endpoint].pop_front()
                    } else {
                        None
                    };
                    ReadState::Data(count, retired)
                } else if state.endpoint_refs[self.peer()] == 0 {
                    ReadState::Closed
                } else {
                    ReadState::Empty
                }
            };
            match result {
                ReadState::Data(count, retired) => {
                    let consumed = !peek;
                    drop(retired);
                    if consumed {
                        wake_pipe_write_tasks(self.queue_key(self.endpoint));
                        wake_poll_wait_tasks();
                    }
                    return count as isize;
                }
                ReadState::Closed => return 0,
                ReadState::Empty if dontwait => return -11,
                ReadState::Empty => {
                    stdout_release_if_owner();
                    let key = self.queue_key(self.endpoint);
                    block_current_and_run_next_if(BlockReason::PipeRead(key), || {
                        let state = self.state.exclusive_access();
                        state.queues[self.endpoint].is_empty()
                            && state.endpoint_refs[self.peer()] != 0
                    });
                }
            }
        }
    }

    pub fn recv_with_flags(&self, buf: &mut [u8], flags: usize) -> isize {
        if buf.is_empty() {
            return 0;
        }
        let dontwait = self.nonblocking() || flags & MSG_DONTWAIT != 0;
        let peek = flags & MSG_PEEK != 0;
        match self.kind {
            UnixSocketKind::Stream => self.read_stream(buf, dontwait, peek),
            UnixSocketKind::SeqPacket => self.read_seqpacket(buf, dontwait, peek),
        }
    }

    pub(crate) fn send_staging_len(&self, len: usize) -> Result<usize, isize> {
        match self.kind {
            UnixSocketKind::Stream => Ok(len.min(UNIX_SOCKET_PACKET_BYTES)),
            UnixSocketKind::SeqPacket if len > UNIX_SOCKET_PACKET_BYTES => Err(-90),
            UnixSocketKind::SeqPacket => Ok(len),
        }
    }

    pub fn send_with_flags(&self, buf: &[u8], flags: usize) -> isize {
        if buf.is_empty() {
            return 0;
        }
        let write_len = match self.send_staging_len(buf.len()) {
            Ok(len) => len,
            Err(errno) => return errno,
        };
        let packet = match Self::copy_packet(&buf[..write_len]) {
            Ok(packet) => packet,
            Err(errno) => return errno,
        };
        let dontwait = self.nonblocking() || flags & MSG_DONTWAIT != 0;
        self.enqueue_record(packet, dontwait, flags & MSG_NOSIGNAL != 0)
    }
}

impl File for UnixSocketFile {
    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, buf: &mut [u8]) -> isize {
        self.recv_with_flags(buf, 0)
    }

    fn write(&self, buf: &mut [u8]) -> isize {
        self.send_with_flags(buf, 0)
    }

    fn stat(&self) -> Kstat {
        Kstat::new(S_IFSOCK)
    }

    fn get_flags(&self) -> usize {
        let mut flags = 0x8000 | 2;
        if self.nonblocking() {
            flags |= O_NONBLOCK;
        }
        flags
    }

    fn set_flags(&self, flags: usize) -> bool {
        self.nonblock
            .store((flags & O_NONBLOCK) != 0, Ordering::Relaxed);
        true
    }

    fn poll(&self) -> (bool, bool, bool) {
        let state = self.state.exclusive_access();
        let peer = self.peer();
        let peer_closed = state.endpoint_refs[peer] == 0;
        let writable = !peer_closed
            && state.queues[peer].len() < UNIX_SOCKET_QUEUE_PACKETS
            && state.queued_bytes[peer] < UNIX_SOCKET_QUEUE_BYTES;
        (
            !state.queues[self.endpoint].is_empty() || peer_closed,
            writable,
            peer_closed,
        )
    }

    fn dup_ref(&self) {
        self.state.exclusive_access().endpoint_refs[self.endpoint] += 1;
    }

    fn close_ref(&self) {
        let endpoint_closed = {
            let mut state = self.state.exclusive_access();
            if state.endpoint_refs[self.endpoint] > 0 {
                state.endpoint_refs[self.endpoint] -= 1;
            }
            state.endpoint_refs[self.endpoint] == 0
        };
        if endpoint_closed {
            wake_pipe_read_tasks(self.queue_key(self.peer()));
            wake_pipe_write_tasks(self.queue_key(self.endpoint));
            wake_poll_wait_tasks();
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
