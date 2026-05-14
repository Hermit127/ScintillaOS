#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
mod imp {
    use smoltcp::iface::{Config, Interface, SocketSet};
    use smoltcp::socket::dhcpv4::Socket as DhcpSocket;
    use smoltcp::time::Instant;
    use smoltcp::wire::{EthernetAddress, HardwareAddress};

    use crate::drivers::net::VirtIONetAdapter;
    use crate::sync::UPSafeCell;
    use lazy_static::lazy_static;

    const DEFAULT_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    pub struct NetworkState {
        pub device: VirtIONetAdapter,
        pub iface: Interface,
        pub sockets: SocketSet<'static>,
        pub dhcp_handle: Option<smoltcp::iface::SocketHandle>,
        pub clock_ms: u64,
        pub inited: bool,
    }

    impl NetworkState {
    pub fn new() -> Option<Self> {
        let mut device = VirtIONetAdapter::new()?;
        let mac_addr = device.mac_address();
            let mac = if mac_addr == [0u8; 6] {
                DEFAULT_MAC
            } else {
                mac_addr
            };

            let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
            let now = Instant::from_millis(0);
            let iface = Interface::new(config, &mut device, now);

            let mut sockets = SocketSet::new(alloc::vec::Vec::new());
            let dhcp_socket = DhcpSocket::new();
            let dhcp_handle = sockets.add(dhcp_socket);

        Some(Self {
            device,
            iface,
            sockets,
            dhcp_handle: Some(dhcp_handle),
            clock_ms: 0,
            inited: false,
        })
    }

        pub fn poll(&mut self, ms_since_boot: u64) {
            self.clock_ms = ms_since_boot;
            let timestamp = Instant::from_millis(self.clock_ms as i64);
            self.iface.poll(timestamp, &mut self.device, &mut self.sockets);
        }
    }

    lazy_static! {
        pub static ref NET_IFACE: UPSafeCell<Option<NetworkState>> = unsafe {
            UPSafeCell::new(NetworkState::new())
        };
    }
}

pub mod socket;

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
pub mod syscall;

#[cfg(not(any(target_arch = "riscv64", target_arch = "aarch64")))]
pub mod syscall {
    pub fn sys_socket(_domain: usize, _type_: usize, _protocol: usize) -> isize { -38 }
    pub fn sys_bind(_sockfd: usize, _addr: *const u8, _addrlen: usize) -> isize { -38 }
    pub fn sys_listen(_sockfd: usize, _backlog: usize) -> isize { -38 }
    pub fn sys_accept(_sockfd: usize, _addr: *mut u8, _addrlen: *mut usize) -> isize { -38 }
    pub fn sys_connect(_sockfd: usize, _addr: *const u8, _addrlen: usize) -> isize { -38 }
    pub fn sys_getsockname(_sockfd: usize, _addr: *mut u8, _addrlen: *mut usize) -> isize { -38 }
    pub fn sys_getpeername(_sockfd: usize, _addr: *mut u8, _addrlen: *mut usize) -> isize { -38 }
    pub fn sys_sendto(_sockfd: usize, _buf: *const u8, _len: usize, _flags: usize, _dest_addr: *const u8, _addrlen: usize) -> isize { -38 }
    pub fn sys_recvfrom(_sockfd: usize, _buf: *mut u8, _len: usize, _flags: usize, _src_addr: *mut u8, _addrlen: *mut usize) -> isize { -38 }
    pub fn sys_setsockopt(_sockfd: usize, _level: usize, _optname: usize, _optval: *const u8, _optlen: usize) -> isize { -38 }
    pub fn sys_getsockopt(_sockfd: usize, _level: usize, _optname: usize, _optval: *mut u8, _optlen: *mut usize) -> isize { -38 }
    pub fn sys_sock_shutdown(_sockfd: usize, _how: usize) -> isize { -38 }
    pub fn sys_sendmsg(_sockfd: usize, _msg: *const u8, _flags: usize) -> isize { -38 }
    pub fn sys_recvmsg(_sockfd: usize, _msg: *mut u8, _flags: usize) -> isize { -38 }
    pub fn socket_read(_sockfd: usize, _buf: *mut u8, _len: usize) -> isize { -38 }
    pub fn socket_write(_sockfd: usize, _buf: *const u8, _len: usize) -> isize { -38 }
}

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
use imp::NET_IFACE;
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
use imp::NetworkState;

pub fn net_init() {
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    {
        let mut state = NET_IFACE.exclusive_access();
        if let Some(state) = state.as_mut() {
            state.inited = true;
            log::info!("net: VirtIO-Net initialized, MAC={:02x?}", state.device.mac_address());
        } else {
            log::warn!("net: No virtio-net device available, networking disabled");
        }
    }
}

pub fn net_poll() {
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    {
        let mut state = NET_IFACE.exclusive_access();
        if let Some(state) = state.as_mut() {
            if !state.inited {
                return;
            }
            state.clock_ms += 10;
            let clock = state.clock_ms;
            let timestamp = smoltcp::time::Instant::from_millis(clock as i64);
            let NetworkState { ref mut iface, ref mut device, ref mut sockets, .. } = state;
            iface.poll(timestamp, device, sockets);
        }
    }
}

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
pub fn with_net_iface<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut NetworkState) -> R,
{
    let mut guard = NET_IFACE.exclusive_access();
    guard.as_mut().map(|state| f(state))
}

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
pub fn with_net_iface_tcp_connect(
    handle: smoltcp::iface::SocketHandle,
    remote: smoltcp::wire::IpEndpoint,
    local: smoltcp::wire::IpListenEndpoint,
) {
    let mut guard = NET_IFACE.exclusive_access();
    if let Some(state) = guard.as_mut() {
        let sock: &mut smoltcp::socket::tcp::Socket = state.sockets.get_mut(handle);
        // Interface has `inner` as its only field (sixlowpan disabled).
        // Transmute &mut Interface → &mut Context (= InterfaceInner).
        let cx: &mut smoltcp::iface::Context = unsafe {
            core::mem::transmute(&mut state.iface)
        };
        let _ = sock.connect(cx, remote, local);
    }
}

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
pub fn wake_net_blocked_tasks() {
    use smoltcp::iface::SocketHandle;
    use smoltcp::socket::tcp::Socket as TcpSocket;
    use smoltcp::socket::udp::Socket as UdpSocket;
    use crate::task::{add_task, BlockReason, TaskStatus, NET_BLOCK_QUEUE};

    let mut to_wake = alloc::vec::Vec::new();
    {
        let mut queue = NET_BLOCK_QUEUE.exclusive_access();
        let mut i = 0;
        while i < queue.len() {
            let should_wake = {
                let inner = queue[i].inner_exclusive_access();
                match &inner.block_reason {
                    Some(BlockReason::TcpRecv(h)) => {
                        let handle: SocketHandle = unsafe { core::mem::transmute(*h) };
                        with_net_iface(|iface| {
                            let sock: &TcpSocket = iface.sockets.get(handle);
                            sock.can_recv() || !sock.may_recv()
                        }).unwrap_or(true)
                    }
                    Some(BlockReason::TcpSend(h)) => {
                        let handle: SocketHandle = unsafe { core::mem::transmute(*h) };
                        with_net_iface(|iface| {
                            let sock: &TcpSocket = iface.sockets.get(handle);
                            sock.can_send()
                        }).unwrap_or(true)
                    }
                    Some(BlockReason::TcpAccept(h)) => {
                        let handle: SocketHandle = unsafe { core::mem::transmute(*h) };
                        with_net_iface(|iface| {
                            let sock: &TcpSocket = iface.sockets.get(handle);
                            sock.is_active() && !sock.is_listening()
                        }).unwrap_or(true)
                    }
                    Some(BlockReason::TcpConnect(h)) => {
                        let handle: SocketHandle = unsafe { core::mem::transmute(*h) };
                        with_net_iface(|iface| {
                            let sock: &TcpSocket = iface.sockets.get(handle);
                            sock.is_active()
                        }).unwrap_or(true)
                    }
                    Some(BlockReason::UdpRecv(h)) => {
                        let handle: SocketHandle = unsafe { core::mem::transmute(*h) };
                        with_net_iface(|iface| {
                            let sock: &UdpSocket = iface.sockets.get(handle);
                            sock.can_recv()
                        }).unwrap_or(true)
                    }
                    None => true, // block_reason cleared, should be woken
                }
            };
            if should_wake {
                to_wake.push(queue.remove(i));
            } else {
                i += 1;
            }
        }
    }
    for task in to_wake {
        let mut inner = task.inner_exclusive_access();
        inner.task_status = TaskStatus::Ready;
        inner.block_reason = None;
        drop(inner);
        add_task(task);
    }
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "aarch64")))]
pub fn wake_net_blocked_tasks() {}
