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
    pub peer_endpoint: UPSafeCell<Option<IpEndpoint>>,
    pub local_endpoint: UPSafeCell<Option<IpEndpoint>>,
    pub was_listening: AtomicBool,
}

impl SocketFile {
    pub fn new(handle: SocketHandle, socket_type: SocketType, nonblock: bool) -> Self {
        Self {
            handle: unsafe { UPSafeCell::new(handle) },
            socket_type,
            nonblock: AtomicBool::new(nonblock),
            peer_endpoint: unsafe { UPSafeCell::new(None) },
            local_endpoint: unsafe { UPSafeCell::new(None) },
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
            SocketType::Tcp => super::syscall::tcp_recv(handle, nonblock, buf),
            SocketType::Udp => super::syscall::udp_recv(handle, nonblock, buf),
        }
    }

    fn write(&self, buf: &mut [u8]) -> isize {
        let nonblock = self.get_nonblock();
        let handle = self.get_handle();
        let data: &[u8] = buf;
        let result = match self.socket_type {
            SocketType::Tcp => super::syscall::tcp_send(handle, nonblock, data),
            SocketType::Udp => super::syscall::udp_write(handle, data, self.get_peer_endpoint()),
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

    fn poll(&self) -> (bool, bool, bool) {
        #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
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
            })
            .unwrap_or((false, false, false))
        }
        #[cfg(not(any(target_arch = "riscv64", target_arch = "aarch64")))]
        {
            (false, false, false)
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
