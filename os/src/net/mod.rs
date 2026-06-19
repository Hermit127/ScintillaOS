// 这里负责操作系统的网络管理：初始化网络设备和 smoltcp 协议栈，并提供网络轮询和唤醒入口。
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

    #[cfg(target_arch = "loongarch64")]
    use self::loopback::LoopbackOnlyAdapter as NetDevice;
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    use crate::drivers::net::VirtIONetAdapter as NetDevice;

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
            // LoongArch 当前只提供本地回环网卡对象。
            // 执行时创建一块发送缓冲区，并填入固定 MAC 地址，让上层 smoltcp 能按以太网设备初始化。
            pub fn new() -> Option<Self> {
                Some(Self {
                    tx_buf: [0u8; NET_BUF_SIZE],
                    mac_addr: [0x52, 0x54, 0x00, 0x12, 0x34, 0x57],
                })
            }

            // 返回回环适配器使用的固定 MAC 地址。
            // 执行时只复制结构体里保存的 6 字节地址，供 Interface 配置硬件地址。
            pub fn mac_address(&self) -> [u8; 6] {
                self.mac_addr
            }
        }

        pub struct LoopbackRxToken;

        impl RxToken for LoopbackRxToken {
            // 回环空接收 token，没有真实帧数据。
            // 执行时把空切片交给 smoltcp 回调，因为这个适配器不会从真实设备收到帧。
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
            // 让 smoltcp 写入回环发送缓冲区。
            // 执行时按 smoltcp 要求截取发送长度，避免超过固定缓冲区大小。
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

            // 纯回环适配器不从硬件接包。
            // 执行时直接返回 None，让协议栈知道当前没有可处理的接收帧。
            fn receive(
                &mut self,
                _timestamp: Instant,
            ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
                None
            }

            // 提供一个发送 token 给协议栈。
            // 执行时借出内部发送缓冲区，smoltcp 可以把要发出的以太网帧写进去。
            fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
                Some(LoopbackTxToken {
                    buf: &mut self.tx_buf,
                })
            }

            // 返回回环适配器的以太网能力。
            // 执行时声明 MTU、单次突发数量和链路类型，供 smoltcp 决定分片和收发策略。
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
        // 初始化 smoltcp interface、socket set 和 DHCP socket。
        // 执行时打开网络设备，配置 MAC、本机地址和 socket 集合，并预先加入 DHCP socket。
        pub fn new() -> Option<Self> {
            let mut device = NetDevice::new()?;
            let mac_addr = device.mac_address();
            let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac_addr)));
            let now = Instant::from_millis(0);
            let mut iface = Interface::new(config, &mut device, now);

            // 注册回环地址，让 127.0.0.1 流量留在协议栈内部。
            iface.update_ip_addrs(|addrs| {
                // 先注册 127.0.0.1/32，作为本机源地址。
                let loopback_host = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 32));
                let _ = addrs.push(loopback_host);
                // 再注册 127.0.0.0/8，用于路由匹配。
                let loopback_net = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 0), 8));
                let _ = addrs.push(loopback_net);
                // DHCP 没成功时使用静态地址兜底。
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

        // 驱动 smoltcp 轮询一次。
        // 执行时更新时间戳，再让 smoltcp 同时处理设备收发和所有 socket 的状态机。
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
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不检查参数内容，因为该平台没有网络后端，统一按 Linux ENOSYS 返回。
    pub fn sys_socket(_domain: usize, _type_: usize, _protocol: usize) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不创建绑定状态，避免让用户态误以为 socket 子系统存在。
    pub fn sys_bind(_sockfd: usize, _addr: *const u8, _addrlen: usize) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不维护监听队列，直接告诉 libc 当前系统调用不可用。
    pub fn sys_listen(_sockfd: usize, _backlog: usize) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不会分配 accepted fd，也不会写回 sockaddr。
    pub fn sys_accept(_sockfd: usize, _addr: *mut u8, _addrlen: *mut usize) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时忽略 flags，因为没有底层 socket 可继承非阻塞或 close-on-exec 状态。
    pub fn sys_accept4(
        _sockfd: usize,
        _addr: *mut u8,
        _addrlen: *mut usize,
        _flags: usize,
    ) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不发起 TCP 握手，也不记录 UDP 默认对端。
    pub fn sys_connect(_sockfd: usize, _addr: *const u8, _addrlen: usize) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不会写回本地地址，用户态据此知道没有 socket 后端。
    pub fn sys_getsockname(_sockfd: usize, _addr: *mut u8, _addrlen: *mut usize) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不会写回对端地址，保持和其他网络入口一致的不可用语义。
    pub fn sys_getpeername(_sockfd: usize, _addr: *mut u8, _addrlen: *mut usize) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不读取用户发送缓冲区，直接返回不可用，避免无网络平台产生副作用。
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
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不写用户接收缓冲区，也不修改来源地址长度。
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
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时接受但忽略 socket 选项参数，统一报告该调用不存在。
    pub fn sys_setsockopt(
        _sockfd: usize,
        _level: usize,
        _optname: usize,
        _optval: *const u8,
        _optlen: usize,
    ) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不写回 optval/optlen，避免伪造不存在的 socket 状态。
    pub fn sys_getsockopt(
        _sockfd: usize,
        _level: usize,
        _optname: usize,
        _optval: *mut u8,
        _optlen: *mut usize,
    ) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不记录半关闭状态，直接返回不可用。
    pub fn sys_sock_shutdown(_sockfd: usize, _how: usize) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不读取 msghdr/iovec，保持网络发送入口的一致失败语义。
    pub fn sys_sendmsg(_sockfd: usize, _msg: *const u8, _flags: usize) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    // 执行时不写回 msghdr/iovec，保持网络接收入口的一致失败语义。
    pub fn sys_recvmsg(_sockfd: usize, _msg: *mut u8, _flags: usize) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    pub(crate) fn tcp_send(
        _handle: smoltcp::iface::SocketHandle,
        _nonblock: bool,
        _data: &[u8],
    ) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    pub(crate) fn tcp_recv(
        _handle: smoltcp::iface::SocketHandle,
        _nonblock: bool,
        _buf: &mut [u8],
    ) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    pub(crate) fn udp_send(
        _handle: smoltcp::iface::SocketHandle,
        _data: &[u8],
        _dest: smoltcp::wire::IpEndpoint,
    ) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    pub(crate) fn udp_write(
        _handle: smoltcp::iface::SocketHandle,
        _data: &[u8],
        _peer: Option<smoltcp::wire::IpEndpoint>,
    ) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    pub(crate) fn udp_write_with_endpoint(
        _handle: smoltcp::iface::SocketHandle,
        _data: &[u8],
        _local: Option<smoltcp::wire::IpEndpoint>,
        _peer: Option<smoltcp::wire::IpEndpoint>,
    ) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    pub(crate) fn udp_recv(
        _handle: smoltcp::iface::SocketHandle,
        _nonblock: bool,
        _buf: &mut [u8],
    ) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    pub(crate) fn udp_recv_with_endpoints(
        _handle: smoltcp::iface::SocketHandle,
        _nonblock: bool,
        _buf: &mut [u8],
        _local: Option<smoltcp::wire::IpEndpoint>,
        _peer: Option<smoltcp::wire::IpEndpoint>,
    ) -> isize {
        -38
    }
    // 无网络实现时没有本地 UDP 待收包。
    pub(crate) fn local_udp_pending_on(_port: u16) -> bool {
        false
    }
    // 无网络实现时本地 UDP 关闭不用做事。
    pub(crate) fn local_udp_close(
        _local: smoltcp::wire::IpEndpoint,
        _peer: Option<smoltcp::wire::IpEndpoint>,
    ) {
    }
    // 无网络实现的架构直接返回 ENOSYS。
    pub(crate) fn local_tcp_send(
        _conn_id: usize,
        _server_side: bool,
        _nonblock: bool,
        _data: &[u8],
    ) -> isize {
        -38
    }
    // 无网络实现的架构直接返回 ENOSYS。
    pub(crate) fn local_tcp_recv(
        _conn_id: usize,
        _server_side: bool,
        _nonblock: bool,
        _buf: &mut [u8],
    ) -> isize {
        -38
    }
    // 无网络实现时本地 TCP 关闭不用做事。
    pub(crate) fn local_tcp_close(_conn_id: usize, _server_side: bool) {}
    // 无网络实现时监听表为空。
    pub(crate) fn local_tcp_unlisten(_port: u16) {}
    // 无网络实现时本地 TCP 永远不可读写。
    pub(crate) fn local_tcp_poll(_conn_id: usize, _server_side: bool) -> (bool, bool, bool) {
        (false, false, false)
    }
    // 无网络实现时没有待接受的本地 TCP 连接。
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

// 初始化网络子系统，并记录是否真的有网卡可用。
// 执行时检查全局 NetworkState 是否创建成功，成功就打开 NET_INITED 并打印地址信息，失败则关闭网络能力。
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

// 定时轮询网络协议栈，并唤醒可能已经可继续的网络任务。
// 执行时推进协议栈时钟，轮询设备和 socket；如果 DHCP 有新地址则打印一次，然后唤醒网络等待队列。
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
// 快速探测当前平台是否存在网络设备。
// 执行时 riscv/aarch64 查询 MMIO virtio-net，LoongArch 使用回环适配器所以固定认为可用。
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
// 读取网络状态对象，判断网络是否已经初始化。
// 执行时进入全局网络状态锁，只有 NetworkState 存在且 inited 标记为真才返回可用。
pub fn net_available() -> bool {
    let guard = NET_IFACE.exclusive_access();
    guard.as_ref().map(|state| state.inited).unwrap_or(false)
}

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
// 用原子标志快速判断网络是否可用。
// 执行时只读 NET_INITED 原子位，给频繁路径提供无需加锁的快速判断。
pub fn net_available_fast() -> bool {
    NET_INITED.load(core::sync::atomic::Ordering::Relaxed)
}

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
// 独占访问网络状态，并在里面执行调用者传入的闭包。
// 执行时拿到 NET_IFACE 的独占访问权；网络未初始化时返回 None，避免调用者碰到空状态。
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
// 专门包一层 TCP connect，绕开 smoltcp 对 Context 的借用限制。
// 执行时同时借出 socket 和 iface context；这里用 unsafe 转换拆开借用，是为了满足 smoltcp connect 的接口形态。
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
// 检查网络阻塞队列，把 socket 状态已经满足的任务唤醒。
// 执行时逐个还原保存的 SocketHandle，查看 TCP/UDP 是否可读写或连接完成，满足条件的任务重新加入就绪队列。
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
                    None => true, // block_reason 已清空，应该唤醒。
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
// 无网络实现时没有需要唤醒的网络任务。
// 执行时保持空实现，因为这些平台不会把任务挂到网络等待队列。
pub fn wake_net_blocked_tasks() {}
