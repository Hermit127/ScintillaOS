use alloc::collections::BTreeMap;
use smoltcp::iface::SocketHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketType {
    Tcp,
    Udp,
}

pub struct SocketEntry {
    pub handle: SocketHandle,
    pub socket_type: SocketType,
    pub nonblock: bool,
    pub cloexec: bool,
}

pub struct SocketTable {
    map: BTreeMap<usize, SocketEntry>,
    next_fd: usize,
}

impl SocketTable {
    pub const fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            next_fd: 0,
        }
    }

    pub fn alloc_fd(&mut self) -> usize {
        let fd = self.next_fd;
        self.next_fd += 1;
        fd
    }

    pub fn insert(&mut self, fd: usize, entry: SocketEntry) {
        self.map.insert(fd, entry);
    }

    pub fn remove(&mut self, fd: usize) -> Option<SocketEntry> {
        self.map.remove(&fd)
    }

    pub fn get(&self, fd: usize) -> Option<&SocketEntry> {
        self.map.get(&fd)
    }

    pub fn get_mut(&mut self, fd: usize) -> Option<&mut SocketEntry> {
        self.map.get_mut(&fd)
    }
}
