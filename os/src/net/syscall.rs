use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::SocketBuffer as TcpSocketBuffer;
use smoltcp::socket::tcp::Socket as TcpSocket;
use smoltcp::socket::udp::PacketBuffer as UdpPacketBuffer;
use smoltcp::socket::udp::PacketMetadata as UdpPacketMetadata;
use smoltcp::socket::udp::Socket as UdpSocket;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

use crate::mm::{translated_ref, translated_refmut};
use crate::net::socket::{SocketEntry, SocketType};
use crate::task::{current_task, current_user_token, suspend_current_and_run_next};

const SOCKET_BUFFER_SIZE: usize = 65536;
const UDP_META_COUNT: usize = 16;
const AF_INET: usize = 2;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;
const SOCK_NONBLOCK: usize = 0x800;

#[repr(C)]
struct SockAddrIn {
    sin_family: u16,
    sin_port: u16,
    sin_addr: u32,
    sin_zero: [u8; 8],
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

    let fd = inner.socket_table.alloc_fd();
    inner.socket_table.insert(fd, SocketEntry {
        handle,
        socket_type: st,
        nonblock,
        cloexec: false,
    });
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

    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();

    let entry = match inner.socket_table.get(sockfd) {
        Some(e) => e,
        None => return -9,
    };
    let handle = entry.handle;
    let st = entry.socket_type;
    drop(inner);

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

    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();

    let entry = match inner.socket_table.get(sockfd) {
        Some(e) => e,
        None => return -9,
    };
    if entry.socket_type != SocketType::Tcp {
        return -95; // EOPNOTSUPP
    }
    let handle = entry.handle;
    let nonblock = entry.nonblock;
    drop(inner);

    let remote = IpEndpoint::new(
        IpAddress::from(Ipv4Address::from_bytes(&ip.to_be_bytes())),
        port,
    );

    let local = IpListenEndpoint {
        addr: None,  // let smoltcp pick local address
        port: 0,     // let smoltcp pick local port
    };

    crate::net::with_net_iface_tcp_connect(handle, remote, local);

    if !nonblock {
        loop {
            crate::net::net_poll();
            let connected = crate::net::with_net_iface(|iface| {
                let sock: &TcpSocket = iface.sockets.get(handle);
                sock.is_active()
            }).unwrap_or(false);
            if connected {
                return 0;
            }
            suspend_current_and_run_next();
        }
    }
    0
}

pub fn sys_listen(sockfd: usize, _backlog: usize) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();

    let entry = match inner.socket_table.get(sockfd) {
        Some(e) => e,
        None => return -9,
    };
    if entry.socket_type != SocketType::Tcp {
        return -95;
    }
    let handle = entry.handle;
    drop(inner);

    let listening = crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.is_listening()
    }).unwrap_or(false);

    if !listening {
        return -22; // EINVAL — socket was not bound with bind()
    }
    0
}

pub fn sys_accept(sockfd: usize, addr: *mut u8, addrlen: *mut usize) -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();

    let entry = match inner.socket_table.get(sockfd) {
        Some(e) => e,
        None => return -9,
    };
    if entry.socket_type != SocketType::Tcp {
        return -95;
    }
    let handle = entry.handle;
    let nonblock = entry.nonblock;
    drop(inner);

    if !nonblock {
        let mut prev_listening = true;
        loop {
            crate::net::net_poll();
            let active = crate::net::with_net_iface(|iface| {
                let sock: &TcpSocket = iface.sockets.get(handle);
                let listening = sock.is_listening();
                let active = sock.is_active();
                if !listening && prev_listening && active {
                    // socket just transitioned from listen to active
                    return true;
                }
                prev_listening = listening;
                false
            }).unwrap_or(false);

            if active {
                break;
            }
            suspend_current_and_run_next();
        }
    }

    let active = crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.is_active()
    }).unwrap_or(false);

    if !active {
        return -11; // EAGAIN
    }

    // Create a new listening socket for the same local endpoint
    let old_local = crate::net::with_net_iface(|iface| {
        let sock: &TcpSocket = iface.sockets.get(handle);
        sock.local_endpoint()
    }).flatten();

    let mut task = current_task().unwrap();
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

    // Replace the old handle with the new listening socket
    if let Some(entry) = inner.socket_table.get_mut(sockfd) {
        entry.handle = new_handle;
    }

    // Create a new fd for the accepted (active) connection
    let new_fd = inner.socket_table.alloc_fd();
    inner.socket_table.insert(new_fd, SocketEntry {
        handle,
        socket_type: SocketType::Tcp,
        nonblock,
        cloexec: false,
    });

    // Fill peer address if requested
    if !addr.is_null() && !addrlen.is_null() {
        let token = current_user_token();
        let peer = crate::net::with_net_iface(|iface| {
            let sock: &TcpSocket = iface.sockets.get(handle);
            sock.remote_endpoint()
        }).flatten();
        if let Some(peer) = peer {
            if let IpAddress::Ipv4(ipv4) = peer.addr {
                let sa = SockAddrIn {
                    sin_family: AF_INET as u16,
                    sin_port: peer.port.to_be(),
                    sin_addr: u32::from_be_bytes(ipv4.0),
                    sin_zero: [0; 8],
                };
                *translated_refmut(token, addrlen as *mut usize) = core::mem::size_of::<SockAddrIn>();
                *translated_refmut(token, addr as *mut SockAddrIn) = sa;
            }
        }
    }

    new_fd as isize
}
