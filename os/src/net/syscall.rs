use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::SocketBuffer as TcpSocketBuffer;
use smoltcp::socket::tcp::Socket as TcpSocket;
use smoltcp::socket::udp::PacketBuffer as UdpPacketBuffer;
use smoltcp::socket::udp::PacketMetadata as UdpPacketMetadata;
use smoltcp::socket::udp::Socket as UdpSocket;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

use crate::mm::{translated_ref, translated_refmut};
use crate::net::socket::{SocketFile, SocketType};
use crate::task::{current_task, current_user_token, block_current_and_run_next, BlockReason};
use alloc::sync::Arc;

const SOCKET_BUFFER_SIZE: usize = 65536;
const UDP_META_COUNT: usize = 16;
const AF_INET: usize = 2;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;
const SOCK_NONBLOCK: usize = 0x800;

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

pub fn sys_socket(domain: usize, type_: usize, _protocol: usize) -> isize {
    if domain != AF_INET {
        return -97;
    }

    let st = match type_ & 0x0F {
        SOCK_STREAM => SocketType::Tcp,
        SOCK_DGRAM => SocketType::Udp,
        _ => return -22,
    };

    let nonblock = (type_ & SOCK_NONBLOCK) != 0;

    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();

    let (handle, st) = match add_to_socket_set(st) {
        Some(v) => v,
        None => return -105,
    };

    let fd = inner.alloc_fd();
    inner.fd_table[fd] = Some(Arc::new(SocketFile::new(handle, st, nonblock)));
    fd as isize
}

pub fn sys_bind(sockfd: usize, addr: *const u8, addrlen: usize) -> isize {
    if addr.is_null() || addrlen < 8 {
        return -14;
    }

    let (ip, port) = match parse_sockaddr_in(addr) {
        Some(v) => v,
        None => return -22,
    };

    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let endpoint = IpEndpoint::new(
        IpAddress::from(Ipv4Address::from_bytes(&ip.to_be_bytes())),
        port,
    );

    crate::net::with_net_iface(|iface| {
        match st {
            SocketType::Tcp => {
                let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
                sock.listen(endpoint).map(|_| 0).unwrap_or(-22)
            }
            SocketType::Udp => {
                let sock: &mut UdpSocket = iface.sockets.get_mut(handle);
                sock.bind(endpoint).map(|_| 0).unwrap_or(-22)
            }
        }
    }).unwrap_or(-1)
}

pub fn sys_connect(sockfd: usize, addr: *const u8, addrlen: usize) -> isize {
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
    if st != SocketType::Tcp {
        return -95;
    }

    let remote = IpEndpoint::new(
        IpAddress::from(Ipv4Address::from_bytes(&ip.to_be_bytes())),
        port,
    );

    let local = IpListenEndpoint {
        addr: None,
        port: 32768 + ((sockfd as u16).wrapping_mul(7919) % 32767),  // non-zero ephemeral port
    };

    crate::net::with_net_iface_tcp_connect(handle, remote, local);
    crate::net::net_poll();

    if !nonblock {
        loop {
            let connected = crate::net::with_net_iface(|iface| {
                let sock: &TcpSocket = iface.sockets.get(handle);
                sock.is_active()
            }).unwrap_or(false);
            if connected {
                return 0;
            }
            block_current_and_run_next(BlockReason::TcpConnect(unsafe { core::mem::transmute(handle) }));
        }
    }

    let connected = crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.is_active()
    }).unwrap_or(false);
    if connected { 0 } else { -115 } // EINPROGRESS
}

pub fn sys_listen(sockfd: usize, _backlog: usize) -> isize {
    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };
    if st != SocketType::Tcp {
        return -95;
    }

    let listening = crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.is_listening()
    }).unwrap_or(false);

    if !listening {
        return -22;
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
    drop(inner);

    crate::net::net_poll();

    if !nonblock {
        let mut prev_listening = true;
        loop {
            crate::net::net_poll();
            let active = crate::net::with_net_iface(|iface| {
                let sock: &TcpSocket = iface.sockets.get(handle);
                let listening = sock.is_listening();
                let active = sock.is_active();
                if !listening && prev_listening && active {
                    return true;
                }
                prev_listening = listening;
                false
            }).unwrap_or(false);

            if active {
                break;
            }
            block_current_and_run_next(BlockReason::TcpAccept(unsafe { core::mem::transmute(handle) }));
        }
    }

    let active = crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.is_active()
    }).unwrap_or(false);

    if !active {
        return -11;
    }

    let old_local = crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.local_endpoint()
    }).flatten();

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
                addr: Some(local_ep.addr),
                port: local_ep.port,
            };
            let sock: &mut TcpSocket = iface.sockets.get_mut(h);
            let _ = sock.listen(listen_endpoint);
        }
        h
    }).expect("NET_IFACE");

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
    inner.fd_table[new_fd] = Some(Arc::new(SocketFile::new(handle, SocketType::Tcp, nonblock)));

    if !addr.is_null() && !addrlen.is_null() {
        let token = current_user_token();
        let peer = crate::net::with_net_iface(|iface| {
            let sock: &TcpSocket = iface.sockets.get(handle);
            sock.remote_endpoint()
        }).flatten();
        if let Some(peer) = peer {
            if let IpAddress::Ipv4(ipv4) = peer.addr {
                *translated_refmut(token, addrlen as *mut usize) = core::mem::size_of::<SockAddrIn>();
                write_sockaddr_in(addr, u32::from_be_bytes(ipv4.0), peer.port);
            }
        }
    }

    new_fd as isize
}

pub fn sys_sendto(sockfd: usize, buf: *const u8, len: usize, flags: usize,
                   dest_addr: *const u8, addrlen: usize) -> isize {
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
        SocketType::Tcp => tcp_send(handle, dontwait, data),
        SocketType::Udp => {
            let (ip, port) = match parse_sockaddr_in(dest_addr) {
                Some(v) => v,
                None => return -22,
            };
            let dest = IpEndpoint::new(
                IpAddress::from(Ipv4Address::from_bytes(&ip.to_be_bytes())),
                port,
            );
            udp_send(handle, data, dest)
        }
    }
}

pub fn sys_recvfrom(sockfd: usize, buf: *mut u8, len: usize, flags: usize,
                    src_addr: *mut u8, addrlen: *mut usize) -> isize {
    let (handle, st, nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    let dontwait = nonblock || (flags & MSG_DONTWAIT) != 0;
    let peek = (flags & MSG_PEEK) != 0;

    let recv_buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };

    let result = match st {
        SocketType::Tcp => tcp_recv(handle, dontwait, recv_buf),
        SocketType::Udp => udp_recv(handle, dontwait, recv_buf),
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
                    *translated_refmut(token, addrlen as *mut usize) =
                        core::mem::size_of::<SockAddrIn>();
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
        SocketType::Tcp => tcp_send(handle, dontwait, &buf[..copied]),
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
            udp_send(handle, &buf[..copied], dest)
        }
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
        SocketType::Tcp => tcp_recv(handle, dontwait, &mut buf),
        SocketType::Udp => udp_recv(handle, dontwait, &mut buf),
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

pub fn sys_setsockopt(sockfd: usize, level: usize, optname: usize,
                       optval: *const u8, optlen: usize) -> isize {
    let (handle, st, _nonblock) = match sock_lookup(sockfd) {
        Some(v) => v,
        None => return -9,
    };

    match level {
        SOL_SOCKET => {
            match optname {
                SO_REUSEADDR => 0, // no-op: always allowed
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
        IPPROTO_TCP => {
            match optname {
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
            }
        }
        _ => 0, // silently ignore unknown levels
    }
}

pub fn sys_getsockopt(sockfd: usize, level: usize, optname: usize,
                       optval: *mut u8, optlen: *mut usize) -> isize {
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
            }).unwrap_or(false);
            *translated_refmut(token, optlen as *mut u32) = 4;
            *translated_refmut(token, optval as *mut i32) = if nagle { 1 } else { 0 };
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

    crate::net::with_net_iface(|iface| {
        let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
        match how {
            SHUT_RD | SHUT_WR | SHUT_RDWR => {
                sock.close();
            }
            _ => return -22,
        }
        0
    }).unwrap_or(-1)
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
        }).flatten(),
        SocketType::Udp => crate::net::with_net_iface(|iface| {
            let sock: &UdpSocket = iface.sockets.get(handle);
            sock.endpoint()
        }).and_then(|ep| ep.addr.map(|a| IpEndpoint::new(a, ep.port))),
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
        }).flatten(),
        SocketType::Udp => crate::net::with_net_iface(|iface| {
            let sock: &UdpSocket = iface.sockets.get(handle);
            sock.endpoint()
        }).and_then(|ep| ep.addr.map(|a| IpEndpoint::new(a, ep.port))),
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
        return crate::net::with_net_iface(|iface| {
            let sock: &mut TcpSocket = iface.sockets.get_mut(handle);
            if !sock.is_active() {
                return -32;
            }
            match sock.send_slice(data) {
                Ok(0) => -11,
                Ok(n) => n as isize,
                Err(_) => -32,
            }
        }).unwrap_or(-1);
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
        }).unwrap_or(-1);
        if sent > 0 {
            offset += sent as usize;
        } else if sent == 0 {
            if !sock_can_send_tcp(handle) {
                block_current_and_run_next(BlockReason::TcpSend(unsafe { core::mem::transmute(handle) }));
            }
        } else {
            return sent;
        }
    }
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
        }).unwrap_or(-1);
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
                block_current_and_run_next(BlockReason::TcpRecv(unsafe { core::mem::transmute(handle) }));
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
    }).unwrap_or(-1)
}

pub(crate) fn udp_write(handle: SocketHandle, data: &[u8]) -> isize {
    crate::net::net_poll();
    crate::net::with_net_iface(|iface| {
        let dest = {
            let sock: &UdpSocket = iface.sockets.get(handle);
            let ep = sock.endpoint();
            ep.addr.map(|a| IpEndpoint::new(a, ep.port))
        };
        match dest {
            Some(dest) => {
                let sock: &mut UdpSocket = iface.sockets.get_mut(handle);
                match sock.send_slice(data, dest) {
                    Ok(()) => data.len() as isize,
                    Err(_) => -11,
                }
            }
            None => -89, // EDESTADDRREQ
        }
    }).unwrap_or(-1)
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
        }).unwrap_or(-1);
    }

    loop {
        crate::net::net_poll();
        let result = crate::net::with_net_iface(|iface| {
            let sock: &mut UdpSocket = iface.sockets.get_mut(handle);
            match sock.recv_slice(buf) {
                Ok((n, _)) => Some(n as isize),
                Err(_) => None,
            }
        }).flatten();
        match result {
            Some(n) => return n,
            None => block_current_and_run_next(BlockReason::UdpRecv(unsafe { core::mem::transmute(handle) })),
        }
    }
}

fn sock_is_active_tcp(handle: SocketHandle) -> bool {
    crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.is_active()
    }).unwrap_or(false)
}

fn sock_can_send_tcp(handle: SocketHandle) -> bool {
    crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.can_send()
    }).unwrap_or(false)
}

fn sock_recv_queue_tcp(handle: SocketHandle) -> usize {
    crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.recv_queue()
    }).unwrap_or(0)
}
