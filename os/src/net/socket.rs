use core::sync::atomic::{AtomicBool, Ordering};
use smoltcp::iface::SocketHandle;
use smoltcp::wire::IpEndpoint;

use crate::fs::{File, Kstat};
use crate::sync::DeviceLock;

pub const S_IFSOCK: u32 = 0o140000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketType {
    Tcp,
    Udp,
    Unix,
}

struct SocketFileState {
    handle: Option<SocketHandle>,
    ref_count: usize,
    peer_endpoint: Option<IpEndpoint>,
    local_endpoint: Option<IpEndpoint>,
    local_tcp: Option<(usize, bool)>,
}

/// Mutable socket metadata is one object-level device domain.  Never retain
/// this lock while calling into smoltcp, blocking, or invoking a wakeup path.
pub struct SocketFile {
    inner: DeviceLock<SocketFileState>,
    pub socket_type: SocketType,
    pub nonblock: AtomicBool,
    pub was_listening: AtomicBool,
}

impl SocketFile {
    pub fn new(handle: SocketHandle, socket_type: SocketType, nonblock: bool) -> Self {
        Self {
            inner: DeviceLock::new_named(
                SocketFileState {
                    handle: Some(handle),
                    ref_count: 1,
                    peer_endpoint: None,
                    local_endpoint: None,
                    local_tcp: None,
                },
                "SocketFile",
            ),
            socket_type,
            nonblock: AtomicBool::new(nonblock),
            was_listening: AtomicBool::new(false),
        }
    }

    pub fn new_local_tcp(conn_id: usize, server_side: bool, nonblock: bool) -> Self {
        Self {
            inner: DeviceLock::new_named(
                SocketFileState {
                    handle: None,
                    ref_count: 1,
                    peer_endpoint: None,
                    local_endpoint: None,
                    local_tcp: Some((conn_id, server_side)),
                },
                "SocketFile",
            ),
            socket_type: SocketType::Tcp,
            nonblock: AtomicBool::new(nonblock),
            was_listening: AtomicBool::new(false),
        }
    }

    pub fn get_handle(&self) -> Option<SocketHandle> {
        self.inner.exclusive_access().handle
    }

    pub fn set_handle(&self, handle: SocketHandle) {
        self.inner.exclusive_access().handle = Some(handle);
    }

    pub fn get_nonblock(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    pub fn get_peer_endpoint(&self) -> Option<IpEndpoint> {
        self.inner.exclusive_access().peer_endpoint
    }

    pub fn set_peer_endpoint(&self, ep: IpEndpoint) {
        self.inner.exclusive_access().peer_endpoint = Some(ep);
    }

    pub fn get_local_endpoint(&self) -> Option<IpEndpoint> {
        self.inner.exclusive_access().local_endpoint
    }

    pub fn set_local_endpoint(&self, ep: IpEndpoint) {
        self.inner.exclusive_access().local_endpoint = Some(ep);
    }

    pub fn get_local_tcp(&self) -> Option<(usize, bool)> {
        self.inner.exclusive_access().local_tcp
    }

    pub fn set_local_tcp(&self, conn_id: usize, server_side: bool) {
        self.inner.exclusive_access().local_tcp = Some((conn_id, server_side));
    }
}

impl File for SocketFile {
    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, buf: &mut [u8]) -> isize {
        if let Some((conn_id, server_side)) = self.get_local_tcp() {
            return super::syscall::local_tcp_recv(
                conn_id,
                server_side,
                self.get_nonblock(),
                false,
                buf,
            );
        }
        let nonblock = self.get_nonblock();
        let Some(handle) = self.get_handle() else {
            return -107;
        };
        match self.socket_type {
            SocketType::Tcp => super::syscall::tcp_recv(handle, nonblock, buf),
            SocketType::Udp => super::syscall::udp_read_socket(self, handle, nonblock, buf),
            SocketType::Unix => -107,
        }
    }

    fn write(&self, buf: &mut [u8]) -> isize {
        if let Some((conn_id, server_side)) = self.get_local_tcp() {
            return super::syscall::local_tcp_send(conn_id, server_side, self.get_nonblock(), buf);
        }
        let nonblock = self.get_nonblock();
        let Some(handle) = self.get_handle() else {
            return -107;
        };
        let data: &[u8] = buf;
        let result = match self.socket_type {
            SocketType::Tcp => super::syscall::tcp_send(handle, nonblock, data),
            SocketType::Udp => {
                let peer = self.get_peer_endpoint();
                if let Some(dest) = peer {
                    if let Some(ret) = super::syscall::try_write_local_udp(self, handle, data, dest)
                    {
                        return ret;
                    }
                }
                super::syscall::udp_write(handle, data, peer)
            }
            SocketType::Unix => -107,
        };
        result
    }

    fn stat(&self) -> Kstat {
        Kstat::new(S_IFSOCK)
    }

    fn get_flags(&self) -> usize {
        const O_NONBLOCK: usize = 0x800;
        let mut flags = 0x8000 | 2; // O_LARGEFILE | O_RDWR
        if self.get_nonblock() {
            flags |= O_NONBLOCK;
        }
        flags
    }

    fn set_flags(&self, flags: usize) -> bool {
        const O_NONBLOCK: usize = 0x800;
        self.nonblock
            .store((flags & O_NONBLOCK) != 0, Ordering::Relaxed);
        true
    }

    fn dup_ref(&self) {
        self.inner.exclusive_access().ref_count += 1;
    }

    fn poll(&self) -> (bool, bool, bool) {
        if let Some((conn_id, server_side)) = self.get_local_tcp() {
            return super::syscall::local_tcp_poll(conn_id, server_side);
        }
        if self.was_listening.load(Ordering::Relaxed) {
            let pending = self
                .get_local_endpoint()
                .map(|local| super::syscall::local_tcp_pending_on(local.port))
                .unwrap_or(false);
            return (pending, true, false);
        }
        if self.socket_type == SocketType::Udp {
            let mut local_pending = false;
            if let Some(local) = self.get_local_endpoint() {
                if super::syscall::local_udp_pending_on(local, self.get_peer_endpoint()) {
                    local_pending = true;
                }
            }
            let local_peer = self
                .get_peer_endpoint()
                .map(super::syscall::is_local_endpoint)
                .unwrap_or(false);
            if local_peer || !crate::net::net_available_fast() {
                return (local_pending, true, false);
            }
        }
        #[cfg(any(
            target_arch = "riscv64",
            target_arch = "aarch64",
            target_arch = "loongarch64"
        ))]
        {
            use smoltcp::socket::tcp::Socket as TcpSocket;
            use smoltcp::socket::udp::Socket as UdpSocket;
            let Some(handle) = self.get_handle() else {
                return (false, false, true);
            };
            crate::net::with_net_iface(|iface| match self.socket_type {
                SocketType::Tcp => {
                    let sock: &TcpSocket = iface.sockets.get(handle);
                    let has_pending = sock.is_active()
                        && !sock.is_listening()
                        && self.was_listening.load(Ordering::Relaxed);
                    (
                        sock.can_recv() || !sock.may_recv() || has_pending,
                        sock.can_send() || !sock.is_active(),
                        !sock.is_active() && !sock.is_listening() && sock.recv_queue() == 0,
                    )
                }
                SocketType::Udp => {
                    let sock: &UdpSocket = iface.sockets.get(handle);
                    (sock.can_recv(), true, false)
                }
                SocketType::Unix => (false, true, false),
            })
            .unwrap_or((false, false, false))
        }
        #[cfg(not(any(
            target_arch = "riscv64",
            target_arch = "aarch64",
            target_arch = "loongarch64"
        )))]
        {
            (false, false, false)
        }
    }

    fn close_ref(&self) {
        let should_close = {
            let mut inner = self.inner.exclusive_access();
            if inner.ref_count > 0 {
                inner.ref_count -= 1;
            }
            inner.ref_count == 0
        };
        if !should_close {
            return;
        }

        if let Some((conn_id, server_side)) = self.get_local_tcp() {
            super::syscall::local_tcp_close(conn_id, server_side);
        }
        if self.was_listening.load(Ordering::Relaxed) {
            if let Some(local) = self.get_local_endpoint() {
                super::syscall::local_tcp_unlisten(local.port);
            }
        }
        if self.socket_type == SocketType::Udp {
            super::syscall::local_udp_close(self.get_local_endpoint(), self.get_peer_endpoint());
        }

        let Some(handle) = self.get_handle() else {
            return;
        };
        #[cfg(any(
            target_arch = "riscv64",
            target_arch = "aarch64",
            target_arch = "loongarch64"
        ))]
        if self.socket_type != SocketType::Unix {
            crate::net::with_net_iface(|iface| {
                iface.sockets.remove(handle);
            });
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
