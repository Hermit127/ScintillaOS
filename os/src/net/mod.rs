#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
mod imp {
    use core::sync::atomic::{AtomicBool, Ordering};
    use smoltcp::iface::{Config, Interface, SocketSet};
    use smoltcp::socket::dhcpv4::Socket as DhcpSocket;
    use smoltcp::time::Instant;
    use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Address, Ipv4Cidr};

    use crate::sync::UPSafeCell;
    use lazy_static::lazy_static;

    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    use crate::drivers::net::VirtIONetAdapter as NetDevice;
    #[cfg(target_arch = "loongarch64")]
    use self::loopback::LoopbackOnlyAdapter as NetDevice;

    #[cfg(target_arch = "loongarch64")]
    mod loopback {
        use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
        use smoltcp::time::Instant;

        const NET_BUF_SIZE: usize = 1536;

        pub struct LoopbackOnlyAdapter {
            tx_buf: [u8; NET_BUF_SIZE],
            mac_addr: [u8; 6],
        }

        impl LoopbackOnlyAdapter {
            pub fn new() -> Option<Self> {
                Some(Self {
                    tx_buf: [0u8; NET_BUF_SIZE],
                    mac_addr: [0x52, 0x54, 0x00, 0x12, 0x34, 0x57],
                })
            }

            pub fn mac_address(&self) -> [u8; 6] {
                self.mac_addr
            }
        }

        pub struct LoopbackRxToken;

        impl RxToken for LoopbackRxToken {
            fn consume<R, F>(self, f: F) -> R
            where
                F: FnOnce(&mut [u8]) -> R,
            {
                f(&mut [])
            }
        }

        pub struct LoopbackTxToken<'a> {
            buf: &'a mut [u8; NET_BUF_SIZE],
        }

        impl TxToken for LoopbackTxToken<'_> {
            fn consume<R, F>(self, len: usize, f: F) -> R
            where
                F: FnOnce(&mut [u8]) -> R,
            {
                f(&mut self.buf[..len.min(NET_BUF_SIZE)])
            }
        }

        impl Device for LoopbackOnlyAdapter {
            type RxToken<'a> = LoopbackRxToken;
            type TxToken<'a> = LoopbackTxToken<'a>;

            fn receive(
                &mut self,
                _timestamp: Instant,
            ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
                None
            }

            fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
                Some(LoopbackTxToken {
                    buf: &mut self.tx_buf,
                })
            }

            fn capabilities(&self) -> DeviceCapabilities {
                let mut caps = DeviceCapabilities::default();
                caps.max_transmission_unit = 1500;
                caps.max_burst_size = Some(1);
                caps.medium = Medium::Ethernet;
                caps
            }
        }
    }

    pub static NET_INITED: AtomicBool = AtomicBool::new(false);

    pub struct NetworkState {
        pub device: NetDevice,
        pub iface: Interface,
        pub sockets: SocketSet<'static>,
        pub dhcp_handle: Option<smoltcp::iface::SocketHandle>,
        pub clock_ms: u64,
        pub inited: bool,
        pub dhcp_logged: bool,
    }

    impl NetworkState {
        pub fn new() -> Option<Self> {
            let mut device = NetDevice::new()?;
            let mac_addr = device.mac_address();
            let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac_addr)));
            let now = Instant::from_millis(0);
            let mut iface = Interface::new(config, &mut device, now);

            // Register loopback as a local IP so 127.0.0.1 traffic stays within the stack
            iface.update_ip_addrs(|addrs| {
                // Register 127.0.0.1/32 first (host address, used as source IP)
                let loopback_host = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 32));
                let _ = addrs.push(loopback_host);
                // Register loopback network for routing
                let loopback_net = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 0), 8));
                let _ = addrs.push(loopback_net);
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
            self.iface
                .poll(timestamp, &mut self.device, &mut self.sockets);
        }
    }

    lazy_static! {
        pub static ref NET_IFACE: UPSafeCell<Option<NetworkState>> =
            unsafe { UPSafeCell::new(NetworkState::new()) };
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
    pub fn sys_socket(_domain: usize, _type_: usize, _protocol: usize) -> isize {
        -38
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
    pub fn sys_accept4(
        _sockfd: usize,
        _addr: *mut u8,
        _addrlen: *mut usize,
        _flags: usize,
    ) -> isize {
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
    pub(crate) fn udp_write_with_endpoint(
        _handle: smoltcp::iface::SocketHandle,
        _data: &[u8],
        _local: Option<smoltcp::wire::IpEndpoint>,
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
    pub(crate) fn udp_recv_with_endpoints(
        _handle: smoltcp::iface::SocketHandle,
        _nonblock: bool,
        _buf: &mut [u8],
        _local: Option<smoltcp::wire::IpEndpoint>,
        _peer: Option<smoltcp::wire::IpEndpoint>,
    ) -> isize {
        -38
    }
    pub(crate) fn local_udp_pending_on(_port: u16) -> bool {
        false
    }
    pub(crate) fn local_udp_close(
        _local: smoltcp::wire::IpEndpoint,
        _peer: Option<smoltcp::wire::IpEndpoint>,
    ) {
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
        _buf: &mut [u8],
    ) -> isize {
        -38
    }
    pub(crate) fn local_tcp_close(_conn_id: usize, _server_side: bool) {}
    pub(crate) fn local_tcp_unlisten(_port: u16) {}
    pub(crate) fn local_tcp_poll(_conn_id: usize, _server_side: bool) -> (bool, bool, bool) {
        (false, false, false)
    }
    pub(crate) fn local_tcp_pending_on(_port: u16) -> bool {
        false
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

pub fn net_init() {
    #[cfg(any(
        target_arch = "riscv64",
        target_arch = "aarch64",
        target_arch = "loongarch64"
    ))]
    {
        let mut state = NET_IFACE.exclusive_access();
        if let Some(state) = state.as_mut() {
            state.inited = true;
            NET_INITED.store(true, core::sync::atomic::Ordering::Relaxed);
            println!(
                "[net] VirtIO-Net initialized, MAC={:02x?}",
                state.device.mac_address()
            );
            println!("[net] Static IPs: loopback 127.0.0.0/8 + 10.0.2.15/24");
        } else {
            NET_INITED.store(false, core::sync::atomic::Ordering::Relaxed);
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
        {
            let mut state = NET_IFACE.exclusive_access();
            if let Some(state) = state.as_mut() {
                if !state.inited {
                    return;
                }
                state.clock_ms += 10;
                let clock = state.clock_ms;
                let timestamp = smoltcp::time::Instant::from_millis(clock as i64);
                let NetworkState {
                    ref mut iface,
                    ref mut device,
                    ref mut sockets,
                    ref mut dhcp_handle,
                    ref mut dhcp_logged,
                    ..
                } = state;
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
        wake_net_blocked_tasks();
    }
}

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
pub fn net_device_present() -> bool {
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    {
        crate::drivers::net::has_mmio_net_device()
    }
    #[cfg(target_arch = "loongarch64")]
    {
        true
    }
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

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
pub fn wake_net_blocked_tasks() {
    use crate::task::{add_priority_task, add_task, BlockReason, TaskStatus, NET_BLOCK_QUEUE};
    use smoltcp::iface::SocketHandle;
    use smoltcp::socket::tcp::Socket as TcpSocket;
    use smoltcp::socket::udp::Socket as UdpSocket;

    let mut to_wake = alloc::vec::Vec::new();
    let orig_blocked;
    {
        let mut queue = NET_BLOCK_QUEUE.exclusive_access();
        orig_blocked = queue.len();
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
                        })
                        .unwrap_or(true)
                    }
                    Some(BlockReason::TcpSend(h)) => {
                        let handle: SocketHandle = unsafe { core::mem::transmute(*h) };
                        with_net_iface(|iface| {
                            let sock: &TcpSocket = iface.sockets.get(handle);
                            sock.can_send()
                        })
                        .unwrap_or(true)
                    }
                    Some(BlockReason::TcpAccept(h)) => {
                        let handle: SocketHandle = unsafe { core::mem::transmute(*h) };
                        with_net_iface(|iface| {
                            let sock: &TcpSocket = iface.sockets.get(handle);
                            sock.is_active() && !sock.is_listening()
                        })
                        .unwrap_or(true)
                    }
                    Some(BlockReason::TcpConnect(h)) => {
                        let handle: SocketHandle = unsafe { core::mem::transmute(*h) };
                        with_net_iface(|iface| {
                            let sock: &TcpSocket = iface.sockets.get(handle);
                            sock.is_active()
                        })
                        .unwrap_or(true)
                    }
                    Some(BlockReason::UdpRecv(h)) => {
                        let handle: SocketHandle = unsafe { core::mem::transmute(*h) };
                        with_net_iface(|iface| {
                            let sock: &UdpSocket = iface.sockets.get(handle);
                            sock.can_recv()
                        })
                        .unwrap_or(true)
                    }
                    Some(BlockReason::PollWait) => true,
                    Some(BlockReason::PipeRead(_))
                    | Some(BlockReason::PipeWrite(_))
                    | Some(BlockReason::Futex(_)) => false,
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
    if !to_wake.is_empty() {
    } else if orig_blocked > 0 {
    }
    if !to_wake.is_empty() {}
    for task in &to_wake {
        add_task(task.clone());
    }
}

#[cfg(not(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
)))]
pub fn wake_net_blocked_tasks() {}
