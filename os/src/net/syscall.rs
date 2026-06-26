use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::Socket as TcpSocket;
use smoltcp::socket::tcp::SocketBuffer as TcpSocketBuffer;
use smoltcp::socket::udp::PacketBuffer as UdpPacketBuffer;
use smoltcp::socket::udp::PacketMetadata as UdpPacketMetadata;
use smoltcp::socket::udp::Socket as UdpSocket;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

use crate::fs::File;
use crate::fs::stdio::stdout_release_if_owner;
use crate::mm::{translated_ref, translated_refmut};
use crate::net::socket::{SocketFile, SocketType};
use crate::sync::UPSafeCell;
use crate::task::{
    add_task, block_current_and_run_next, current_task, current_user_token,
    suspend_current_and_run_next, BlockReason, TaskStatus, NET_BLOCK_QUEUE, PID2TCB,
};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use lazy_static::*;

const SOCKET_BUFFER_SIZE: usize = 65536;
const UDP_META_COUNT: usize = 16;
const AF_UNIX: usize = 1;
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
const SO_RCVBUF: usize = 8;
const SO_SNDBUF: usize = 7;
const SO_KEEPALIVE: usize = 9;

const IPPROTO_TCP: usize = 6;
const TCP_NODELAY: usize = 1;

const SHUT_RD: usize = 0;
const SHUT_WR: usize = 1;
const SHUT_RDWR: usize = 2;
const LOCAL_TCP_BUFFER_LIMIT: usize = 256 * 1024;
static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(49152);
static NEXT_DUMMY_HANDLE: AtomicUsize = AtomicUsize::new(1_000_000);
static LOCAL_UDP_DEBUG_COUNT: AtomicUsize = AtomicUsize::new(0);
static LOCAL_UDP_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
static LOCAL_UDP_BIG_RECV_COUNT: AtomicUsize = AtomicUsize::new(0);

fn log_local_udp_event(tag: &str, src: IpEndpoint, dst_port: u16, len: usize) {
    let idx = LOCAL_UDP_DEBUG_COUNT.fetch_add(1, Ordering::Relaxed);
    if idx < 16 {
        println!(
            "[udp-local] {} src={:?} dst_port={} len={}",
            tag, src, dst_port, len
        );
    }
}

fn log_local_udp_drop(src: IpEndpoint, dst_port: u16, len: usize) {
    let idx = LOCAL_UDP_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
    if idx < 8 {
        println!(
            "[udp-local] drop src={:?} dst_port={} len={}",
            src, dst_port, len
        );
    }
}

fn log_local_udp_big_recv(tag: &str, src: IpEndpoint, dst_port: u16, len: usize) {
    if len < 100 {
        return;
    }
    let idx = LOCAL_UDP_BIG_RECV_COUNT.fetch_add(1, Ordering::Relaxed);
    if idx < 16 {
        println!(
            "[udp-local] {} src={:?} dst_port={} len={}",
            tag, src, dst_port, len
        );
    }
}

#[derive(Clone)]
struct LocalUdpPacket {
    dst_port: u16,
    src: IpEndpoint,
    data: Vec<u8>,
}

struct LocalTcpConn {
    client_to_server: VecDeque<u8>,
    server_to_client: VecDeque<u8>,
    client_endpoint: IpEndpoint,
    server_endpoint: IpEndpoint,
    client_closed: bool,
    server_closed: bool,
}

struct LocalTcpListener {
    port: u16,
    pending: VecDeque<usize>,
}

lazy_static! {
    static ref LOCAL_UDP_QUEUE: UPSafeCell<VecDeque<LocalUdpPacket>> =
        unsafe { UPSafeCell::new(VecDeque::new()) };
    static ref LOCAL_TCP_CONNS: UPSafeCell<Vec<Option<LocalTcpConn>>> =
        unsafe { UPSafeCell::new(Vec::new()) };
    static ref LOCAL_TCP_LISTENERS: UPSafeCell<Vec<LocalTcpListener>> =
        unsafe { UPSafeCell::new(Vec::new()) };
}

fn net_ready() -> bool {
    crate::net::net_available_fast()
}

fn dummy_socket_handle() -> SocketHandle {
    unsafe { core::mem::transmute(NEXT_DUMMY_HANDLE.fetch_add(1, Ordering::Relaxed)) }
}

fn create_socket_fd(st: SocketType, nonblock: bool, cloexec: bool) -> isize {
    let handle = if st == SocketType::Unix {
        dummy_socket_handle()
    } else if net_ready() {
        match add_to_socket_set(st) {
            Some((handle, _)) => handle,
            None => return -105,
        }
    } else {
        dummy_socket_handle()
    };
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let fd = inner.alloc_fd();
    if fd >= inner.fd_table.len() {
        return -24;
    }
    let file: Arc<dyn File + Send + Sync> = Arc::new(SocketFile::new(handle, st, nonblock));
    inner.fd_table[fd] = Some(file.clone());
    inner.fd_cloexec[fd] = cloexec;
    let tgid = inner.tgid;
    drop(inner);
    install_fd_for_thread_group(task.getpid(), tgid, fd, file, cloexec);
    fd as isize
}

fn alloc_null_socket_fd(cloexec: bool) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let fd = inner.alloc_fd();
    if fd >= inner.fd_table.len() {
        return -24;
    }
    inner.fd_table[fd] = Some(Arc::new(crate::fs::NullSocket));
    inner.fd_cloexec[fd] = cloexec;
    fd as isize
}

fn install_fd_for_thread_group(
    owner_pid: usize,
    tgid: usize,
    fd: usize,
    file: Arc<dyn File + Send + Sync>,
    cloexec: bool,
) {
    let tasks: Vec<_> = PID2TCB
        .exclusive_access()
        .values()
        .filter(|task| task.getpid() != owner_pid)
        .filter(|task| {
            task.try_inner_borrow()
                .map(|inner| inner.tgid == tgid)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    for task in tasks {
        if let Some(mut inner) = task.try_inner_exclusive_access() {
            while inner.fd_table.len() <= fd {
                inner.fd_table.push(None);
                inner.fd_cloexec.push(false);
            }
            file.dup_ref();
            inner.fd_table[fd] = Some(file.clone());
            inner.fd_cloexec[fd] = cloexec;
        }
    }
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

fn sockaddr_family(ptr: *const u8) -> Option<u16> {
    if ptr.is_null() {
        return None;
    }
    let token = current_user_token();
    Some(*translated_ref(token, ptr as *const u16))
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
    if st == SocketType::Unix {
        return None;
    }
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
            SocketType::Unix => unreachable!(),
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

fn is_local_ipv4(addr: IpAddress) -> bool {
    let IpAddress::Ipv4(ipv4) = addr;
    ipv4.0[0] == 127 || ipv4.0 == [0, 0, 0, 0] || ipv4.0 == [10, 0, 2, 15]
}

pub(crate) fn is_local_endpoint(endpoint: IpEndpoint) -> bool {
    is_local_ipv4(endpoint.addr)
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

fn local_tcp_listener_index(port: u16) -> Option<usize> {
    let listeners = LOCAL_TCP_LISTENERS.exclusive_access();
    listeners.iter().position(|listener| listener.port == port)
}

fn local_tcp_endpoint(addr: IpEndpoint) -> bool {
    is_local_ipv4(addr.addr)
}

fn set_socket_local_tcp(sockfd: usize, conn_id: usize, server_side: bool) -> bool {
    let Some(task) = current_task() else {
        return false;
    };
    let inner = task.inner_exclusive_access();
    if sockfd >= inner.fd_table.len() {
        return false;
    }
    let Some(file) = &inner.fd_table[sockfd] else {
        return false;
    };
    let Some(sock) = file.as_any().downcast_ref::<SocketFile>() else {
        return false;
    };
    sock.set_local_tcp(conn_id, server_side);
    true
}

fn socket_local_tcp(sockfd: usize) -> Option<(usize, bool, bool)> {
    let task = current_task()?;
    let inner = task.inner_exclusive_access();
    if sockfd >= inner.fd_table.len() {
        return None;
    }
    let file = inner.fd_table[sockfd].as_ref()?;
    let sock = file.as_any().downcast_ref::<SocketFile>()?;
    let (conn_id, server_side) = sock.get_local_tcp()?;
    Some((conn_id, server_side, sock.get_nonblock()))
}

pub(crate) fn local_tcp_listen(port: u16) {
    let mut listeners = LOCAL_TCP_LISTENERS.exclusive_access();
    if listeners.iter().all(|listener| listener.port != port) {
        listeners.push(LocalTcpListener {
            port,
            pending: VecDeque::new(),
        });
    }
}

pub(crate) fn local_tcp_unlisten(port: u16) {
    let mut listeners = LOCAL_TCP_LISTENERS.exclusive_access();
    listeners.retain(|listener| listener.port != port);
}

pub(crate) fn local_tcp_pending_on(port: u16) -> bool {
    let listeners = LOCAL_TCP_LISTENERS.exclusive_access();
    listeners
        .iter()
        .find(|listener| listener.port == port)
        .map(|listener| !listener.pending.is_empty())
        .unwrap_or(false)
}

pub(crate) fn local_udp_pending_on(port: u16) -> bool {
    let queue = LOCAL_UDP_QUEUE.exclusive_access();
    queue.iter().any(|packet| packet.dst_port == port)
}

fn local_tcp_connect(
    port: u16,
    client_endpoint: IpEndpoint,
    server_endpoint: IpEndpoint,
) -> Option<usize> {
    let listener_idx = local_tcp_listener_index(port)?;
    let conn_id = {
        let mut conns = LOCAL_TCP_CONNS.exclusive_access();
        let conn = LocalTcpConn {
            client_to_server: VecDeque::new(),
            server_to_client: VecDeque::new(),
            client_endpoint,
            server_endpoint,
            client_closed: false,
            server_closed: false,
        };
        if let Some(idx) = conns.iter().position(|slot| slot.is_none()) {
            conns[idx] = Some(conn);
            idx
        } else {
            conns.push(Some(conn));
            conns.len() - 1
        }
    };
    let mut listeners = LOCAL_TCP_LISTENERS.exclusive_access();
    if let Some(listener) = listeners.get_mut(listener_idx) {
        listener.pending.push_back(conn_id);
        Some(conn_id)
    } else {
        None
    }
}

fn local_tcp_accept(port: u16) -> Option<usize> {
    let mut listeners = LOCAL_TCP_LISTENERS.exclusive_access();
    listeners
        .iter_mut()
        .find(|listener| listener.port == port)
        .and_then(|listener| listener.pending.pop_front())
}

fn local_tcp_client_endpoint(conn_id: usize) -> Option<IpEndpoint> {
    let conns = LOCAL_TCP_CONNS.exclusive_access();
    conns
        .get(conn_id)
        .and_then(|slot| slot.as_ref())
        .map(|conn| conn.client_endpoint)
}

fn local_tcp_server_endpoint(conn_id: usize) -> Option<IpEndpoint> {
    let conns = LOCAL_TCP_CONNS.exclusive_access();
    conns
        .get(conn_id)
        .and_then(|slot| slot.as_ref())
        .map(|conn| conn.server_endpoint)
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
        let ret = {
            let mut conns = LOCAL_TCP_CONNS.exclusive_access();
            let Some(Some(conn)) = conns.get_mut(conn_id) else {
                return -32;
            };
            if (server_side && conn.client_closed) || (!server_side && conn.server_closed) {
                return -32;
            }
            let dst = if server_side {
                &mut conn.server_to_client
            } else {
                &mut conn.client_to_server
            };
            let room = LOCAL_TCP_BUFFER_LIMIT.saturating_sub(dst.len());
            if room == 0 {
                -11
            } else {
                let n = room.min(data.len() - offset);
                for byte in &data[offset..offset + n] {
                    dst.push_back(*byte);
                }
                n as isize
            }
        };
        if ret > 0 {
            offset += ret as usize;
            if offset == data.len() || nonblock {
                return offset as isize;
            }
        } else if nonblock {
            return if offset > 0 { offset as isize } else { -11 };
        } else {
            stdout_release_if_owner();
            suspend_current_and_run_next();
        }
    }
}

pub(crate) fn local_tcp_recv(
    conn_id: usize,
    server_side: bool,
    nonblock: bool,
    peek: bool,
    buf: &mut [u8],
) -> isize {
    if buf.is_empty() {
        return 0;
    }
    loop {
        let ret = {
            let mut conns = LOCAL_TCP_CONNS.exclusive_access();
            let Some(Some(conn)) = conns.get_mut(conn_id) else {
                return 0;
            };
            let (src, peer_closed) = if server_side {
                (&mut conn.client_to_server, conn.client_closed)
            } else {
                (&mut conn.server_to_client, conn.server_closed)
            };
            let n = buf.len().min(src.len());
            if peek {
                for (out, byte) in buf.iter_mut().zip(src.iter()).take(n) {
                    *out = *byte;
                }
            } else {
                for out in buf.iter_mut().take(n) {
                    *out = src.pop_front().unwrap();
                }
            }
            if n > 0 {
                n as isize
            } else if peer_closed {
                0
            } else {
                -11
            }
        };
        if ret != -11 || nonblock {
            return ret;
        }
        stdout_release_if_owner();
        suspend_current_and_run_next();
    }
}

pub(crate) fn local_tcp_close(conn_id: usize, server_side: bool) {
    let mut conns = LOCAL_TCP_CONNS.exclusive_access();
    let Some(Some(conn)) = conns.get_mut(conn_id) else {
        return;
    };
    if server_side {
        conn.server_closed = true;
    } else {
        conn.client_closed = true;
    }
    if conn.server_closed && conn.client_closed {
        conns[conn_id] = None;
    }
}

pub(crate) fn local_tcp_shutdown(conn_id: usize, server_side: bool, how: usize) -> isize {
    match how {
        SHUT_RD | SHUT_WR | SHUT_RDWR => {
            local_tcp_close(conn_id, server_side);
            0
        }
        _ => -22,
    }
}

pub(crate) fn local_tcp_poll(conn_id: usize, server_side: bool) -> (bool, bool, bool) {
    let conns = LOCAL_TCP_CONNS.exclusive_access();
    let Some(Some(conn)) = conns.get(conn_id) else {
        return (true, false, true);
    };
    let (recv, send_room, peer_closed, self_closed) = if server_side {
        (
            !conn.client_to_server.is_empty(),
            conn.server_to_client.len() < LOCAL_TCP_BUFFER_LIMIT,
            conn.client_closed,
            conn.server_closed,
        )
    } else {
        (
            !conn.server_to_client.is_empty(),
            conn.client_to_server.len() < LOCAL_TCP_BUFFER_LIMIT,
            conn.server_closed,
            conn.client_closed,
        )
    };
    (
        recv || peer_closed,
        send_room && !peer_closed && !self_closed,
        self_closed,
    )
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
    let src = source_addr_for_dest(local, dest);
    set_socket_local_endpoint(sockfd, src);
    src
}

fn ensure_udp_socket_local_endpoint(
    sock: &SocketFile,
    handle: SocketHandle,
    dest: IpEndpoint,
) -> IpEndpoint {
    if let Some(local) = sock.get_local_endpoint() {
        return source_addr_for_dest(local, dest);
    }
    if let Some(local) = smoltcp_udp_endpoint(handle) {
        sock.set_local_endpoint(local);
        return source_addr_for_dest(local, dest);
    }

    let port = next_ephemeral_port();
    let local = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), port);
    if net_ready() {
        crate::net::with_net_iface(|iface| {
            let sock: &mut UdpSocket = iface.sockets.get_mut(handle);
            let _ = sock.bind(IpListenEndpoint { addr: None, port });
        });
    }
    let src = source_addr_for_dest(local, dest);
    sock.set_local_endpoint(src);
    src
}

fn enqueue_local_udp(src: IpEndpoint, data: &[u8], dest: IpEndpoint) -> isize {
    {
        let mut queue = LOCAL_UDP_QUEUE.exclusive_access();
        if queue.len() >= 256 {
            log_local_udp_drop(src, dest.port, data.len());
            return data.len() as isize;
        }
        log_local_udp_event("enqueue", src, dest.port, data.len());
        queue.push_back(LocalUdpPacket {
            dst_port: dest.port,
            src,
            data: data.to_vec(),
        });
    }
    wake_local_udp_receivers();
    crate::task::wake_poll_wait_tasks();
    data.len() as isize
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
    Some(enqueue_local_udp(src, data, dest))
}

pub(crate) fn try_write_local_udp(
    sock: &SocketFile,
    handle: SocketHandle,
    data: &[u8],
    dest: IpEndpoint,
) -> Option<isize> {
    if !is_local_ipv4(dest.addr) {
        return None;
    }
    let src = ensure_udp_socket_local_endpoint(sock, handle, dest);
    Some(enqueue_local_udp(src, data, dest))
}

fn local_recv_endpoint(sockfd: usize, handle: SocketHandle) -> Option<IpEndpoint> {
    get_socket_local_endpoint(sockfd).or_else(|| {
        let ep = smoltcp_udp_endpoint(handle)?;
        set_socket_local_endpoint(sockfd, ep);
        Some(ep)
    })
}

fn local_recv_endpoint_socket(sock: &SocketFile, handle: SocketHandle) -> Option<IpEndpoint> {
    sock.get_local_endpoint().or_else(|| {
        let ep = smoltcp_udp_endpoint(handle)?;
        sock.set_local_endpoint(ep);
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
    let mut queue = LOCAL_UDP_QUEUE.exclusive_access();
    let pos = queue
        .iter()
        .position(|packet| packet.dst_port == local.port)?;
    let packet = if peek {
        queue[pos].clone()
    } else {
        queue.remove(pos)?
    };
    let n = packet.data.len().min(buf.len());
    buf[..n].copy_from_slice(&packet.data[..n]);
    log_local_udp_event("recv_fd", packet.src, local.port, n);
    log_local_udp_big_recv("recv_fd_big", packet.src, local.port, n);
    drop(queue);

    if !src_addr.is_null() && !addrlen.is_null() {
        let IpAddress::Ipv4(ipv4) = packet.src.addr;
        let token = current_user_token();
        *translated_refmut(token, addrlen as *mut u32) = core::mem::size_of::<SockAddrIn>() as u32;
        write_sockaddr_in(src_addr, u32::from_be_bytes(ipv4.0), packet.src.port);
    }

    Some(n as isize)
}

fn try_recv_local_udp_socket(
    sock: &SocketFile,
    handle: SocketHandle,
    buf: &mut [u8],
    peek: bool,
) -> Option<isize> {
    let local = local_recv_endpoint_socket(sock, handle)?;
    let mut queue = LOCAL_UDP_QUEUE.exclusive_access();
    let pos = queue
        .iter()
        .position(|packet| packet.dst_port == local.port)?;
    let packet = if peek {
        queue[pos].clone()
    } else {
        queue.remove(pos)?
    };
    let n = packet.data.len().min(buf.len());
    buf[..n].copy_from_slice(&packet.data[..n]);
    log_local_udp_event("recv_sock", packet.src, local.port, n);
    log_local_udp_big_recv("recv_sock_big", packet.src, local.port, n);
    Some(n as isize)
}

pub(crate) fn udp_read_socket(
    sock: &SocketFile,
    handle: SocketHandle,
    nonblock: bool,
    buf: &mut [u8],
) -> isize {
    if let Some(ret) = try_recv_local_udp_socket(sock, handle, buf, false) {
        return ret;
    }
    if nonblock {
        return udp_recv(handle, true, buf);
    }

    loop {
        crate::net::net_poll();
        crate::net::net_poll();
        crate::net::net_poll();
        if let Some(ret) = try_recv_local_udp_socket(sock, handle, buf, false) {
            return ret;
        }
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

pub fn sys_socket(domain: usize, type_: usize, _protocol: usize) -> isize {
    let st = match (domain, type_ & 0x0F) {
        (AF_INET, SOCK_STREAM) => SocketType::Tcp,
        (AF_INET, SOCK_DGRAM) => SocketType::Udp,
        (AF_UNIX, SOCK_STREAM) | (AF_UNIX, SOCK_DGRAM) => SocketType::Unix,
        (AF_INET | AF_UNIX, _) => return -22,
        _ => return -97,
    };

    let nonblock = (type_ & SOCK_NONBLOCK) != 0;
    let cloexec = (type_ & SOCK_CLOEXEC) != 0;

    create_socket_fd(st, nonblock, cloexec)
}

pub fn sys_bind(sockfd: usize, addr: *const u8, addrlen: usize) -> isize {
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

    if !net_ready() {
        let local_addr = if ip == 0 {
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)
        } else {
            endpoint.addr
        };
        set_socket_local_endpoint(sockfd, IpEndpoint::new(local_addr, port));
        if st == SocketType::Tcp {
            local_tcp_listen(port);
        }
        return 0;
    }

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
            sock.bind(if ip == 0 {
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
        SocketType::Unix => -95,
    })
    .unwrap_or(-1);
    if ret == 0 {
        let local_addr = if ip == 0 {
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)
        } else {
            endpoint.addr
        };
        set_socket_local_endpoint(sockfd, IpEndpoint::new(local_addr, port));
        if st == SocketType::Tcp {
            local_tcp_listen(port);
        }
    }
    ret
}

pub fn sys_connect(sockfd: usize, addr: *const u8, addrlen: usize) -> isize {
    if addr.is_null() {
        return -14;
    }

    let (handle, st, nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    if st == SocketType::Unix {
        if addrlen < 2 {
            return -22;
        }
        return match sockaddr_family(addr) {
            Some(family) if family as usize == AF_UNIX => -2,
            Some(_) => -97,
            None => -14,
        };
    }

    if addrlen < 8 {
        return -14;
    }

    let (ip, port) = match parse_sockaddr_in(addr) {
        Some(v) => v,
        None => return -22,
    };

    let remote = IpEndpoint::new(
        IpAddress::from(Ipv4Address::from_bytes(&ip.to_be_bytes())),
        port,
    );

    if st == SocketType::Udp {
        let task = current_task().unwrap();
        let inner = task.inner_exclusive_access();
        if sockfd < inner.fd_table.len() {
            if let Some(file) = &inner.fd_table[sockfd] {
                if let Some(s) = file.as_any().downcast_ref::<SocketFile>() {
                    s.set_peer_endpoint(remote);
                }
            }
        }
        return 0;
    }

    if st != SocketType::Tcp {
        return -95;
    }

    if local_tcp_endpoint(remote) {
        let local = source_addr_for_dest(
            IpEndpoint::new(
                IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                next_ephemeral_port(),
            ),
            remote,
        );
        let server = source_addr_for_dest(
            IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), port),
            remote,
        );
        if let Some(conn_id) = local_tcp_connect(port, local, server) {
            set_socket_local_tcp(sockfd, conn_id, false);
            set_socket_local_endpoint(sockfd, local);
            if let Some(task) = current_task() {
                let inner = task.inner_exclusive_access();
                if sockfd < inner.fd_table.len() {
                    if let Some(file) = &inner.fd_table[sockfd] {
                        if let Some(sock) = file.as_any().downcast_ref::<SocketFile>() {
                            sock.set_peer_endpoint(remote);
                        }
                    }
                }
            }
            return 0;
        }
        return -111;
    }

    if !net_ready() {
        return -97;
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

    if !net_ready() {
        if let Some(local) = get_socket_local_endpoint(sockfd) {
            local_tcp_listen(local.port);
            return 0;
        }
        return -22;
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
        local_tcp_listen(local.port);
    }
    0
}

pub fn sys_accept(sockfd: usize, addr: *mut u8, addrlen: *mut usize) -> isize {
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
    let local_port = sock.get_local_endpoint().map(|ep| ep.port);
    drop(inner);

    if let Some(port) = local_port {
        loop {
            if let Some(conn_id) = local_tcp_accept(port) {
                let new_handle = if net_ready() {
                    match add_to_socket_set(SocketType::Tcp) {
                        Some((handle, _)) => handle,
                        None => return -105,
                    }
                } else {
                    dummy_socket_handle()
                };
                let task = current_task().unwrap();
                let mut inner = task.inner_exclusive_access();
                let new_fd = inner.alloc_fd();
                if new_fd >= inner.fd_table.len() {
                    return -24;
                }
                let new_sock = Arc::new(SocketFile::new(new_handle, SocketType::Tcp, nonblock));
                new_sock.set_local_tcp(conn_id, true);
                if let Some(local) = local_tcp_server_endpoint(conn_id) {
                    new_sock.set_local_endpoint(local);
                }
                if let Some(peer) = local_tcp_client_endpoint(conn_id) {
                    new_sock.set_peer_endpoint(peer);
                }
                let file: Arc<dyn File + Send + Sync> = new_sock.clone();
                inner.fd_table[new_fd] = Some(file.clone());
                let tgid = inner.tgid;
                let cloexec = inner.fd_cloexec.get(new_fd).copied().unwrap_or(false);
                drop(inner);
                install_fd_for_thread_group(task.getpid(), tgid, new_fd, file, cloexec);
                return new_fd as isize;
            }
            if nonblock {
                return -11;
            }
            stdout_release_if_owner();
            suspend_current_and_run_next();
        }
    }

    if !net_ready() {
        return -22;
    }

    crate::net::net_poll();

    if !nonblock {
        let mut prev_listening = true;
        loop {
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
    let file: Arc<dyn File + Send + Sync> = Arc::new(SocketFile::new(handle, SocketType::Tcp, nonblock));
    inner.fd_table[new_fd] = Some(file.clone());
    let tgid = inner.tgid;
    let cloexec = inner.fd_cloexec.get(new_fd).copied().unwrap_or(false);

    // Release inner lock before writing peer address (avoids holding lock during net_poll)
    drop(inner);
    install_fd_for_thread_group(task.getpid(), tgid, new_fd, file, cloexec);

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

pub fn sys_sendto(
    sockfd: usize,
    buf: *const u8,
    len: usize,
    flags: usize,
    dest_addr: *const u8,
    addrlen: usize,
) -> isize {
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
            if let Some((conn_id, server_side, _)) = socket_local_tcp(sockfd) {
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
        SocketType::Unix => -107,
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
    let (handle, st, nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let dontwait = nonblock || (flags & MSG_DONTWAIT) != 0;
    let peek = (flags & MSG_PEEK) != 0;

    let recv_buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };

    let result = match st {
        SocketType::Tcp => {
            if let Some((conn_id, server_side, _)) = socket_local_tcp(sockfd) {
                local_tcp_recv(conn_id, server_side, dontwait, peek, recv_buf)
            } else {
                tcp_recv(handle, dontwait, recv_buf)
            }
        }
        SocketType::Udp => loop {
            if let Some(ret) = try_recv_local_udp(sockfd, handle, recv_buf, src_addr, addrlen, peek)
            {
                return ret;
            }
            let ret = if net_ready() {
                udp_recv(handle, true, recv_buf)
            } else {
                -11
            };
            if ret != -11 || dontwait {
                break ret;
            }
            block_current_and_run_next(BlockReason::UdpRecv(unsafe {
                core::mem::transmute(handle)
            }));
        },
        SocketType::Unix => -107,
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
            if let Some((conn_id, server_side, _)) = socket_local_tcp(sockfd) {
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
                None
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
        SocketType::Unix => -107,
    }
}

pub fn sys_recvmsg(sockfd: usize, msg: *mut u8, flags: usize) -> isize {
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
            if let Some((conn_id, server_side, _)) = socket_local_tcp(sockfd) {
                local_tcp_recv(conn_id, server_side, dontwait, peek, &mut buf)
            } else {
                tcp_recv(handle, dontwait, &mut buf)
            }
        }
        SocketType::Udp => loop {
            if let Some(ret) = try_recv_local_udp(
                sockfd,
                handle,
                &mut buf,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                peek,
            ) {
                break ret;
            }
            let ret = if net_ready() {
                udp_recv(handle, true, &mut buf)
            } else {
                -11
            };
            if ret != -11 || dontwait {
                break ret;
            }
            block_current_and_run_next(BlockReason::UdpRecv(unsafe {
                core::mem::transmute(handle)
            }));
        },
        SocketType::Unix => -107,
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
                        if net_ready() {
                            crate::net::with_net_iface(|iface| {
                                let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
                                if val {
                                    sock.set_keep_alive(Some(smoltcp::time::Duration::from_secs(
                                        7200,
                                    )));
                                } else {
                                    sock.set_keep_alive(None);
                                }
                            });
                        }
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
                if net_ready() {
                    crate::net::with_net_iface(|iface| {
                        let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
                        sock.set_nagle_enabled(!val);
                    });
                }
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
    if optval.is_null() || optlen.is_null() {
        return -14;
    }

    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let token = current_user_token();

    match (level, optname) {
        (SOL_SOCKET, SO_REUSEADDR) | (SOL_SOCKET, SO_KEEPALIVE) => {
            *translated_refmut(token, optlen as *mut u32) = 4;
            *translated_refmut(token, optval as *mut i32) = 1;
            0
        }
        (SOL_SOCKET, SO_RCVBUF) | (SOL_SOCKET, SO_SNDBUF) => {
            *translated_refmut(token, optlen as *mut u32) = 4;
            *translated_refmut(token, optval as *mut i32) = SOCKET_BUFFER_SIZE as i32;
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
            *translated_refmut(token, optlen as *mut u32) = 4;
            *translated_refmut(token, optval as *mut i32) = if nagle { 1 } else { 0 };
            0
        }
        (IPPROTO_TCP, 2) => {
            // TCP_MAXSEG - return typical Ethernet MSS
            if st != SocketType::Tcp {
                return -95;
            }
            *translated_refmut(token, optlen as *mut u32) = 4;
            *translated_refmut(token, optval as *mut i32) = 1460;
            0
        }
        (IPPROTO_TCP, 11) | (IPPROTO_TCP, 13) => {
            *translated_refmut(token, optlen as *mut u32) = 4;
            *translated_refmut(token, optval as *mut i32) = 65536;
            0
        }
        (IPPROTO_TCP, 14) => {
            *translated_refmut(token, optlen as *mut u32) = 4;
            *translated_refmut(token, optval as *mut u32) = u32::from_be_bytes(*b"reno");
            0
        }
        _ => -92, // ENOPROTOOPT
    }
}

pub fn sys_sock_shutdown(sockfd: usize, how: usize) -> isize {
    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    if st != SocketType::Tcp {
        return -95;
    }

    if let Some((conn_id, server_side, _)) = socket_local_tcp(sockfd) {
        return local_tcp_shutdown(conn_id, server_side, how);
    }

    if !net_ready() {
        return -97;
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
    if addr.is_null() || addrlen.is_null() {
        return -14;
    }

    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let peer: Option<IpEndpoint> = match st {
        SocketType::Tcp => crate::net::with_net_iface(|iface| {
            let sock: &TcpSocket = iface.sockets.get(handle);
            sock.remote_endpoint()
        })
        .flatten()
        .or_else(|| get_socket_peer_endpoint(sockfd)),
        SocketType::Udp => get_socket_peer_endpoint(sockfd).or_else(|| {
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
        }),
        SocketType::Unix => None,
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
        SocketType::Udp => crate::net::with_net_iface(|iface| {
            let sock: &UdpSocket = iface.sockets.get(handle);
            sock.endpoint()
        })
        .map(|ep| {
            IpEndpoint::new(
                ep.addr.unwrap_or(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)),
                ep.port,
            )
        })
        .or_else(|| get_socket_local_endpoint(sockfd)),
        SocketType::Unix => None,
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

pub(crate) fn udp_write(handle: SocketHandle, data: &[u8], peer: Option<IpEndpoint>) -> isize {
    crate::net::net_poll();
    match peer {
        Some(dest) => crate::net::with_net_iface(|iface| {
            let sock: &mut UdpSocket = iface.sockets.get_mut(handle);
            match sock.send_slice(data, dest) {
                Ok(()) => data.len() as isize,
                Err(_) => -11,
            }
        })
        .unwrap_or(-1),
        None => -89, // EDESTADDRREQ
    }
}

pub(crate) fn udp_recv(handle: SocketHandle, nonblock: bool, buf: &mut [u8]) -> isize {
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
