use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::SocketBuffer as TcpSocketBuffer;
use smoltcp::socket::tcp::Socket as TcpSocket;
use smoltcp::socket::udp::PacketBuffer as UdpPacketBuffer;
use smoltcp::socket::udp::PacketMetadata as UdpPacketMetadata;
use smoltcp::socket::udp::Socket as UdpSocket;
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};

use crate::mm::translated_ref;
use crate::net::socket::{SocketEntry, SocketType};
use crate::task::{current_task, current_user_token};

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
