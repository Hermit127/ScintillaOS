use core::sync::atomic::{AtomicBool, Ordering};
use smoltcp::iface::SocketHandle;
use smoltcp::wire::IpEndpoint;

use crate::fs::{File, Kstat};
use crate::sync::UPSafeCell;

pub const S_IFSOCK: u32 = 0o140000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketType {
    Tcp,
    Udp,
}

pub struct SocketFile {
    pub handle: UPSafeCell<SocketHandle>,
    pub socket_type: SocketType,
    pub nonblock: AtomicBool,
    ref_count: UPSafeCell<usize>,
    pub peer_endpoint: UPSafeCell<Option<IpEndpoint>>,
    pub local_endpoint: UPSafeCell<Option<IpEndpoint>>,
    pub local_tcp: UPSafeCell<Option<(usize, bool)>>,
    pub was_listening: AtomicBool,
}

impl SocketFile {
    pub fn new(handle: SocketHandle, socket_type: SocketType, nonblock: bool) -> Self {
        Self {
            handle: unsafe { UPSafeCell::new(handle) },
            socket_type,
            nonblock: AtomicBool::new(nonblock),
            ref_count: unsafe { UPSafeCell::new(1) },
            peer_endpoint: unsafe { UPSafeCell::new(None) },
            local_endpoint: unsafe { UPSafeCell::new(None) },
            local_tcp: unsafe { UPSafeCell::new(None) },
            was_listening: AtomicBool::new(false),
        }
    }

    pub fn get_handle(&self) -> SocketHandle {
        *self.handle.exclusive_access()
    }

    pub fn set_handle(&self, handle: SocketHandle) {
        *self.handle.exclusive_access() = handle;
    }

    pub fn get_nonblock(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    pub fn get_peer_endpoint(&self) -> Option<IpEndpoint> {
        *self.peer_endpoint.exclusive_access()
    }

    pub fn set_peer_endpoint(&self, ep: IpEndpoint) {
        *self.peer_endpoint.exclusive_access() = Some(ep);
    }

    pub fn get_local_endpoint(&self) -> Option<IpEndpoint> {
        *self.local_endpoint.exclusive_access()
    }

    pub fn set_local_endpoint(&self, ep: IpEndpoint) {
        *self.local_endpoint.exclusive_access() = Some(ep);
    }

    pub fn get_local_tcp(&self) -> Option<(usize, bool)> {
        *self.local_tcp.exclusive_access()
    }

    pub fn set_local_tcp(&self, conn_id: usize, server_side: bool) {
        *self.local_tcp.exclusive_access() = Some((conn_id, server_side));
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
        let nonblock = self.get_nonblock();
        let handle = self.get_handle();
        match self.socket_type {
            SocketType::Tcp => {
                if let Some((conn_id, server_side)) = self.get_local_tcp() {
                    super::syscall::local_tcp_recv(conn_id, server_side, nonblock, buf)
                } else {
                    super::syscall::tcp_recv(handle, nonblock, buf)
                }
            }
            SocketType::Udp => super::syscall::udp_recv_with_endpoints(
                handle,
                nonblock,
                buf,
                self.get_local_endpoint(),
                self.get_peer_endpoint(),
            ),
        }
    }

    fn write(&self, buf: &mut [u8]) -> isize {
        let nonblock = self.get_nonblock();
        let handle = self.get_handle();
        let data: &[u8] = buf;
        let result = match self.socket_type {
            SocketType::Tcp => {
                if let Some((conn_id, server_side)) = self.get_local_tcp() {
                    super::syscall::local_tcp_send(conn_id, server_side, nonblock, data)
                } else {
                    super::syscall::tcp_send(handle, nonblock, data)
                }
            }
            SocketType::Udp => super::syscall::udp_write_with_endpoint(
                handle,
                data,
                self.get_local_endpoint(),
                self.get_peer_endpoint(),
            ),
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
        *self.ref_count.exclusive_access() += 1;
    }

    fn close_ref(&self) {
        let should_close = {
            let mut ref_count = self.ref_count.exclusive_access();
            if *ref_count > 0 {
                *ref_count -= 1;
            }
            *ref_count == 0
        };
        if !should_close {
            return;
        }

        if let Some((conn_id, server_side)) = self.get_local_tcp() {
            super::syscall::local_tcp_close(conn_id, server_side);
        } else if self.socket_type == SocketType::Udp {
            if let Some(local) = self.get_local_endpoint() {
                super::syscall::local_udp_close(local, self.get_peer_endpoint());
            }
        } else if self.socket_type == SocketType::Tcp && self.was_listening.load(Ordering::Relaxed)
        {
            if let Some(local) = self.get_local_endpoint() {
                super::syscall::local_tcp_unlisten(local.port);
            }
        }
    }

    fn poll(&self) -> (bool, bool, bool) {
        if self.socket_type == SocketType::Tcp {
            if let Some((conn_id, server_side)) = self.get_local_tcp() {
                return super::syscall::local_tcp_poll(conn_id, server_side);
            }
            if let Some(local) = self.get_local_endpoint() {
                if super::syscall::local_tcp_pending_on(local.port) {
                    return (true, true, false);
                }
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
            let handle = self.get_handle();
            crate::net::with_net_iface(|iface| match self.socket_type {
                SocketType::Tcp => {
                    let sock: &TcpSocket = iface.sockets.get(handle);
                    let has_pending = sock.is_active()
                        && !sock.is_listening()
                        && self.was_listening.load(Ordering::Relaxed);
                    if self.was_listening.load(Ordering::Relaxed) {
                        (has_pending, true, false)
                    } else {
                        (
                            sock.can_recv() || !sock.may_recv(),
                            sock.can_send() || !sock.is_active(),
                            !sock.is_active() && !sock.is_listening() && sock.recv_queue() == 0,
                        )
                    }
                }
                SocketType::Udp => {
                    let sock: &UdpSocket = iface.sockets.get(handle);
                    let local_pending = self
                        .get_local_endpoint()
                        .map(|ep| super::syscall::local_udp_pending_on(ep.port))
                        .unwrap_or(false);
                    (sock.can_recv() || local_pending, true, false)
                }
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

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
