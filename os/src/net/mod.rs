#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
mod imp {
    use core::sync::atomic::AtomicBool;
    use smoltcp::iface::{Config, Interface, SocketSet};
    use smoltcp::socket::dhcpv4::Socket as DhcpSocket;
    use smoltcp::time::Instant;
    use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Address, Ipv4Cidr};

    use crate::drivers::net::VirtIONetAdapter;
    use crate::sync::DeviceLock;
    use lazy_static::lazy_static;

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
            let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac_addr)));
            let now = Instant::from_millis(0);
            let mut iface = Interface::new(config, &mut device, now);

            // Register loopback as a local IP so 127.0.0.1 traffic stays within the stack
            iface.update_ip_addrs(|addrs| {
                // smoltcp selects the first IPv4 address as the source for an
                // unspecified TCP bind, so the externally routable address
                // must precede loopback.
                let static_ip = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24));
                let _ = addrs.push(static_ip);
                let loopback = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8));
                let _ = addrs.push(loopback);
            });
            let _ = iface
                .routes_mut()
                .add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2));

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
            self.iface
                .poll(timestamp, &mut self.device, &mut self.sockets);
        }
    }

    lazy_static! {
        // smoltcp's interface, socket set and the underlying VirtIO adapter
        // form one serialized device transaction domain.  A poll or socket
        // operation must never observe a partially-mutated interface.
        pub static ref NET_IFACE: DeviceLock<Option<NetworkState>> =
            DeviceLock::new_named(NetworkState::new(), "NetworkInterface");
    }
}

pub mod socket;

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
pub mod syscall;

#[cfg(not(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
)))]
pub mod syscall {
    use alloc::sync::Arc;

    use crate::syscall::install_fd_shared;
    use crate::task::current_task;

    const AF_INET: usize = 2;
    const SOCK_STREAM: usize = 1;
    const SOCK_DGRAM: usize = 2;
    const SOCK_CLOEXEC: usize = 0x80000;

    pub fn sys_socket(domain: usize, type_: usize, _protocol: usize) -> isize {
        if domain != AF_INET {
            return -97;
        }
        match type_ & 0x0F {
            SOCK_STREAM | SOCK_DGRAM => {}
            _ => return -22,
        }

        let task = current_task().unwrap();
        install_fd_shared(&task, Arc::new(crate::fs::NullSocket), (type_ & SOCK_CLOEXEC) != 0)
            .map(|fd| fd as isize)
            .unwrap_or_else(|errno| errno)
    }
    pub fn sys_bind(_sockfd: usize, _addr: *const u8, _addrlen: usize) -> isize {
        -38
    }
    pub fn sys_listen(_sockfd: usize, _backlog: usize) -> isize {
        -38
    }
    pub fn sys_accept(_sockfd: usize, _addr: *mut u8, _addrlen: *mut usize) -> isize {
        -38
    }
    pub fn sys_connect(_sockfd: usize, _addr: *const u8, _addrlen: usize) -> isize {
        -38
    }
    pub fn sys_getsockname(_sockfd: usize, _addr: *mut u8, _addrlen: *mut usize) -> isize {
        -38
    }
    pub fn sys_getpeername(_sockfd: usize, _addr: *mut u8, _addrlen: *mut usize) -> isize {
        -38
    }
    pub fn sys_sendto(
        _sockfd: usize,
        _buf: *const u8,
        _len: usize,
        _flags: usize,
        _dest_addr: *const u8,
        _addrlen: usize,
    ) -> isize {
        -38
    }
    pub fn sys_recvfrom(
        _sockfd: usize,
        _buf: *mut u8,
        _len: usize,
        _flags: usize,
        _src_addr: *mut u8,
        _addrlen: *mut usize,
    ) -> isize {
        -38
    }
    pub fn sys_setsockopt(
        _sockfd: usize,
        _level: usize,
        _optname: usize,
        _optval: *const u8,
        _optlen: usize,
    ) -> isize {
        -38
    }
    pub fn sys_getsockopt(
        _sockfd: usize,
        _level: usize,
        _optname: usize,
        _optval: *mut u8,
        _optlen: *mut usize,
    ) -> isize {
        -38
    }
    pub fn sys_sock_shutdown(_sockfd: usize, _how: usize) -> isize {
        -38
    }
    pub fn sys_sendmsg(_sockfd: usize, _msg: *const u8, _flags: usize) -> isize {
        -38
    }
    pub fn sys_recvmsg(_sockfd: usize, _msg: *mut u8, _flags: usize) -> isize {
        -38
    }
    pub(crate) fn tcp_send(
        _handle: smoltcp::iface::SocketHandle,
        _nonblock: bool,
        _data: &[u8],
    ) -> isize {
        -38
    }
    pub(crate) fn tcp_recv(
        _handle: smoltcp::iface::SocketHandle,
        _nonblock: bool,
        _buf: &mut [u8],
    ) -> isize {
        -38
    }
    pub(crate) fn udp_send(
        _handle: smoltcp::iface::SocketHandle,
        _data: &[u8],
        _dest: smoltcp::wire::IpEndpoint,
    ) -> isize {
        -38
    }
    pub(crate) fn udp_write(
        _handle: smoltcp::iface::SocketHandle,
        _data: &[u8],
        _peer: Option<smoltcp::wire::IpEndpoint>,
    ) -> isize {
        -38
    }
    pub(crate) fn udp_recv(
        _handle: smoltcp::iface::SocketHandle,
        _nonblock: bool,
        _buf: &mut [u8],
    ) -> isize {
        -38
    }
    pub(crate) fn local_tcp_send(
        _conn_id: usize,
        _server_side: bool,
        _nonblock: bool,
        _data: &[u8],
    ) -> isize {
        -38
    }
    pub(crate) fn local_tcp_recv(
        _conn_id: usize,
        _server_side: bool,
        _nonblock: bool,
        _peek: bool,
        _buf: &mut [u8],
    ) -> isize {
        -38
    }
    pub(crate) fn local_tcp_close(_conn_id: usize, _server_side: bool) {}
    pub(crate) fn local_tcp_unlisten(_port: u16) {}
    pub(crate) fn local_tcp_pending_on(_port: u16) -> bool {
        false
    }
    pub(crate) fn local_tcp_poll(_conn_id: usize, _server_side: bool) -> (bool, bool, bool) {
        (false, false, true)
    }
}

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
use imp::NetworkState;
#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
use imp::NET_IFACE;
#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
use imp::NET_INITED;
#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
use smoltcp::socket::dhcpv4::{Event as DhcpEvent, Socket as DhcpSocket};
#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
use smoltcp::wire::{IpCidr, Ipv4Address, Ipv4Cidr};

#[cfg(target_arch = "riscv64")]
const VIRTIO_NET_MMIO: usize = 0x10003000;
#[cfg(target_arch = "aarch64")]
const VIRTIO_NET_MMIO: usize = 0xa00_0300;
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
const VIRTIO_MMIO_DEVICE_ID: usize = 0x08;
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
const VIRTIO_NET_DEVICE_ID: u32 = 1;

pub fn net_init() {
    #[cfg(any(
        target_arch = "riscv64",
        target_arch = "aarch64",
        target_arch = "loongarch64"
    ))]
    {
        // Console output may issue SBI calls; collect the result while the
        // network lock is held and log only after releasing it.
        let mac = {
            let mut state = NET_IFACE.exclusive_access();
            if let Some(state) = state.as_mut() {
                state.inited = true;
                NET_INITED.store(true, core::sync::atomic::Ordering::Relaxed);
                Some(state.device.mac_address())
            } else {
                NET_INITED.store(false, core::sync::atomic::Ordering::Relaxed);
                None
            }
        };
        if let Some(mac) = mac {
            println!("[net] VirtIO-Net initialized, MAC={:02x?}", mac);
            println!("[net] Static IPs: loopback 127.0.0.0/8 + 10.0.2.15/24");
        } else {
            println!("[net] No virtio-net device available, networking disabled");
        }
    }
}

pub fn net_poll() {
    #[cfg(any(
        target_arch = "riscv64",
        target_arch = "aarch64",
        target_arch = "loongarch64"
    ))]
    {
        if !NET_INITED.load(core::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let (readiness_changed, dhcp_update) = {
            let mut state = NET_IFACE.exclusive_access();
            let Some(state) = state.as_mut() else {
                return;
            };
            if !state.inited {
                return;
            }
            let clock = polyhal::time::Time::now().to_msec() as u64;
            state.clock_ms = clock;
            let timestamp = smoltcp::time::Instant::from_millis(clock as i64);
            let NetworkState {
                ref mut iface,
                ref mut device,
                ref mut sockets,
                ref mut dhcp_handle,
                ref mut dhcp_logged,
                ..
            } = state;
            let readiness_changed = iface.poll(timestamp, device, sockets);
            let dhcp_update = dhcp_handle.and_then(|handle| {
                let socket: &mut DhcpSocket = sockets.get_mut(handle);
                socket.poll().map(|event| match event {
                    DhcpEvent::Configured(config) => {
                        let dns_servers = config
                            .dns_servers
                            .iter()
                            .copied()
                            .collect::<alloc::vec::Vec<_>>();
                        (Some(config.address), config.router, dns_servers)
                    }
                    DhcpEvent::Deconfigured => (None, None, alloc::vec::Vec::new()),
                })
            });
            if let Some((address, router, _)) = &dhcp_update {
                let address = address.unwrap_or(Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24));
                iface.update_ip_addrs(|addrs| {
                    if !addrs.is_empty() {
                        addrs[0] = IpCidr::Ipv4(address);
                    } else {
                        let _ = addrs.push(IpCidr::Ipv4(address));
                    }
                });
                let gateway = router.unwrap_or(Ipv4Address::new(10, 0, 2, 2));
                let _ = iface.routes_mut().add_default_ipv4_route(gateway);
                *dhcp_logged = address.address() != Ipv4Address::new(10, 0, 2, 15)
                    || router.is_some();
            }
            (readiness_changed, dhcp_update)
        };
        if let Some((address, router, dns_servers)) = dhcp_update {
            if let Some(address) = address {
                println!("[net] DHCP address={} gateway={:?}", address, router);
                for dns in dns_servers {
                    println!("[net] DHCP DNS={}", dns);
                }
            } else {
                println!("[net] DHCP lease lost; using static QEMU SLIRP fallback");
            }
        }
        if readiness_changed {
            wake_net_blocked_tasks();
        }
    }
}

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
pub fn net_device_present() -> bool {
    let ptr = polyhal::PhysAddr::new(VIRTIO_NET_MMIO + VIRTIO_MMIO_DEVICE_ID).get_mut_ptr();
    let device_id = unsafe { core::ptr::read_volatile(ptr as *const u32) };
    device_id == VIRTIO_NET_DEVICE_ID
}

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
pub fn net_available() -> bool {
    let guard = NET_IFACE.exclusive_access();
    guard.as_ref().map(|state| state.inited).unwrap_or(false)
}

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
pub fn net_available_fast() -> bool {
    NET_INITED.load(core::sync::atomic::Ordering::Relaxed)
}

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
pub fn with_net_iface<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut NetworkState) -> R,
{
    let mut guard = NET_IFACE.exclusive_access();
    guard.as_mut().map(|state| f(state))
}

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
pub fn with_net_iface_tcp_connect(
    handle: smoltcp::iface::SocketHandle,
    remote: smoltcp::wire::IpEndpoint,
    local: smoltcp::wire::IpListenEndpoint,
) -> isize {
    let mut guard = NET_IFACE.exclusive_access();
    if let Some(state) = guard.as_mut() {
        let sock: &mut smoltcp::socket::tcp::Socket = state.sockets.get_mut(handle);
        let cx: &mut smoltcp::iface::Context = unsafe { core::mem::transmute(&mut state.iface) };
        match sock.connect(cx, remote, local) {
            Ok(()) => 0,
            Err(e) => -111,
        }
    } else {
        -1
    }
}

#[cfg(target_arch = "loongarch64")]
pub fn net_device_present() -> bool {
    NET_IFACE.exclusive_access().is_some()
}

#[cfg(not(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
)))]
pub fn net_available() -> bool {
    false
}

#[cfg(not(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
)))]
pub fn net_available_fast() -> bool {
    false
}

#[cfg(not(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
)))]
pub fn with_net_iface<F, R>(_f: F) -> Option<R>
where
    F: FnOnce(&mut ()) -> R,
{
    None
}

#[cfg(not(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
)))]
pub fn with_net_iface_tcp_connect(
    _handle: smoltcp::iface::SocketHandle,
    _remote: smoltcp::wire::IpEndpoint,
    _local: smoltcp::wire::IpListenEndpoint,
) -> isize {
    -97
}

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
pub fn wake_net_blocked_tasks() {
    use crate::task::{net_blocked_snapshot, wake_task, BlockReason};
    use smoltcp::iface::SocketHandle;
    use smoltcp::socket::tcp::Socket as TcpSocket;
    use smoltcp::socket::udp::Socket as UdpSocket;

    // Device polling and TCB inspection happen after taking a stable snapshot;
    // no scheduler queue guard is retained across either operation.  Selected
    // PIDs are then removed in one short scheduler-locked phase.
    let mut wake_tasks = net_blocked_snapshot();
    wake_tasks.retain(|task| {
        let (reason, local_pending) = {
            let inner = task.inner_exclusive_access();
            let reason = inner.block_reason;
            let local_pending = match reason {
                Some(BlockReason::TcpAccept(handle)) => inner.fd_table.iter().any(|file| {
                    let Some(file) = file else {
                        return false;
                    };
                    let Some(sock) = file
                        .as_any()
                        .downcast_ref::<crate::net::socket::SocketFile>()
                    else {
                        return false;
                    };
                    if sock.socket_type != crate::net::socket::SocketType::Tcp {
                        return false;
                    }
                    let Some(socket_set_handle) = sock.get_handle() else {
                        return false;
                    };
                    let socket_handle: usize = unsafe { core::mem::transmute(socket_set_handle) };
                    socket_handle == handle
                        && sock
                            .get_local_endpoint()
                            .map(|ep| crate::net::syscall::local_tcp_pending_on(ep.port))
                            .unwrap_or(false)
                }),
                _ => false,
            };
            (reason, local_pending)
        };
        let should_wake = match reason {
            Some(BlockReason::TcpRecv(handle)) => {
                let handle: SocketHandle = unsafe { core::mem::transmute(handle) };
                with_net_iface(|iface| {
                    let sock: &TcpSocket = iface.sockets.get(handle);
                    sock.can_recv() || !sock.may_recv()
                })
                .unwrap_or(true)
            }
            Some(BlockReason::TcpSend(handle)) => {
                let handle: SocketHandle = unsafe { core::mem::transmute(handle) };
                with_net_iface(|iface| {
                    let sock: &TcpSocket = iface.sockets.get(handle);
                    sock.can_send()
                })
                .unwrap_or(true)
            }
            Some(BlockReason::TcpAccept(handle)) => {
                let handle: SocketHandle = unsafe { core::mem::transmute(handle) };
                local_pending
                    || with_net_iface(|iface| {
                        let sock: &TcpSocket = iface.sockets.get(handle);
                        sock.is_active() && !sock.is_listening()
                    })
                    .unwrap_or(true)
            }
            Some(BlockReason::TcpConnect(handle)) => {
                let handle: SocketHandle = unsafe { core::mem::transmute(handle) };
                with_net_iface(|iface| {
                    let sock: &TcpSocket = iface.sockets.get(handle);
                    // Wake on either successful establishment or a terminal
                    // failure.  is_active() is true even in SYN-SENT and false
                    // again after an RST, so it cannot represent connect(2)
                    // completion.
                    sock.state() != smoltcp::socket::tcp::State::SynSent
                })
                .unwrap_or(true)
            }
            Some(BlockReason::UdpRecv(handle)) => {
                let handle: SocketHandle = unsafe { core::mem::transmute(handle) };
                with_net_iface(|iface| {
                    let sock: &UdpSocket = iface.sockets.get(handle);
                    sock.can_recv()
                })
                .unwrap_or(true)
            }
            Some(BlockReason::PollWait) => true,
            Some(BlockReason::LocalTcpRecv(_, _))
            | Some(BlockReason::LocalTcpSend(_, _))
            | Some(BlockReason::LocalTcpAccept(_))
            | Some(BlockReason::PipeRead(_))
            | Some(BlockReason::PipeWrite(_))
            | Some(BlockReason::Futex(_, _))
            | Some(BlockReason::FilePageLoad(_))
            | Some(BlockReason::SleepLock(_))
            | Some(BlockReason::VmReadLock(_))
            | Some(BlockReason::VmWriteLock(_))
            | Some(BlockReason::ExecPeer(_))
            | Some(BlockReason::SignalWait(_))
            | Some(BlockReason::Vfork(_)) => false,
            None => true,
        };
        should_wake
    });
    for task in wake_tasks {
        let _ = wake_task(task);
    }
}

#[cfg(not(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
)))]
pub fn wake_net_blocked_tasks() {}
