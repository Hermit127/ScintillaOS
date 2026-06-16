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

fn net_ready() -> bool {
    crate::net::net_available_fast()
}

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

fn iov_total_len(iov: *const Iovec, iovcnt: usize) -> usize {
    let iovs = unsafe { core::slice::from_raw_parts(iov, iovcnt) };
    let mut total = 0usize;
    for i in iovs {
        total = total.saturating_add(i.iov_len);
    }
    total
}

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

fn is_local_ipv4(addr: IpAddress) -> bool {
    let IpAddress::Ipv4(ipv4) = addr;
    ipv4.0[0] == 127 || ipv4.0 == [0, 0, 0, 0] || ipv4.0 == [10, 0, 2, 15]
}

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

fn local_udp_packet_matches(
    packet: &LocalUdpPacket,
    local: IpEndpoint,
    peer: Option<IpEndpoint>,
) -> bool {
    packet.dst_port == local.port && peer.map(|peer| packet.src == peer).unwrap_or(true)
}

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

pub(crate) fn local_udp_pending_on(port: u16) -> bool {
    LOCAL_UDP_QUEUE
        .exclusive_access()
        .iter()
        .any(|packet| packet.dst_port == port)
}

pub(crate) fn local_udp_close(local: IpEndpoint, peer: Option<IpEndpoint>) {
    LOCAL_UDP_QUEUE
        .exclusive_access()
        .retain(|packet| !local_udp_packet_matches(packet, local, peer));
}

fn local_recv_endpoint(sockfd: usize, handle: SocketHandle) -> Option<IpEndpoint> {
    get_socket_local_endpoint(sockfd).or_else(|| {
        let ep = smoltcp_udp_endpoint(handle)?;
        set_socket_local_endpoint(sockfd, ep);
        Some(ep)
    })
}

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

fn local_tcp_peer_closed(conn: &LocalTcpConn, server_side: bool) -> bool {
    if server_side {
        conn.client_closed
    } else {
        conn.server_closed
    }
}

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

pub(crate) fn local_tcp_unlisten(port: u16) {
    LOCAL_TCP_LISTEN_PORTS.exclusive_access().remove(&port);
    LOCAL_TCP_PENDING.exclusive_access().remove(&port);
    wake_poll_wait_tasks();
}

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

pub(crate) fn local_tcp_pending_on(port: u16) -> bool {
    LOCAL_TCP_PENDING
        .exclusive_access()
        .get(&port)
        .map(|queue| !queue.is_empty())
        .unwrap_or(false)
}

fn local_tcp_peer_endpoint(conn_id: usize, server_side: bool) -> Option<IpEndpoint> {
    let conns = LOCAL_TCP_CONNS.exclusive_access();
    let conn = conns.get(&conn_id)?;
    if server_side {
        Some(conn.client)
    } else {
        Some(conn.server)
    }
}

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

    // Update the server socket's handle to the new one
    if sockfd < inner.fd_table.len() {
        if let Some(file) = &inner.fd_table[sockfd] {
            if let Some(s) = file.as_any().downcast_ref::<SocketFile>() {
                s.set_handle(new_handle);
            }
        }
    }

    // The new fd gets the old (now active) handle
    let new_fd = inner.alloc_fd();
    if new_fd >= inner.fd_table.len() {
        return -24;
    }
    inner.fd_table[new_fd] = Some(Arc::new(SocketFile::new(handle, SocketType::Tcp, nonblock)));

    // Release inner lock before writing peer address (avoids holding lock during net_poll)
    drop(inner);

    // Ensure any pending data from client is delivered to the accepted socket
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

    // For UDP, require destination address
    if st == SocketType::Udp && (dest_addr.is_null() || addrlen < 8) {
        return -89; // EDESTADDRREQ
    }
    // For TCP, dest_addr is ignored (use connected peer)

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

    // Fill source address for UDP if requested
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

        // Fill msg_flags
        let msghdr_mut: &mut Msghdr = translated_refmut(token, msg as *mut Msghdr);
        msghdr_mut.msg_flags = 0;

        // Fill msg_name with peer address for UDP
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

    fn write_sockopt_bytes(token: PageTable, optval: *mut u8, optlen: *mut usize, data: &[u8]) {
        let requested = *translated_ref(token, optlen as *const u32) as usize;
        let n = requested.min(data.len());
        translated_byte_buffer(token, optval, n).copy_from_slice(&data[..n]);
        *translated_refmut(token, optlen as *mut u32) = n as u32;
    }

    fn write_sockopt_i32(token: PageTable, optval: *mut u8, optlen: *mut usize, value: i32) {
        write_sockopt_bytes(token, optval, optlen, &value.to_ne_bytes());
    }

    fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
        if offset + 4 <= buf.len() {
            buf[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
        }
    }

    fn put_u64(buf: &mut [u8], offset: usize, value: u64) {
        if offset + 8 <= buf.len() {
            buf[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
        }
    }

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

pub(crate) fn udp_write(handle: SocketHandle, data: &[u8], peer: Option<IpEndpoint>) -> isize {
    udp_write_with_endpoint(handle, data, smoltcp_udp_endpoint(handle), peer)
}

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

pub(crate) fn udp_recv(handle: SocketHandle, nonblock: bool, buf: &mut [u8]) -> isize {
    let local = smoltcp_udp_endpoint(handle);
    udp_recv_with_endpoints(handle, nonblock, buf, local, None)
}

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

fn sock_is_active_tcp(handle: SocketHandle) -> bool {
    crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.is_active()
    })
    .unwrap_or(false)
}

fn sock_can_send_tcp(handle: SocketHandle) -> bool {
    crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.can_send()
    })
    .unwrap_or(false)
}

fn sock_recv_queue_tcp(handle: SocketHandle) -> usize {
    crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.recv_queue()
    })
    .unwrap_or(0)
}
