#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
mod imp {
    use core::sync::atomic::{AtomicBool, Ordering};
    use smoltcp::iface::{Config, Interface, SocketSet};
    use smoltcp::socket::dhcpv4::Socket as DhcpSocket;
    use smoltcp::time::Instant;
    use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Cidr, Ipv4Address};

    use crate::drivers::net::VirtIONetAdapter;
    use crate::sync::UPSafeCell;
    use lazy_static::lazy_static;

    const DEFAULT_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    pub static NET_INITED: AtomicBool = AtomicBool::new(false);

    pub struct NetworkState {
        pub device: VirtIONetAdapter,
        pub iface: Interface,
        pub sockets: SocketSet<'static>,
        pub dhcp_handle: Option<smoltcp::iface::SocketHandle>,
        pub clock_ms: u64,
        pub inited: bool,
        pub dhcp_logged: bool,
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
            let mut iface = Interface::new(config, &mut device, now);

            // Register loopback as a local IP so 127.0.0.1 traffic stays within the stack
            iface.update_ip_addrs(|addrs| {
                let loopback = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8));
                let _ = addrs.push(loopback);
                // Static IP as fallback in case DHCP never completes
                let static_ip = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24));
                let _ = addrs.push(static_ip);
            });

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
            dhcp_logged: false,
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
    pub(crate) fn tcp_send(_handle: smoltcp::iface::SocketHandle, _nonblock: bool, _data: &[u8]) -> isize { -38 }
    pub(crate) fn tcp_recv(_handle: smoltcp::iface::SocketHandle, _nonblock: bool, _buf: &mut [u8]) -> isize { -38 }
    pub(crate) fn udp_send(_handle: smoltcp::iface::SocketHandle, _data: &[u8], _dest: smoltcp::wire::IpEndpoint) -> isize { -38 }
    pub(crate) fn udp_write(_handle: smoltcp::iface::SocketHandle, _data: &[u8]) -> isize { -38 }
    pub(crate) fn udp_recv(_handle: smoltcp::iface::SocketHandle, _nonblock: bool, _buf: &mut [u8]) -> isize { -38 }
}

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
use imp::NET_IFACE;
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
use imp::NetworkState;
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
use imp::NET_INITED;

pub fn net_init() {
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    {
        let mut state = NET_IFACE.exclusive_access();
        if let Some(state) = state.as_mut() {
            state.inited = true;
            NET_INITED.store(true, core::sync::atomic::Ordering::Relaxed);
            println!("[net] VirtIO-Net initialized, MAC={:02x?}", state.device.mac_address());
            println!("[net] Static IPs: loopback 127.0.0.0/8 + 10.0.2.15/24");
        } else {
            println!("[net] No virtio-net device available, networking disabled");
        }
    }
}

pub fn net_poll() {
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    {
        if !NET_INITED.load(core::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let mut state = NET_IFACE.exclusive_access();
        if let Some(state) = state.as_mut() {
            if !state.inited {
                return;
            }
            state.clock_ms += 10;
            let clock = state.clock_ms;
            let timestamp = smoltcp::time::Instant::from_millis(clock as i64);
            let NetworkState { ref mut iface, ref mut device, ref mut sockets, ref mut dhcp_handle, ref mut dhcp_logged, .. } = state;
            iface.poll(timestamp, device, sockets);

            if !*dhcp_logged && iface.ip_addrs().len() > 2 {
                *dhcp_logged = true;
                println!("[net] DHCP assigned additional IP, total IPs:");
                for addr in iface.ip_addrs() {
                    println!("[net]   {}", addr);
                }
            }
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

    let inited = NET_INITED.load(core::sync::atomic::Ordering::Relaxed);
    if !inited {
        return;
    }

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
