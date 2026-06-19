// 这里负责操作系统的网络系统调用接口：实现 socket 相关系统调用，以及本地 TCP/UDP 回环兼容层。
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::Socket as TcpSocket;
use smoltcp::socket::tcp::SocketBuffer as TcpSocketBuffer;
use smoltcp::socket::udp::PacketBuffer as UdpPacketBuffer;
use smoltcp::socket::udp::PacketMetadata as UdpPacketMetadata;
use smoltcp::socket::udp::Socket as UdpSocket;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

use crate::mm::{translated_byte_buffer, translated_ref, translated_refmut};
use crate::net::socket::{SocketFile, SocketType};
use crate::sync::UPSafeCell;
use crate::task::{
    add_task, block_current_and_run_next, current_task, current_user_token,
    suspend_current_and_run_next, wake_poll_wait_tasks, BlockReason, TaskStatus, NET_BLOCK_QUEUE,
};
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use lazy_static::*;
use polyhal::pagetable::PageTable;

const SOCKET_BUFFER_SIZE: usize = 65536;
const LOCAL_TCP_BUFFER_SIZE: usize = 1024 * 1024;
const LOCAL_UDP_QUEUE_LIMIT: usize = 256;
const UDP_META_COUNT: usize = 16;
const AF_INET: usize = 2;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;
const SOCK_NONBLOCK: usize = 0x800;
const SOCK_CLOEXEC: usize = 0x80000;

const MSG_DONTWAIT: usize = 0x40;
const MSG_PEEK: usize = 0x02;
const MSG_NOSIGNAL: usize = 0x4000;

const SOL_SOCKET: usize = 1;
const SO_REUSEADDR: usize = 2;
const SO_ERROR: usize = 4;
const SO_RCVBUF: usize = 8;
const SO_SNDBUF: usize = 7;
const SO_KEEPALIVE: usize = 9;

const IPPROTO_TCP: usize = 6;
const TCP_NODELAY: usize = 1;
const TCP_MAXSEG: usize = 2;
const TCP_INFO: usize = 11;
const TCP_CONGESTION: usize = 13;

const SHUT_RD: usize = 0;
const SHUT_WR: usize = 1;
const SHUT_RDWR: usize = 2;
static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(49152);
static NEXT_LOCAL_TCP_ID: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone)]
struct LocalUdpPacket {
    dst_port: u16,
    src: IpEndpoint,
    data: Vec<u8>,
}

#[derive(Clone)]
struct LocalTcpConn {
    client: IpEndpoint,
    server: IpEndpoint,
    c2s: VecDeque<u8>,
    s2c: VecDeque<u8>,
    client_closed: bool,
    server_closed: bool,
}

lazy_static! {
    static ref LOCAL_UDP_QUEUE: UPSafeCell<VecDeque<LocalUdpPacket>> =
        unsafe { UPSafeCell::new(VecDeque::new()) };
    static ref LOCAL_TCP_PENDING: UPSafeCell<BTreeMap<u16, VecDeque<usize>>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
    static ref LOCAL_TCP_CONNS: UPSafeCell<BTreeMap<usize, LocalTcpConn>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
    static ref LOCAL_TCP_LISTEN_PORTS: UPSafeCell<BTreeSet<u16>> =
        unsafe { UPSafeCell::new(BTreeSet::new()) };
}

// 判断网络子系统是否已经初始化可用。
// 执行时走快速原子标志，不进入协议栈锁，用在每个 socket syscall 的入口检查。
fn net_ready() -> bool {
    crate::net::net_available_fast()
}

// 分配一个临时端口号。
// 执行时用原子递增拿端口；如果越界或落到保留区，就把序列重置回临时端口起点。
fn next_ephemeral_port() -> u16 {
    let old = NEXT_EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
    if old < 49152 || old == u16::MAX {
        NEXT_EPHEMERAL_PORT.store(49153, Ordering::Relaxed);
        49152
    } else {
        old
    }
}

#[repr(C)]
struct SockAddrIn {
    sin_family: u16,
    sin_port: u16,
    sin_addr: u32,
    sin_zero: [u8; 8],
}

#[repr(C)]
struct Iovec {
    iov_base: *mut u8,
    iov_len: usize,
}

#[repr(C)]
struct Msghdr {
    msg_name: *mut u8,
    msg_namelen: u32,
    msg_iov: *mut Iovec,
    msg_iovlen: usize,
    msg_control: *mut u8,
    msg_controllen: usize,
    msg_flags: i32,
}

// 从用户传入的 sockaddr_in 解析 IPv4 地址和端口。
// 执行时先通过当前用户页表翻译指针，再检查地址族，并把网络字节序的端口和地址转回主机序。
fn parse_sockaddr_in(ptr: *const u8) -> Option<(u32, u16)> {
    if ptr.is_null() {
        return None;
    }
    let token = current_user_token();
    let sa: &SockAddrIn = translated_ref(token, ptr as *const SockAddrIn);
    if sa.sin_family as usize != AF_INET {
        return None;
    }
    let port = u16::from_be(sa.sin_port);
    let addr = u32::from_be(sa.sin_addr);
    Some((addr, port))
}

// 把 IPv4 地址和端口写回用户 sockaddr_in。
// 执行时在内核栈上组装 sockaddr_in，再通过用户页表把结构体写到用户缓冲区。
fn write_sockaddr_in(ptr: *mut u8, sin_addr: u32, sin_port: u16) {
    let token = current_user_token();
    let sa = SockAddrIn {
        sin_family: AF_INET as u16,
        sin_port: sin_port.to_be(),
        sin_addr: sin_addr.to_be(),
        sin_zero: [0; 8],
    };
    *translated_refmut(token, ptr as *mut SockAddrIn) = sa;
}

// 统计 iovec 数组总长度。
// 执行时把 iovec 指针视为调用方已经翻译好的数组，逐项累加长度并用 saturating_add 防溢出。
fn iov_total_len(iov: *const Iovec, iovcnt: usize) -> usize {
    let iovs = unsafe { core::slice::from_raw_parts(iov, iovcnt) };
    let mut total = 0usize;
    for i in iovs {
        total = total.saturating_add(i.iov_len);
    }
    total
}

// 从用户 iovec 拷贝数据到内核缓冲区。
// 执行时按 iovec 顺序把多段用户数据拼进连续缓冲区，缓冲区满了就停止。
fn copy_from_iov(buf: &mut [u8], iov: *const Iovec, iovcnt: usize) -> usize {
    let iovs = unsafe { core::slice::from_raw_parts(iov, iovcnt) };
    let mut written = 0usize;
    for i in iovs {
        if written >= buf.len() {
            break;
        }
        let len = i.iov_len.min(buf.len() - written);
        unsafe {
            core::ptr::copy_nonoverlapping(i.iov_base, buf.as_mut_ptr().add(written), len);
        }
        written += len;
    }
    written
}

// 把内核缓冲区数据分散写回用户 iovec。
// 执行时按 iovec 顺序把连续数据拆回多段用户缓冲区，只写实际收到的字节。
fn copy_to_iov(buf: &[u8], iov: *mut Iovec, iovcnt: usize) -> usize {
    let iovs = unsafe { core::slice::from_raw_parts(iov, iovcnt) };
    let mut read = 0usize;
    for i in iovs {
        if read >= buf.len() {
            break;
        }
        let len = i.iov_len.min(buf.len() - read);
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr().add(read), i.iov_base, len);
        }
        read += len;
    }
    read
}

// 在 smoltcp socket set 里新建 TCP 或 UDP socket。
// 执行时进入全局网络状态，为 TCP/UDP 分配收发缓冲区并把 socket 加入 smoltcp 的 SocketSet。
fn add_to_socket_set(st: SocketType) -> Option<(SocketHandle, SocketType)> {
    crate::net::with_net_iface(|iface| {
        let handle = match st {
            SocketType::Tcp => {
                let sock = TcpSocket::new(
                    TcpSocketBuffer::new(alloc::vec![0u8; SOCKET_BUFFER_SIZE]),
                    TcpSocketBuffer::new(alloc::vec![0u8; SOCKET_BUFFER_SIZE]),
                );
                iface.sockets.add(sock)
            }
            SocketType::Udp => {
                let meta_vec = alloc::vec![UdpPacketMetadata::EMPTY; UDP_META_COUNT];
                let payload_vec = alloc::vec![0u8; SOCKET_BUFFER_SIZE];
                let sock = UdpSocket::new(
                    UdpPacketBuffer::new(meta_vec.clone(), payload_vec.clone()),
                    UdpPacketBuffer::new(meta_vec, payload_vec),
                );
                iface.sockets.add(sock)
            }
        };
        (handle, st)
    })
}

// 通过 fd 找到 SocketFile，并取出 handle、类型和 nonblock 状态。
// 执行时先按输入条件查缓存或表项，缓存未命中时继续访问底层结构，最后返回找到的对象。
fn sock_lookup(sockfd: usize) -> Option<(SocketHandle, SocketType, bool)> {
    let task = current_task()?;
    let inner = task.inner_exclusive_access();
    if sockfd >= inner.fd_table.len() {
        return None;
    }
    let file = inner.fd_table[sockfd].as_ref()?;
    let sock = file.as_any().downcast_ref::<SocketFile>()?;
    let handle = sock.get_handle();
    let st = sock.socket_type;
    let nonblock = sock.get_nonblock();
    drop(inner);
    Some((handle, st, nonblock))
}

// 把本地地址记录到 socket fd 对象里。
// 执行时从当前进程 fd 表找到 SocketFile，并把 local endpoint 写入 fd 层缓存。
fn set_socket_local_endpoint(sockfd: usize, endpoint: IpEndpoint) {
    let Some(task) = current_task() else {
        return;
    };
    let inner = task.inner_exclusive_access();
    if sockfd >= inner.fd_table.len() {
        return;
    }
    if let Some(file) = &inner.fd_table[sockfd] {
        if let Some(sock) = file.as_any().downcast_ref::<SocketFile>() {
            sock.set_local_endpoint(endpoint);
        }
    }
}

// 把对端地址记录到 socket fd 对象里。
// 执行时从当前进程 fd 表找到 SocketFile，并把 peer endpoint 写入 fd 层缓存。
fn set_socket_peer_endpoint(sockfd: usize, endpoint: IpEndpoint) {
    let Some(task) = current_task() else {
        return;
    };
    let inner = task.inner_exclusive_access();
    if sockfd >= inner.fd_table.len() {
        return;
    }
    if let Some(file) = &inner.fd_table[sockfd] {
        if let Some(sock) = file.as_any().downcast_ref::<SocketFile>() {
            sock.set_peer_endpoint(endpoint);
        }
    }
}

// 从 socket fd 对象里读取本地地址。
// 执行时检查 fd 合法性和具体类型，成功后返回 SocketFile 里保存的 local endpoint。
fn get_socket_local_endpoint(sockfd: usize) -> Option<IpEndpoint> {
    let task = current_task()?;
    let inner = task.inner_exclusive_access();
    if sockfd >= inner.fd_table.len() {
        return None;
    }
    let file = inner.fd_table[sockfd].as_ref()?;
    let sock = file.as_any().downcast_ref::<SocketFile>()?;
    sock.get_local_endpoint()
}

// 从 socket fd 对象里读取对端地址。
// 执行时检查 fd 合法性和具体类型，成功后返回 SocketFile 里保存的 peer endpoint。
fn get_socket_peer_endpoint(sockfd: usize) -> Option<IpEndpoint> {
    let task = current_task()?;
    let inner = task.inner_exclusive_access();
    if sockfd >= inner.fd_table.len() {
        return None;
    }
    let file = inner.fd_table[sockfd].as_ref()?;
    let sock = file.as_any().downcast_ref::<SocketFile>()?;
    sock.get_peer_endpoint()
}

// 记录这个 fd 对应的本地 TCP 回环连接。
// 执行时把连接 id 和端角色写进 SocketFile，后续收发就不再经过 smoltcp TCP。
fn set_socket_local_tcp(sockfd: usize, conn_id: usize, server_side: bool) {
    let Some(task) = current_task() else {
        return;
    };
    let inner = task.inner_exclusive_access();
    if sockfd >= inner.fd_table.len() {
        return;
    }
    if let Some(file) = &inner.fd_table[sockfd] {
        if let Some(sock) = file.as_any().downcast_ref::<SocketFile>() {
            sock.set_local_tcp(conn_id, server_side);
        }
    }
}

// 读取 fd 对应的本地 TCP 回环连接。
// 执行时从 fd 表取出 SocketFile，再返回里面记录的本地 TCP 连接信息。
fn get_socket_local_tcp(sockfd: usize) -> Option<(usize, bool)> {
    let task = current_task()?;
    let inner = task.inner_exclusive_access();
    if sockfd >= inner.fd_table.len() {
        return None;
    }
    let file = inner.fd_table[sockfd].as_ref()?;
    let sock = file.as_any().downcast_ref::<SocketFile>()?;
    sock.get_local_tcp()
}

// 判断目标地址是不是本机回环或本机静态地址。
// 执行时只看 IPv4 字节：127/8、0.0.0.0 和默认静态地址都按本机路径处理。
fn is_local_ipv4(addr: IpAddress) -> bool {
    let IpAddress::Ipv4(ipv4) = addr;
    ipv4.0[0] == 127 || ipv4.0 == [0, 0, 0, 0] || ipv4.0 == [10, 0, 2, 15]
}

// 根据目标地址挑一个合适的本地源地址。
// 执行时如果已有明确本地 IP 就直接用；否则按目标是否回环选择 127.0.0.1 或 10.0.2.15。
fn source_addr_for_dest(local: IpEndpoint, dest: IpEndpoint) -> IpEndpoint {
    let IpAddress::Ipv4(local_ip) = local.addr;
    if local_ip.0 != [0, 0, 0, 0] {
        return local;
    }
    let IpAddress::Ipv4(dest_ip) = dest.addr;
    let src_ip = if dest_ip.0[0] == 127 {
        Ipv4Address::new(127, 0, 0, 1)
    } else {
        Ipv4Address::new(10, 0, 2, 15)
    };
    IpEndpoint::new(IpAddress::Ipv4(src_ip), local.port)
}

// 从 smoltcp UDP socket 里读取当前绑定端点。
// 执行时进入 SocketSet 查询 UDP endpoint；端口为 0 说明还没绑定，返回 None。
fn smoltcp_udp_endpoint(handle: SocketHandle) -> Option<IpEndpoint> {
    crate::net::with_net_iface(|iface| {
        let sock: &UdpSocket = iface.sockets.get(handle);
        let ep = sock.endpoint();
        if ep.port == 0 {
            None
        } else {
            Some(IpEndpoint::new(
                ep.addr.unwrap_or(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)),
                ep.port,
            ))
        }
    })
    .flatten()
}

// 确保 UDP socket 有本地端点，没绑定就自动分配临时端口。
// 执行时优先复用 fd 缓存或 smoltcp 绑定端点；都没有时自动 bind 临时端口并缓存下来。
fn ensure_udp_local_endpoint(sockfd: usize, handle: SocketHandle, dest: IpEndpoint) -> IpEndpoint {
    if let Some(local) = get_socket_local_endpoint(sockfd) {
        return source_addr_for_dest(local, dest);
    }
    if let Some(local) = smoltcp_udp_endpoint(handle) {
        set_socket_local_endpoint(sockfd, local);
        return source_addr_for_dest(local, dest);
    }

    let port = next_ephemeral_port();
    let local = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), port);
    crate::net::with_net_iface(|iface| {
        let sock: &mut UdpSocket = iface.sockets.get_mut(handle);
        let _ = sock.bind(IpListenEndpoint { addr: None, port });
    });
    set_socket_local_endpoint(sockfd, local);
    source_addr_for_dest(local, dest)
}

// 唤醒可能在等本地 UDP 包的任务。
// 执行时先从等待队列里找满足条件的任务，再把它们改回 Ready，最后交给调度器运行。
fn wake_local_udp_receivers() {
    let mut to_wake = Vec::new();
    {
        let mut queue = NET_BLOCK_QUEUE.exclusive_access();
        let mut i = 0;
        while i < queue.len() {
            let should_wake = {
                let inner = queue[i].inner_exclusive_access();
                matches!(inner.block_reason, Some(BlockReason::UdpRecv(_)))
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
    wake_poll_wait_tasks();
}

// 把本地 UDP 包放进内核模拟队列。
// 执行时把数据复制到本地队列；队列满时丢掉最旧包，并主动让出 CPU 给接收方运行。
fn enqueue_local_udp(src: IpEndpoint, dest: IpEndpoint, data: &[u8]) -> isize {
    let queue_was_full = {
        let mut queue = LOCAL_UDP_QUEUE.exclusive_access();
        let queue_was_full = queue.len() >= LOCAL_UDP_QUEUE_LIMIT;
        if queue_was_full {
            queue.pop_front();
        }
        queue.push_back(LocalUdpPacket {
            dst_port: dest.port,
            src,
            data: data.to_vec(),
        });
        queue_was_full
    };
    wake_local_udp_receivers();
    if queue_was_full {
        crate::fs::stdio::stdout_release_if_owner();
        suspend_current_and_run_next();
    }
    data.len() as isize
}

// 判断队列里的本地 UDP 包是否匹配当前接收端。
// 执行时比较目标端口，并在 socket 已 connect 的情况下额外要求来源端点一致。
fn local_udp_packet_matches(
    packet: &LocalUdpPacket,
    local: IpEndpoint,
    peer: Option<IpEndpoint>,
) -> bool {
    packet.dst_port == local.port && peer.map(|peer| packet.src == peer).unwrap_or(true)
}

// 如果目标是本机地址，就优先走本地 UDP 队列。
// 执行时先判断目标 IP 是否属于本机；命中时确保本地端点存在，并把数据投递到内核 UDP 队列。
fn try_send_local_udp(
    sockfd: usize,
    handle: SocketHandle,
    data: &[u8],
    dest: IpEndpoint,
) -> Option<isize> {
    if !is_local_ipv4(dest.addr) {
        return None;
    }
    let src = ensure_udp_local_endpoint(sockfd, handle, dest);
    Some(enqueue_local_udp(src, dest, data))
}

// 判断本地 UDP 队列里是否有发往指定端口的包。
pub(crate) fn local_udp_pending_on(port: u16) -> bool {
    LOCAL_UDP_QUEUE
        .exclusive_access()
        .iter()
        .any(|packet| packet.dst_port == port)
}

// 关闭本地 UDP 端点时清理队列里相关的包。
pub(crate) fn local_udp_close(local: IpEndpoint, peer: Option<IpEndpoint>) {
    LOCAL_UDP_QUEUE
        .exclusive_access()
        .retain(|packet| !local_udp_packet_matches(packet, local, peer));
}

// 计算当前 UDP fd 用来接收本地包的端点。
// 执行时先用 fd 缓存；如果没有，就从 smoltcp UDP 绑定状态补回缓存。
fn local_recv_endpoint(sockfd: usize, handle: SocketHandle) -> Option<IpEndpoint> {
    get_socket_local_endpoint(sockfd).or_else(|| {
        let ep = smoltcp_udp_endpoint(handle)?;
        set_socket_local_endpoint(sockfd, ep);
        Some(ep)
    })
}

// 从本地 UDP 队列里尝试取一个匹配当前 fd 的包。
// 执行时按 fd 的本地端点和默认对端匹配队列，支持 MSG_PEEK，并可把来源地址写回用户态。
fn try_recv_local_udp(
    sockfd: usize,
    handle: SocketHandle,
    buf: &mut [u8],
    src_addr: *mut u8,
    addrlen: *mut usize,
    peek: bool,
) -> Option<isize> {
    let local = local_recv_endpoint(sockfd, handle)?;
    let peer = get_socket_peer_endpoint(sockfd);
    let mut queue = LOCAL_UDP_QUEUE.exclusive_access();
    let pos = queue
        .iter()
        .position(|packet| local_udp_packet_matches(packet, local, peer))?;
    let packet = if peek {
        queue[pos].clone()
    } else {
        queue.remove(pos)?
    };
    let n = packet.data.len().min(buf.len());
    buf[..n].copy_from_slice(&packet.data[..n]);
    drop(queue);

    if !src_addr.is_null() && !addrlen.is_null() {
        let IpAddress::Ipv4(ipv4) = packet.src.addr;
        let token = current_user_token();
        *translated_refmut(token, addrlen as *mut u32) = core::mem::size_of::<SockAddrIn>() as u32;
        write_sockaddr_in(src_addr, u32::from_be_bytes(ipv4.0), packet.src.port);
    }

    Some(n as isize)
}

// 按明确的本地和对端端点从本地 UDP 队列取包。
// 执行时用传入的 local/peer 直接匹配队列，取到后复制数据并从队列移除。
fn try_recv_local_udp_by_endpoint(
    local: IpEndpoint,
    peer: Option<IpEndpoint>,
    buf: &mut [u8],
) -> Option<isize> {
    let mut queue = LOCAL_UDP_QUEUE.exclusive_access();
    let pos = queue
        .iter()
        .position(|packet| local_udp_packet_matches(packet, local, peer))?;
    let packet = queue.remove(pos)?;
    let n = packet.data.len().min(buf.len());
    buf[..n].copy_from_slice(&packet.data[..n]);
    Some(n as isize)
}

// 唤醒可能在 accept 本地 TCP 连接的任务。
// 执行时先从等待队列里找满足条件的任务，再把它们改回 Ready，最后交给调度器运行。
fn wake_local_tcp_acceptors() {
    let mut to_wake = Vec::new();
    {
        let mut queue = NET_BLOCK_QUEUE.exclusive_access();
        let mut i = 0;
        while i < queue.len() {
            let should_wake = {
                let inner = queue[i].inner_exclusive_access();
                matches!(inner.block_reason, Some(BlockReason::TcpAccept(_)))
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
    wake_poll_wait_tasks();
}

// 连接本地 TCP 监听端口时创建一对内核模拟连接。
// 执行时确认目标端口正在监听，分配连接 id 和客户端临时端口，再把连接放入待 accept 队列。
fn create_local_tcp(sockfd: usize, remote: IpEndpoint) -> isize {
    if !LOCAL_TCP_LISTEN_PORTS
        .exclusive_access()
        .contains(&remote.port)
    {
        return -111;
    }

    let local_port = next_ephemeral_port();
    let local = source_addr_for_dest(
        IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), local_port),
        remote,
    );
    let server = source_addr_for_dest(
        IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), remote.port),
        remote,
    );
    let conn_id = NEXT_LOCAL_TCP_ID.fetch_add(1, Ordering::Relaxed);

    LOCAL_TCP_CONNS.exclusive_access().insert(
        conn_id,
        LocalTcpConn {
            client: local,
            server,
            c2s: VecDeque::new(),
            s2c: VecDeque::new(),
            client_closed: false,
            server_closed: false,
        },
    );
    LOCAL_TCP_PENDING
        .exclusive_access()
        .entry(remote.port)
        .or_default()
        .push_back(conn_id);

    set_socket_local_endpoint(sockfd, local);
    set_socket_peer_endpoint(sockfd, server);
    set_socket_local_tcp(sockfd, conn_id, false);
    wake_local_tcp_acceptors();
    0
}

// 从监听端口队列里取出一个待 accept 的本地 TCP 连接。
// 执行时先弹出 pending 队列里的连接 id，队列空了就删除端口项，再复制连接状态返回给 accept。
fn pop_local_tcp_pending(port: u16) -> Option<(usize, LocalTcpConn)> {
    let conn_id = {
        let mut pending = LOCAL_TCP_PENDING.exclusive_access();
        let queue = pending.get_mut(&port)?;
        let conn_id = queue.pop_front()?;
        if queue.is_empty() {
            pending.remove(&port);
        }
        conn_id
    };
    let conn = LOCAL_TCP_CONNS.exclusive_access().get(&conn_id).cloned()?;
    Some((conn_id, conn))
}

// 如果监听端口有本地 TCP 连接，就直接创建 accepted fd。
// 执行时取出本地 pending 连接，为 accepted socket 分配新 fd，并按需把客户端地址写回用户态。
fn accept_local_tcp(sockfd: usize, addr: *mut u8, addrlen: *mut usize) -> Option<isize> {
    let nonblock = sock_lookup(sockfd)
        .map(|(_, _, nonblock)| nonblock)
        .unwrap_or(false);
    let local = get_socket_local_endpoint(sockfd).or_else(|| {
        let (handle, st, _) = sock_lookup(sockfd)?;
        if st != SocketType::Tcp {
            return None;
        }
        crate::net::with_net_iface(|iface| {
            let sock: &TcpSocket = iface.sockets.get(handle);
            sock.local_endpoint()
        })
        .flatten()
    })?;
    let (conn_id, conn) = pop_local_tcp_pending(local.port)?;

    let new_handle = crate::net::with_net_iface(|iface| {
        let new_sock = TcpSocket::new(
            TcpSocketBuffer::new(alloc::vec![0u8; SOCKET_BUFFER_SIZE]),
            TcpSocketBuffer::new(alloc::vec![0u8; SOCKET_BUFFER_SIZE]),
        );
        iface.sockets.add(new_sock)
    })
    .expect("NET_IFACE");

    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let new_fd = inner.alloc_fd();
    if new_fd >= inner.fd_table.len() {
        return Some(-24);
    }
    let sock = Arc::new(SocketFile::new(new_handle, SocketType::Tcp, nonblock));
    sock.set_local_endpoint(conn.server);
    sock.set_peer_endpoint(conn.client);
    sock.set_local_tcp(conn_id, true);
    inner.fd_table[new_fd] = Some(sock);
    drop(inner);

    if !addr.is_null() && !addrlen.is_null() {
        if let IpAddress::Ipv4(ipv4) = conn.client.addr {
            let token = current_user_token();
            *translated_refmut(token, addrlen as *mut u32) =
                core::mem::size_of::<SockAddrIn>() as u32;
            write_sockaddr_in(addr, u32::from_be_bytes(ipv4.0), conn.client.port);
        }
    }

    Some(new_fd as isize)
}

// 判断本地 TCP 连接的对端是否已经关闭。
// 执行时根据当前 fd 属于服务端还是客户端，读取另一端的关闭标记。
fn local_tcp_peer_closed(conn: &LocalTcpConn, server_side: bool) -> bool {
    if server_side {
        conn.client_closed
    } else {
        conn.server_closed
    }
}

// 往本地 TCP 回环连接的对端接收队列写数据。
// 执行时按当前端选择 c2s 或 s2c 队列；队列满时阻塞等待，非阻塞模式则返回 EAGAIN。
pub(crate) fn local_tcp_send(
    conn_id: usize,
    server_side: bool,
    nonblock: bool,
    data: &[u8],
) -> isize {
    if data.is_empty() {
        return 0;
    }

    let mut offset = 0usize;
    loop {
        let mut completed = false;
        {
            let mut conns = LOCAL_TCP_CONNS.exclusive_access();
            let Some(conn) = conns.get_mut(&conn_id) else {
                return if offset > 0 { offset as isize } else { -32 };
            };
            if local_tcp_peer_closed(conn, server_side) {
                return if offset > 0 { offset as isize } else { -32 };
            }
            let queue = if server_side {
                &mut conn.s2c
            } else {
                &mut conn.c2s
            };
            let free = LOCAL_TCP_BUFFER_SIZE.saturating_sub(queue.len());
            if free > 0 {
                let n = free.min(data.len() - offset);
                queue.extend(data[offset..offset + n].iter().copied());
                offset += n;
                wake_poll_wait_tasks();
                if offset == data.len() {
                    completed = true;
                }
            }
        }

        if completed {
            crate::fs::stdio::stdout_release_if_owner();
            suspend_current_and_run_next();
            return offset as isize;
        }

        if nonblock {
            return if offset > 0 { offset as isize } else { -11 };
        }
        crate::fs::stdio::stdout_release_if_owner();
        suspend_current_and_run_next();
    }
}

// 从本地 TCP 回环连接的当前端接收队列读数据。
// 执行时按当前端读取对端写入的队列；没有数据时根据对端关闭或非阻塞状态返回 0/EAGAIN。
pub(crate) fn local_tcp_recv(
    conn_id: usize,
    server_side: bool,
    nonblock: bool,
    buf: &mut [u8],
) -> isize {
    if buf.is_empty() {
        return 0;
    }

    loop {
        {
            let mut conns = LOCAL_TCP_CONNS.exclusive_access();
            let Some(conn) = conns.get_mut(&conn_id) else {
                return 0;
            };
            let peer_closed = local_tcp_peer_closed(conn, server_side);
            let queue = if server_side {
                &mut conn.c2s
            } else {
                &mut conn.s2c
            };
            if !queue.is_empty() {
                let n = buf.len().min(queue.len());
                for byte in buf.iter_mut().take(n) {
                    *byte = queue.pop_front().unwrap();
                }
                wake_poll_wait_tasks();
                return n as isize;
            }
            if peer_closed {
                return 0;
            }
        }

        if nonblock {
            return -11;
        }
        crate::fs::stdio::stdout_release_if_owner();
        suspend_current_and_run_next();
    }
}

// 关闭本地 TCP 回环连接的一端。
// 执行时标记当前端已关闭；等两端都关闭并且队列清空后，才删除连接记录。
pub(crate) fn local_tcp_close(conn_id: usize, server_side: bool) {
    let mut remove = false;
    {
        let mut conns = LOCAL_TCP_CONNS.exclusive_access();
        if let Some(conn) = conns.get_mut(&conn_id) {
            if server_side {
                conn.server_closed = true;
            } else {
                conn.client_closed = true;
            }
            remove = conn.client_closed
                && conn.server_closed
                && conn.c2s.is_empty()
                && conn.s2c.is_empty();
        }
    }
    if remove {
        LOCAL_TCP_CONNS.exclusive_access().remove(&conn_id);
    }
    wake_poll_wait_tasks();
}

// 移除本地 TCP 监听端口。
// 执行时从监听集合和 pending 队列同时删除端口，再唤醒 poll/accept 等待者重新检查状态。
pub(crate) fn local_tcp_unlisten(port: u16) {
    LOCAL_TCP_LISTEN_PORTS.exclusive_access().remove(&port);
    LOCAL_TCP_PENDING.exclusive_access().remove(&port);
    wake_poll_wait_tasks();
}

// 查询本地 TCP 回环连接的 poll 状态。
// 执行时按端角色读取入队列、出队列和对端关闭位，组合成可读、可写、异常三种状态。
pub(crate) fn local_tcp_poll(conn_id: usize, server_side: bool) -> (bool, bool, bool) {
    let conns = LOCAL_TCP_CONNS.exclusive_access();
    let Some(conn) = conns.get(&conn_id) else {
        return (true, false, true);
    };
    let (incoming_len, outgoing_len, peer_closed) = if server_side {
        (conn.c2s.len(), conn.s2c.len(), conn.client_closed)
    } else {
        (conn.s2c.len(), conn.c2s.len(), conn.server_closed)
    };
    (
        incoming_len > 0 || peer_closed,
        !peer_closed && outgoing_len < LOCAL_TCP_BUFFER_SIZE,
        peer_closed,
    )
}

// 判断指定本地 TCP 端口是否有待 accept 连接。
// 执行时只看 pending 队列是否存在且非空，用于 listen fd 的 poll 和 accept 快速路径。
pub(crate) fn local_tcp_pending_on(port: u16) -> bool {
    LOCAL_TCP_PENDING
        .exclusive_access()
        .get(&port)
        .map(|queue| !queue.is_empty())
        .unwrap_or(false)
}

// 返回本地 TCP 连接对端的地址。
// 执行时按当前端角色返回另一端 endpoint，getpeername 会把它写成 sockaddr_in。
fn local_tcp_peer_endpoint(conn_id: usize, server_side: bool) -> Option<IpEndpoint> {
    let conns = LOCAL_TCP_CONNS.exclusive_access();
    let conn = conns.get(&conn_id)?;
    if server_side {
        Some(conn.client)
    } else {
        Some(conn.server)
    }
}

// 系统调用入口：创建 TCP/UDP socket fd。
// 执行时校验 AF_INET 和 socket 类型，在 smoltcp 中分配 socket，再放入当前进程 fd 表并设置 cloexec。
pub fn sys_socket(domain: usize, type_: usize, _protocol: usize) -> isize {
    if domain != AF_INET {
        return -97;
    }
    if !net_ready() {
        return -97;
    }

    let st = match type_ & 0x0F {
        SOCK_STREAM => SocketType::Tcp,
        SOCK_DGRAM => SocketType::Udp,
        _ => return -22,
    };

    let nonblock = (type_ & SOCK_NONBLOCK) != 0;
    let cloexec = (type_ & SOCK_CLOEXEC) != 0;

    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    let (handle, st) = match add_to_socket_set(st) {
        Some(v) => v,
        None => return -105,
    };

    let fd = inner.alloc_fd();
    if fd >= inner.fd_table.len() {
        return -24;
    }
    inner.fd_table[fd] = Some(Arc::new(SocketFile::new(handle, st, nonblock)));
    inner.fd_cloexec[fd] = cloexec;
    fd as isize
}

// 系统调用入口：把 socket 绑定到本地 IPv4 地址。
// 执行时解析 sockaddr_in，端口为 0 时分配临时端口；TCP 进入 listen，UDP 调 bind，并缓存本地端点。
pub fn sys_bind(sockfd: usize, addr: *const u8, addrlen: usize) -> isize {
    if !net_ready() {
        return -97;
    }
    if addr.is_null() || addrlen < 8 {
        return -14;
    }

    let (ip, mut port) = match parse_sockaddr_in(addr) {
        Some(v) => v,
        None => return -22,
    };
    if port == 0 {
        port = next_ephemeral_port();
    }

    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let endpoint = IpEndpoint::new(
        IpAddress::from(Ipv4Address::from_bytes(&ip.to_be_bytes())),
        port,
    );

    let ret = crate::net::with_net_iface(|iface| match st {
        SocketType::Tcp => {
            let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
            sock.listen(if ip == 0 {
                IpListenEndpoint { addr: None, port }
            } else {
                IpListenEndpoint {
                    addr: Some(endpoint.addr),
                    port,
                }
            })
            .map(|_| 0)
            .unwrap_or(-22)
        }
        SocketType::Udp => {
            let sock: &mut UdpSocket = iface.sockets.get_mut(handle);
            let _ = sock.bind(if ip == 0 {
                IpListenEndpoint { addr: None, port }
            } else {
                IpListenEndpoint {
                    addr: Some(endpoint.addr),
                    port,
                }
            });
            0
        }
    })
    .unwrap_or(-1);
    if ret == 0 {
        let local_addr = if ip == 0 {
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)
        } else {
            endpoint.addr
        };
        set_socket_local_endpoint(sockfd, IpEndpoint::new(local_addr, port));
    }
    ret
}

// 系统调用入口：连接 TCP 对端或记录 UDP 默认对端。
// 执行时 UDP 只保存默认对端；TCP 本机地址走内核回环连接，外部地址交给 smoltcp connect。
pub fn sys_connect(sockfd: usize, addr: *const u8, addrlen: usize) -> isize {
    if !net_ready() {
        return -97;
    }
    if addr.is_null() || addrlen < 8 {
        return -14;
    }

    let (ip, port) = match parse_sockaddr_in(addr) {
        Some(v) => v,
        None => return -22,
    };

    let (handle, st, nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let remote = IpEndpoint::new(
        IpAddress::from(Ipv4Address::from_bytes(&ip.to_be_bytes())),
        port,
    );

    if st == SocketType::Udp {
        ensure_udp_local_endpoint(sockfd, handle, remote);
        set_socket_peer_endpoint(sockfd, remote);
        return 0;
    }

    if st != SocketType::Tcp {
        return -95;
    }

    if is_local_ipv4(remote.addr) {
        return create_local_tcp(sockfd, remote);
    }

    let local = IpListenEndpoint {
        addr: None,
        port: 32768 + ((sockfd as u16).wrapping_mul(7919) % 32767), // non-zero ephemeral port
    };

    let connect_ret = crate::net::with_net_iface_tcp_connect(handle, remote, local);
    if connect_ret < 0 {
        return connect_ret;
    }
    crate::net::net_poll();

    if !nonblock {
        loop {
            crate::net::net_poll();
            let connected = crate::net::with_net_iface(|iface| {
                let sock: &TcpSocket = iface.sockets.get(handle);
                sock.is_active()
            })
            .unwrap_or(false);
            if connected {
                return 0;
            }
            block_current_and_run_next(BlockReason::TcpConnect(unsafe {
                core::mem::transmute(handle)
            }));
        }
    }

    let connected = crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.is_active()
    })
    .unwrap_or(false);
    if connected {
        0
    } else {
        -115
    } // EINPROGRESS
}

// 系统调用入口：把 TCP socket 切到监听状态。
// 执行时确认 fd 是 TCP 并且 smoltcp 已处于 listening，再把本地端口登记到本地回环监听表。
pub fn sys_listen(sockfd: usize, _backlog: usize) -> isize {
    if !net_ready() {
        return -97;
    }
    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };
    if st != SocketType::Tcp {
        return -95;
    }

    {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        if sockfd < inner.fd_table.len() {
            if let Some(file) = &inner.fd_table[sockfd] {
                if let Some(s) = file.as_any().downcast_ref::<SocketFile>() {
                    s.was_listening
                        .store(true, core::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    let listening = crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.is_listening()
    })
    .unwrap_or(false);

    if !listening {
        return -22;
    }
    if let Some(local) = get_socket_local_endpoint(sockfd) {
        LOCAL_TCP_LISTEN_PORTS.exclusive_access().insert(local.port);
    }
    0
}

// 系统调用入口：接收一个 TCP 连接，优先处理本地回环连接。
// 执行时先尝试弹出本地 pending 连接；没有时轮询 smoltcp，阻塞模式会挂到 TcpAccept 等待队列。
pub fn sys_accept(sockfd: usize, addr: *mut u8, addrlen: *mut usize) -> isize {
    if !net_ready() {
        return -97;
    }
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();

    if sockfd >= inner.fd_table.len() {
        return -9;
    }
    let sock = match inner.fd_table[sockfd].as_ref() {
        Some(file) => match file.as_any().downcast_ref::<SocketFile>() {
            Some(s) => s,
            None => return -95,
        },
        None => return -9,
    };
    if sock.socket_type != SocketType::Tcp {
        return -95;
    }
    let handle = sock.get_handle();
    let nonblock = sock.get_nonblock();
    drop(inner);

    if let Some(ret) = accept_local_tcp(sockfd, addr, addrlen) {
        return ret;
    }

    crate::net::net_poll();

    if !nonblock {
        let mut prev_listening = true;
        loop {
            if let Some(ret) = accept_local_tcp(sockfd, addr, addrlen) {
                return ret;
            }
            crate::net::net_poll();
            crate::net::net_poll();
            let active = crate::net::with_net_iface(|iface| {
                let sock: &TcpSocket = iface.sockets.get(handle);
                let listening = sock.is_listening();
                let active = sock.is_active();
                if !listening && prev_listening && active {
                    return true;
                }
                if listening {
                    prev_listening = true;
                }
                false
            })
            .unwrap_or(false);

            if active {
                break;
            }
            block_current_and_run_next(BlockReason::TcpAccept(unsafe {
                core::mem::transmute(handle)
            }));
        }
    }

    let active = crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.is_active()
    })
    .unwrap_or(false);

    if !active {
        return -11;
    }

    let old_local = crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.local_endpoint()
    })
    .flatten();

    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    let new_handle = crate::net::with_net_iface(|iface| {
        let new_sock = TcpSocket::new(
            TcpSocketBuffer::new(alloc::vec![0u8; SOCKET_BUFFER_SIZE]),
            TcpSocketBuffer::new(alloc::vec![0u8; SOCKET_BUFFER_SIZE]),
        );
        let h = iface.sockets.add(new_sock);
        if let Some(local_ep) = &old_local {
            let listen_endpoint = IpListenEndpoint {
                addr: None,
                port: local_ep.port,
            };
            let sock: &mut TcpSocket = iface.sockets.get_mut(h);
            let _ = sock.listen(listen_endpoint);
        }
        h
    })
    .expect("NET_IFACE");

    // 接收成功后，监听 fd 换成新的监听 socket，旧的 active handle 交给 accepted fd。
    if sockfd < inner.fd_table.len() {
        if let Some(file) = &inner.fd_table[sockfd] {
            if let Some(s) = file.as_any().downcast_ref::<SocketFile>() {
                s.set_handle(new_handle);
            }
        }
    }

    // 新 fd 接管已经建立连接的旧 handle。
    let new_fd = inner.alloc_fd();
    if new_fd >= inner.fd_table.len() {
        return -24;
    }
    inner.fd_table[new_fd] = Some(Arc::new(SocketFile::new(handle, SocketType::Tcp, nonblock)));

    // 写用户地址前先放掉进程锁，避免网络轮询或地址翻译时持锁过久。
    drop(inner);

    // 再轮询两次，尽量把客户端已经发出的首批数据送到 accepted socket。
    crate::net::net_poll();
    crate::net::net_poll();

    if !addr.is_null() && !addrlen.is_null() {
        let token = current_user_token();
        let peer = crate::net::with_net_iface(|iface| {
            let sock: &TcpSocket = iface.sockets.get(handle);
            sock.remote_endpoint()
        })
        .flatten();
        if let Some(peer) = peer {
            if let IpAddress::Ipv4(ipv4) = peer.addr {
                *translated_refmut(token, addrlen as *mut usize) =
                    core::mem::size_of::<SockAddrIn>();
                write_sockaddr_in(addr, u32::from_be_bytes(ipv4.0), peer.port);
            }
        }
    }

    new_fd as isize
}

// 系统调用入口：accept 后按 flags 设置新 fd 属性。
// 执行时复用 sys_accept 创建连接，再按 SOCK_CLOEXEC 和 SOCK_NONBLOCK 修改新 fd 状态。
pub fn sys_accept4(sockfd: usize, addr: *mut u8, addrlen: *mut usize, flags: usize) -> isize {
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return -22;
    }

    let fd = sys_accept(sockfd, addr, addrlen);
    if fd < 0 {
        return fd;
    }

    let fd = fd as usize;
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    if (flags & SOCK_CLOEXEC) != 0 {
        inner.fd_cloexec[fd] = true;
    }
    if (flags & SOCK_NONBLOCK) != 0 {
        if let Some(file) = &inner.fd_table[fd] {
            let current_flags = file.get_flags();
            file.set_flags(current_flags | SOCK_NONBLOCK);
        }
    }
    fd as isize
}

// 系统调用入口：从用户缓冲区取数据并发送到指定或默认 UDP/TCP 对端。
// 执行时根据 MSG_DONTWAIT 合并非阻塞语义；TCP 用连接状态发送，UDP 解析目标地址并优先走本地队列。
pub fn sys_sendto(
    sockfd: usize,
    buf: *const u8,
    len: usize,
    flags: usize,
    dest_addr: *const u8,
    addrlen: usize,
) -> isize {
    if !net_ready() {
        return -97;
    }
    let (handle, st, nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let dontwait = nonblock || (flags & MSG_DONTWAIT) != 0;

    // UDP 的 sendto 必须带目标地址，否则没有默认目的地。
    if st == SocketType::Udp && (dest_addr.is_null() || addrlen < 8) {
        return -89; // EDESTADDRREQ
    }
    // TCP 忽略 dest_addr，直接使用已经建立的连接。

    let data = unsafe { core::slice::from_raw_parts(buf, len) };

    match st {
        SocketType::Tcp => {
            if let Some((conn_id, server_side)) = get_socket_local_tcp(sockfd) {
                local_tcp_send(conn_id, server_side, dontwait, data)
            } else {
                tcp_send(handle, dontwait, data)
            }
        }
        SocketType::Udp => {
            let (ip, port) = match parse_sockaddr_in(dest_addr) {
                Some(v) => v,
                None => return -22,
            };
            let dest = IpEndpoint::new(
                IpAddress::from(Ipv4Address::from_bytes(&ip.to_be_bytes())),
                port,
            );
            if let Some(ret) = try_send_local_udp(sockfd, handle, data, dest) {
                return ret;
            }
            udp_send(handle, data, dest)
        }
    }
}

// 系统调用入口：接收 socket 数据，并可选写回来源地址。
// 执行时 TCP 直接按连接收；UDP 先查本地队列，再查 smoltcp，阻塞模式会挂到 UdpRecv 等待队列。
pub fn sys_recvfrom(
    sockfd: usize,
    buf: *mut u8,
    len: usize,
    flags: usize,
    src_addr: *mut u8,
    addrlen: *mut usize,
) -> isize {
    if !net_ready() {
        return -97;
    }
    let (handle, st, nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let dontwait = nonblock || (flags & MSG_DONTWAIT) != 0;
    let peek = (flags & MSG_PEEK) != 0;

    let recv_buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };

    let result = match st {
        SocketType::Tcp => {
            if let Some((conn_id, server_side)) = get_socket_local_tcp(sockfd) {
                local_tcp_recv(conn_id, server_side, dontwait, recv_buf)
            } else {
                tcp_recv(handle, dontwait, recv_buf)
            }
        }
        SocketType::Udp => loop {
            if let Some(ret) = try_recv_local_udp(sockfd, handle, recv_buf, src_addr, addrlen, peek)
            {
                return ret;
            }
            let ret = udp_recv(handle, true, recv_buf);
            if ret != -11 || dontwait {
                break ret;
            }
            block_current_and_run_next(BlockReason::UdpRecv(unsafe {
                core::mem::transmute(handle)
            }));
        },
    };

    // UDP 调用者如果要求来源地址，就把当前 endpoint 写回用户 sockaddr。
    if result > 0 && !src_addr.is_null() && !addrlen.is_null() && st == SocketType::Udp && !peek {
        let token = current_user_token();
        let endpoint = crate::net::with_net_iface(|iface| {
            let sock: &UdpSocket = iface.sockets.get(handle);
            sock.endpoint()
        });
        if let Some(ep) = endpoint {
            if let Some(addr) = ep.addr {
                if let IpAddress::Ipv4(ipv4) = addr {
                    *translated_refmut(token, addrlen as *mut u32) =
                        core::mem::size_of::<SockAddrIn>() as u32;
                    write_sockaddr_in(src_addr, u32::from_be_bytes(ipv4.0), ep.port);
                }
            }
        }
    }

    result
}

// 系统调用入口：按 msghdr/iovec 发送 socket 消息。
// 执行时先把用户 iovec 聚合到内核缓冲区，再复用 TCP/UDP 发送路径处理默认或显式对端。
pub fn sys_sendmsg(sockfd: usize, msg: *const u8, flags: usize) -> isize {
    if !net_ready() {
        return -97;
    }
    let token = current_user_token();
    let msghdr: &Msghdr = translated_ref(token, msg as *const Msghdr);

    let (handle, st, nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let dontwait = nonblock || (flags & MSG_DONTWAIT) != 0;

    let iov = msghdr.msg_iov as *const Iovec;
    let iovcnt = msghdr.msg_iovlen;
    let total = iov_total_len(iov, iovcnt);

    if total == 0 {
        return 0;
    }

    let mut buf = alloc::vec![0u8; total.min(SOCKET_BUFFER_SIZE)];
    let copied = copy_from_iov(&mut buf, iov, iovcnt);

    match st {
        SocketType::Tcp => {
            if let Some((conn_id, server_side)) = get_socket_local_tcp(sockfd) {
                local_tcp_send(conn_id, server_side, dontwait, &buf[..copied])
            } else {
                tcp_send(handle, dontwait, &buf[..copied])
            }
        }
        SocketType::Udp => {
            let dest = if !msghdr.msg_name.is_null() && msghdr.msg_namelen as usize >= 8 {
                match parse_sockaddr_in(msghdr.msg_name) {
                    Some((ip, port)) => Some(IpEndpoint::new(
                        IpAddress::from(Ipv4Address::from_bytes(&ip.to_be_bytes())),
                        port,
                    )),
                    None => return -22,
                }
            } else {
                get_socket_peer_endpoint(sockfd)
            };
            let dest = match dest {
                Some(d) => d,
                None => return -89,
            };
            if let Some(ret) = try_send_local_udp(sockfd, handle, &buf[..copied], dest) {
                return ret;
            }
            udp_send(handle, &buf[..copied], dest)
        }
    }
}

// 系统调用入口：按 msghdr/iovec 接收 socket 消息。
// 执行时接收到连续内核缓冲区后再分散写回 iovec；UDP 本地队列会顺手写回 msg_name。
pub fn sys_recvmsg(sockfd: usize, msg: *mut u8, flags: usize) -> isize {
    if !net_ready() {
        return -97;
    }
    let token = current_user_token();
    let msghdr: &Msghdr = translated_ref(token, msg as *const Msghdr);

    let (handle, st, nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let dontwait = nonblock || (flags & MSG_DONTWAIT) != 0;
    let peek = (flags & MSG_PEEK) != 0;

    let iov = msghdr.msg_iov;
    let iovcnt = msghdr.msg_iovlen;
    let total = iov_total_len(iov as *const Iovec, iovcnt);

    if total == 0 {
        return 0;
    }

    let mut buf = alloc::vec![0u8; total.min(SOCKET_BUFFER_SIZE)];

    let result = match st {
        SocketType::Tcp => {
            if let Some((conn_id, server_side)) = get_socket_local_tcp(sockfd) {
                local_tcp_recv(conn_id, server_side, dontwait, &mut buf)
            } else {
                tcp_recv(handle, dontwait, &mut buf)
            }
        }
        SocketType::Udp => {
            if let Some(local) = local_recv_endpoint(sockfd, handle) {
                let peer = get_socket_peer_endpoint(sockfd);
                let mut queue = LOCAL_UDP_QUEUE.exclusive_access();
                if let Some(pos) = queue
                    .iter()
                    .position(|packet| local_udp_packet_matches(packet, local, peer))
                {
                    let packet = if peek {
                        queue[pos].clone()
                    } else {
                        queue.remove(pos).unwrap()
                    };
                    let n = packet.data.len().min(buf.len());
                    buf[..n].copy_from_slice(&packet.data[..n]);
                    drop(queue);

                    copy_to_iov(&buf[..n], iov, iovcnt);
                    let msghdr_mut: &mut Msghdr = translated_refmut(token, msg as *mut Msghdr);
                    msghdr_mut.msg_flags = 0;
                    if !msghdr.msg_name.is_null() {
                        let max = core::mem::size_of::<SockAddrIn>();
                        let copy = max.min(msghdr.msg_namelen as usize);
                        msghdr_mut.msg_namelen = copy as u32;
                        let IpAddress::Ipv4(ipv4) = packet.src.addr;
                        write_sockaddr_in(
                            msghdr.msg_name,
                            u32::from_be_bytes(ipv4.0),
                            packet.src.port,
                        );
                    }
                    return n as isize;
                }
            }
            udp_recv(handle, dontwait, &mut buf)
        }
    };

    if result > 0 {
        let n = result as usize;
        copy_to_iov(&buf[..n], iov, iovcnt);

        // 当前没有额外消息标志，按 Linux 习惯清零返回。
        let msghdr_mut: &mut Msghdr = translated_refmut(token, msg as *mut Msghdr);
        msghdr_mut.msg_flags = 0;

        // UDP 如果调用者提供 msg_name，就写回来源地址。
        if st == SocketType::Udp && !peek {
            let endpoint = crate::net::with_net_iface(|iface| {
                let sock: &UdpSocket = iface.sockets.get(handle);
                sock.endpoint()
            });
            if let Some(ep) = endpoint {
                if let Some(addr) = ep.addr {
                    if let IpAddress::Ipv4(ipv4) = addr {
                        if !msghdr.msg_name.is_null() {
                            let max = core::mem::size_of::<SockAddrIn>();
                            let copy = max.min(msghdr.msg_namelen as usize);
                            msghdr_mut.msg_namelen = copy as u32;
                            write_sockaddr_in(msghdr.msg_name, u32::from_be_bytes(ipv4.0), ep.port);
                        }
                    }
                }
            }
        }
    }

    result
}

// 系统调用入口：处理常见 socket 选项，未实现的选项尽量兼容返回成功。
// 执行时只真正处理 keepalive 和 TCP_NODELAY，其余常见选项接受但不改变状态，方便 libc 探测通过。
pub fn sys_setsockopt(
    sockfd: usize,
    level: usize,
    optname: usize,
    optval: *const u8,
    optlen: usize,
) -> isize {
    if !net_ready() {
        return -97;
    }
    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    match level {
        SOL_SOCKET => {
            match optname {
                SO_REUSEADDR => 0,          // no-op: always allowed
                SO_RCVBUF | SO_SNDBUF => 0, // no-op: accept but ignore buffer size
                SO_KEEPALIVE => {
                    if st == SocketType::Tcp {
                        let val = if optlen >= 4 && !optval.is_null() {
                            let token = current_user_token();
                            let v: &i32 = translated_ref(token, optval as *const i32);
                            *v != 0
                        } else {
                            false
                        };
                        crate::net::with_net_iface(|iface| {
                            let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
                            if val {
                                sock.set_keep_alive(Some(smoltcp::time::Duration::from_secs(7200)));
                            } else {
                                sock.set_keep_alive(None);
                            }
                        });
                    }
                    0
                }
                _ => 0, // silently ignore unknown options
            }
        }
        IPPROTO_TCP => match optname {
            TCP_NODELAY => {
                if st != SocketType::Tcp {
                    return -95;
                }
                let val = if optlen >= 4 && !optval.is_null() {
                    let token = current_user_token();
                    let v: &i32 = translated_ref(token, optval as *const i32);
                    *v != 0
                } else {
                    false
                };
                crate::net::with_net_iface(|iface| {
                    let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
                    sock.set_nagle_enabled(!val);
                });
                0
            }
            _ => 0,
        },
        _ => 0, // silently ignore unknown levels
    }
}

// 系统调用入口：读取 socket 选项，覆盖 libc 常查的几类。
// 执行时按 level/optname 写回整数、拥塞算法名或简化 tcp_info，不支持的选项返回 ENOPROTOOPT。
pub fn sys_getsockopt(
    sockfd: usize,
    level: usize,
    optname: usize,
    optval: *mut u8,
    optlen: *mut usize,
) -> isize {
    if !net_ready() {
        return -97;
    }
    if optval.is_null() || optlen.is_null() {
        return -14;
    }

    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let token = current_user_token();

    // 把字节形式的 socket 选项写回用户态。
    // 执行时先尊重用户传入的 optlen 上限，再通过用户页表写入实际字节数。
    fn write_sockopt_bytes(token: PageTable, optval: *mut u8, optlen: *mut usize, data: &[u8]) {
        let requested = *translated_ref(token, optlen as *const u32) as usize;
        let n = requested.min(data.len());
        translated_byte_buffer(token, optval, n).copy_from_slice(&data[..n]);
        *translated_refmut(token, optlen as *mut u32) = n as u32;
    }

    // 把 i32 形式的 socket 选项写回用户态。
    // 执行时把整数转成本机字节序，再交给字节写回函数统一处理 optlen。
    fn write_sockopt_i32(token: PageTable, optval: *mut u8, optlen: *mut usize, value: i32) {
        write_sockopt_bytes(token, optval, optlen, &value.to_ne_bytes());
    }

    // 按小端写一个 u32 到临时缓冲区。
    // 执行时先检查偏移没有越界，再写入本机字节序的 4 字节字段。
    fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
        if offset + 4 <= buf.len() {
            buf[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
        }
    }

    // 按小端写一个 u64 到临时缓冲区。
    // 执行时先检查偏移没有越界，再写入本机字节序的 8 字节字段。
    fn put_u64(buf: &mut [u8], offset: usize, value: u64) {
        if offset + 8 <= buf.len() {
            buf[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
        }
    }

    // 写回一份简化版 tcp_info，满足用户态探测。
    // 执行时填入稳定的 MSS、RTT、窗口等字段，不追求精确统计，只保证常见程序读到合理结构。
    fn write_tcp_info(token: PageTable, optval: *mut u8, optlen: *mut usize) {
        let mut info = [0u8; 232];
        info[0] = 1; // TCP_ESTABLISHED
        put_u32(&mut info, 8, 200_000); // tcpi_rto, usec
        put_u32(&mut info, 16, 1460); // tcpi_snd_mss
        put_u32(&mut info, 20, 1460); // tcpi_rcv_mss
        put_u32(&mut info, 60, 1500); // tcpi_pmtu
        put_u32(&mut info, 64, LOCAL_TCP_BUFFER_SIZE as u32); // tcpi_rcv_ssthresh
        put_u32(&mut info, 68, 1000); // tcpi_rtt, usec
        put_u32(&mut info, 72, 100); // tcpi_rttvar
        put_u32(&mut info, 76, u32::MAX); // tcpi_snd_ssthresh
        put_u32(&mut info, 80, 10); // tcpi_snd_cwnd
        put_u32(&mut info, 84, 1460); // tcpi_advmss
        put_u32(&mut info, 88, 3); // tcpi_reordering
        put_u32(&mut info, 96, LOCAL_TCP_BUFFER_SIZE as u32); // tcpi_rcv_space
        put_u64(&mut info, 104, u64::MAX); // tcpi_pacing_rate
        put_u64(&mut info, 112, u64::MAX); // tcpi_max_pacing_rate
        put_u32(&mut info, 220, LOCAL_TCP_BUFFER_SIZE as u32); // tcpi_snd_wnd
        put_u32(&mut info, 224, LOCAL_TCP_BUFFER_SIZE as u32); // tcpi_rcv_wnd
        write_sockopt_bytes(token, optval, optlen, &info);
    }

    match (level, optname) {
        (SOL_SOCKET, SO_REUSEADDR) | (SOL_SOCKET, SO_KEEPALIVE) => {
            write_sockopt_i32(token, optval, optlen, 1);
            0
        }
        (SOL_SOCKET, SO_ERROR) => {
            write_sockopt_i32(token, optval, optlen, 0);
            0
        }
        (SOL_SOCKET, SO_RCVBUF) | (SOL_SOCKET, SO_SNDBUF) => {
            write_sockopt_i32(token, optval, optlen, SOCKET_BUFFER_SIZE as i32);
            0
        }
        (IPPROTO_TCP, TCP_NODELAY) => {
            if st != SocketType::Tcp {
                return -95;
            }
            let nagle = crate::net::with_net_iface(|iface| {
                let sock: &TcpSocket = iface.sockets.get(handle);
                !sock.nagle_enabled()
            })
            .unwrap_or(false);
            write_sockopt_i32(token, optval, optlen, if nagle { 1 } else { 0 });
            0
        }
        (IPPROTO_TCP, TCP_MAXSEG) => {
            if st != SocketType::Tcp {
                return -95;
            }
            write_sockopt_i32(token, optval, optlen, 1460);
            0
        }
        (IPPROTO_TCP, TCP_INFO) => {
            if st != SocketType::Tcp {
                return -95;
            }
            write_tcp_info(token, optval, optlen);
            0
        }
        (IPPROTO_TCP, TCP_CONGESTION) => {
            if st != SocketType::Tcp {
                return -95;
            }
            write_sockopt_bytes(token, optval, optlen, b"reno\0");
            0
        }
        _ => -92, // ENOPROTOOPT
    }
}

// 系统调用入口：关闭 socket 的读端、写端或两端。
// 执行时本地 TCP 直接标记一端关闭；普通 smoltcp TCP 调 close，非法 how 返回 EINVAL。
pub fn sys_sock_shutdown(sockfd: usize, how: usize) -> isize {
    if !net_ready() {
        return -97;
    }
    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    if st != SocketType::Tcp {
        return -95;
    }

    if let Some((conn_id, server_side)) = get_socket_local_tcp(sockfd) {
        match how {
            SHUT_RD | SHUT_WR | SHUT_RDWR => {
                local_tcp_close(conn_id, server_side);
                return 0;
            }
            _ => return -22,
        }
    }

    crate::net::with_net_iface(|iface| {
        let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
        match how {
            SHUT_RD | SHUT_WR | SHUT_RDWR => {
                sock.close();
            }
            _ => return -22,
        }
        0
    })
    .unwrap_or(-1)
}

// 系统调用入口：读取 socket 对端地址。
// 执行时优先读取本地 TCP 回环记录，否则从 smoltcp 或 UDP 缓存取 peer endpoint，再写回 sockaddr_in。
pub fn sys_getpeername(sockfd: usize, addr: *mut u8, addrlen: *mut usize) -> isize {
    if !net_ready() {
        return -97;
    }
    if addr.is_null() || addrlen.is_null() {
        return -14;
    }

    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let peer: Option<IpEndpoint> = match st {
        SocketType::Tcp => get_socket_local_tcp(sockfd)
            .and_then(|(conn_id, server_side)| local_tcp_peer_endpoint(conn_id, server_side))
            .or_else(|| {
                crate::net::with_net_iface(|iface| {
                    let sock: &TcpSocket = iface.sockets.get(handle);
                    sock.remote_endpoint()
                })
                .flatten()
            }),
        SocketType::Udp => get_socket_peer_endpoint(sockfd),
    };

    match peer {
        Some(ep) => {
            if let IpAddress::Ipv4(ipv4) = ep.addr {
                let token = current_user_token();
                *translated_refmut(token, addrlen as *mut u32) =
                    core::mem::size_of::<SockAddrIn>() as u32;
                write_sockaddr_in(addr, u32::from_be_bytes(ipv4.0), ep.port);
                0
            } else {
                -97
            }
        }
        None => -107, // ENOTCONN
    }
}

// 系统调用入口：读取 socket 本地地址。
// 执行时 TCP 从 smoltcp 或缓存取本地端点；UDP 还会根据默认对端修正 0.0.0.0 的源地址。
pub fn sys_getsockname(sockfd: usize, addr: *mut u8, addrlen: *mut usize) -> isize {
    if !net_ready() {
        return -97;
    }
    if addr.is_null() || addrlen.is_null() {
        return -14;
    }

    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let local: Option<IpEndpoint> = match st {
        SocketType::Tcp => crate::net::with_net_iface(|iface| {
            let sock: &TcpSocket = iface.sockets.get(handle);
            sock.local_endpoint()
        })
        .flatten()
        .or_else(|| get_socket_local_endpoint(sockfd)),
        SocketType::Udp => {
            let local = get_socket_local_endpoint(sockfd).or_else(|| {
                crate::net::with_net_iface(|iface| {
                    let sock: &UdpSocket = iface.sockets.get(handle);
                    sock.endpoint()
                })
                .map(|ep| {
                    IpEndpoint::new(
                        ep.addr.unwrap_or(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)),
                        ep.port,
                    )
                })
            });
            match (local, get_socket_peer_endpoint(sockfd)) {
                (Some(local), Some(peer)) => Some(source_addr_for_dest(local, peer)),
                (local, _) => local,
            }
        }
    };

    match local {
        Some(ep) => {
            if let IpAddress::Ipv4(ipv4) = ep.addr {
                let token = current_user_token();
                *translated_refmut(token, addrlen as *mut u32) =
                    core::mem::size_of::<SockAddrIn>() as u32;
                write_sockaddr_in(addr, u32::from_be_bytes(ipv4.0), ep.port);
                0
            } else {
                -97
            }
        }
        None => -22, // EINVAL: not bound
    }
}

// ---- Internal helpers ----

// 通过 smoltcp TCP socket 发送数据，必要时阻塞等待可写。
// 执行时非阻塞路径只尝试一次；阻塞路径循环轮询协议栈，直到全部写入或连接断开。
pub(crate) fn tcp_send(handle: SocketHandle, nonblock: bool, data: &[u8]) -> isize {
    if data.is_empty() {
        return 0;
    }
    if nonblock {
        crate::net::net_poll();
        let result = crate::net::with_net_iface(|iface| {
            let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
            if !sock.is_active() {
                return -32;
            }
            match sock.send_slice(data) {
                Ok(0) => -11,
                Ok(n) => n as isize,
                Err(_) => -32,
            }
        })
        .unwrap_or(-1);
        crate::net::net_poll();
        suspend_current_and_run_next();
        return result;
    }

    let mut offset = 0usize;
    while offset < data.len() {
        if !sock_is_active_tcp(handle) {
            return -32;
        }
        crate::net::net_poll();
        let sent = crate::net::with_net_iface(|iface| {
            let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
            match sock.send_slice(&data[offset..]) {
                Ok(n) => n as isize,
                Err(_) => -32,
            }
        })
        .unwrap_or(-1);
        if sent > 0 {
            offset += sent as usize;
        } else if sent == 0 {
            if !sock_can_send_tcp(handle) {
                suspend_current_and_run_next();
            }
        } else {
            return sent;
        }
    }
    crate::net::net_poll();
    suspend_current_and_run_next();
    offset as isize
}

// 通过 smoltcp TCP socket 接收数据，必要时阻塞等待可读。
// 执行时非阻塞路径无数据返回 EAGAIN；阻塞路径轮询到有数据、EOF 或 socket 消失为止。
pub(crate) fn tcp_recv(handle: SocketHandle, nonblock: bool, buf: &mut [u8]) -> isize {
    if buf.is_empty() {
        return 0;
    }
    if nonblock {
        crate::net::net_poll();
        return crate::net::with_net_iface(|iface| {
            let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
            if !sock.is_active() && sock.recv_queue() == 0 {
                return 0;
            }
            match sock.recv_slice(buf) {
                Ok(0) if !sock.may_recv() => 0,
                Ok(0) => -11,
                Ok(n) => n as isize,
                Err(_) => -11,
            }
        })
        .unwrap_or(-1);
    }

    loop {
        crate::net::net_poll();
        let result = crate::net::with_net_iface(|iface| {
            let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
            match sock.recv_slice(buf) {
                Ok(0) if !sock.may_recv() => Ok(0),
                Ok(0) => Err(()),
                Ok(n) => Ok(n),
                Err(_) => Err(()),
            }
        });
        match result {
            Some(Ok(n)) => return n as isize,
            Some(Err(())) => {
                if !sock_is_active_tcp(handle) && sock_recv_queue_tcp(handle) == 0 {
                    return 0;
                }
                suspend_current_and_run_next();
            }
            None => return -1,
        }
    }
}

// 向指定 UDP 端点发送一段数据。
// 执行时先轮询协议栈刷新 socket 状态，再调用 smoltcp 的 send_slice，失败按 EAGAIN 处理。
pub(crate) fn udp_send(handle: SocketHandle, data: &[u8], dest: IpEndpoint) -> isize {
    crate::net::net_poll();
    crate::net::with_net_iface(|iface| {
        let sock: &mut UdpSocket = iface.sockets.get_mut(handle);
        match sock.send_slice(data, dest) {
            Ok(()) => data.len() as isize,
            Err(_) => -11,
        }
    })
    .unwrap_or(-1)
}

// UDP write 路径，带上记录好的本地端点和默认对端。
// 执行时如果目标是本机地址就投递到本地队列，否则交给 smoltcp；没有默认对端返回 EDESTADDRREQ。
pub(crate) fn udp_write_with_endpoint(
    handle: SocketHandle,
    data: &[u8],
    local: Option<IpEndpoint>,
    peer: Option<IpEndpoint>,
) -> isize {
    crate::net::net_poll();
    match peer {
        Some(dest) => {
            if is_local_ipv4(dest.addr) {
                if let Some(local) = local.or_else(|| smoltcp_udp_endpoint(handle)) {
                    let src = source_addr_for_dest(local, dest);
                    return enqueue_local_udp(src, dest, data);
                }
            }
            crate::net::with_net_iface(|iface| {
                let sock: &mut UdpSocket = iface.sockets.get_mut(handle);
                match sock.send_slice(data, dest) {
                    Ok(()) => data.len() as isize,
                    Err(_) => -11,
                }
            })
            .unwrap_or(-1)
        }
        None => -89, // EDESTADDRREQ
    }
}

// UDP write 的简化入口，只依赖默认对端。
// 执行时先从 smoltcp 取当前本地端点，再转给完整的 UDP write 路径。
pub(crate) fn udp_write(handle: SocketHandle, data: &[u8], peer: Option<IpEndpoint>) -> isize {
    udp_write_with_endpoint(handle, data, smoltcp_udp_endpoint(handle), peer)
}

// UDP read 路径，优先从本地队列收包，再走 smoltcp。
// 执行时先按 local/peer 匹配本地包；没有本地包时才进入真实网卡接收路径。
pub(crate) fn udp_recv_with_endpoints(
    handle: SocketHandle,
    nonblock: bool,
    buf: &mut [u8],
    local: Option<IpEndpoint>,
    peer: Option<IpEndpoint>,
) -> isize {
    if let Some(local) = local.or_else(|| smoltcp_udp_endpoint(handle)) {
        if let Some(ret) = try_recv_local_udp_by_endpoint(local, peer, buf) {
            return ret;
        }
    }
    udp_recv_smoltcp(handle, nonblock, buf)
}

// UDP read 的简化入口。
// 执行时取出当前 UDP socket 的绑定端点，并按未 connect 的 UDP 语义接收任意来源。
pub(crate) fn udp_recv(handle: SocketHandle, nonblock: bool, buf: &mut [u8]) -> isize {
    let local = smoltcp_udp_endpoint(handle);
    udp_recv_with_endpoints(handle, nonblock, buf, local, None)
}

// 从 smoltcp UDP socket 接收真实网卡数据。
// 执行时非阻塞只试一次；阻塞模式反复轮询，没包时把任务挂到 UdpRecv 等待队列。
fn udp_recv_smoltcp(handle: SocketHandle, nonblock: bool, buf: &mut [u8]) -> isize {
    if nonblock {
        crate::net::net_poll();
        return crate::net::with_net_iface(|iface| {
            let sock: &mut UdpSocket = iface.sockets.get_mut(handle);
            match sock.recv_slice(buf) {
                Ok((n, _)) => n as isize,
                Err(_) => -11,
            }
        })
        .unwrap_or(-1);
    }

    loop {
        crate::net::net_poll();
        crate::net::net_poll();
        crate::net::net_poll();
        let result = crate::net::with_net_iface(|iface| {
            let sock: &mut UdpSocket = iface.sockets.get_mut(handle);
            match sock.recv_slice(buf) {
                Ok((n, _)) => Some(n as isize),
                Err(_) => None,
            }
        })
        .flatten();
        match result {
            Some(n) => return n,
            None => block_current_and_run_next(BlockReason::UdpRecv(unsafe {
                core::mem::transmute(handle)
            })),
        }
    }
}

// 查询 smoltcp TCP socket 是否处于 active 状态。
// 执行时进入 SocketSet 找到 TCP socket，读取 active 状态；网络状态不存在时按断开处理。
fn sock_is_active_tcp(handle: SocketHandle) -> bool {
    crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.is_active()
    })
    .unwrap_or(false)
}

// 查询 smoltcp TCP socket 是否可发送。
// 执行时读取 smoltcp 的 can_send 标记，用来决定阻塞发送是否需要让出 CPU。
fn sock_can_send_tcp(handle: SocketHandle) -> bool {
    crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.can_send()
    })
    .unwrap_or(false)
}

// 查询 smoltcp TCP socket 的接收队列长度。
// 执行时读取 recv_queue 字节数，连接不活跃时用它判断是否还有残留数据可读。
fn sock_recv_queue_tcp(handle: SocketHandle) -> usize {
    crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.recv_queue()
    })
    .unwrap_or(0)
}
