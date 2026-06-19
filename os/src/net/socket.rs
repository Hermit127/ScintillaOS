// 这里负责操作系统的网络文件接口：把 smoltcp socket 包装成内核 File trait 可管理的 fd 对象。
use core::sync::atomic::{AtomicBool, Ordering};
use smoltcp::iface::SocketHandle;
use smoltcp::wire::IpEndpoint;

use crate::fs::{File, Kstat};
use crate::sync::UPSafeCell;

pub const S_IFSOCK: u32 = 0o140000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketType {
    Tcp,
    Udp,
}

pub struct SocketFile {
    pub handle: UPSafeCell<SocketHandle>,
    pub socket_type: SocketType,
    pub nonblock: AtomicBool,
    ref_count: UPSafeCell<usize>,
    pub peer_endpoint: UPSafeCell<Option<IpEndpoint>>,
    pub local_endpoint: UPSafeCell<Option<IpEndpoint>>,
    pub local_tcp: UPSafeCell<Option<(usize, bool)>>,
    pub was_listening: AtomicBool,
}

impl SocketFile {
    // 创建一个 socket 文件对象，保存 smoltcp handle 和阻塞模式。
    // 执行时初始化 fd 层需要的引用计数、本地/对端端点缓存，以及本地 TCP 回环状态。
    pub fn new(handle: SocketHandle, socket_type: SocketType, nonblock: bool) -> Self {
        Self {
            handle: unsafe { UPSafeCell::new(handle) },
            socket_type,
            nonblock: AtomicBool::new(nonblock),
            ref_count: unsafe { UPSafeCell::new(1) },
            peer_endpoint: unsafe { UPSafeCell::new(None) },
            local_endpoint: unsafe { UPSafeCell::new(None) },
            local_tcp: unsafe { UPSafeCell::new(None) },
            was_listening: AtomicBool::new(false),
        }
    }

    // 读取当前 smoltcp socket handle。
    // 执行时进入 UPSafeCell 取出 handle 的拷贝，避免外层长期持有内部可变借用。
    pub fn get_handle(&self) -> SocketHandle {
        *self.handle.exclusive_access()
    }

    // 更新 smoltcp socket handle，accept/connect 后可能会替换。
    // 执行时用 UPSafeCell 独占写入新 handle，让 fd 继续指向正确的 smoltcp socket。
    pub fn set_handle(&self, handle: SocketHandle) {
        *self.handle.exclusive_access() = handle;
    }

    // 查询 O_NONBLOCK 状态。
    // 执行时从原子布尔读取当前非阻塞标志，读写路径据此决定返回 EAGAIN 还是挂起。
    pub fn get_nonblock(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    // 读取记录下来的对端地址。
    // 执行时返回 fd 层缓存的 peer endpoint，UDP 默认对端和本地 TCP 回环都会用到。
    pub fn get_peer_endpoint(&self) -> Option<IpEndpoint> {
        *self.peer_endpoint.exclusive_access()
    }

    // 记录 socket 对端地址。
    // 执行时写入 peer endpoint 缓存，connect/sendto 后的后续读写可以直接复用。
    pub fn set_peer_endpoint(&self, ep: IpEndpoint) {
        *self.peer_endpoint.exclusive_access() = Some(ep);
    }

    // 读取记录下来的本地地址。
    // 执行时返回 fd 层缓存的 local endpoint，用于 getsockname、UDP 收包和本地回环匹配。
    pub fn get_local_endpoint(&self) -> Option<IpEndpoint> {
        *self.local_endpoint.exclusive_access()
    }

    // 记录 socket 本地地址。
    // 执行时保存 bind 或自动分配出的本地端点，避免每次都从 smoltcp socket 里反查。
    pub fn set_local_endpoint(&self, ep: IpEndpoint) {
        *self.local_endpoint.exclusive_access() = Some(ep);
    }

    // 读取本地 TCP 回环连接编号和当前端角色。
    // 执行时返回连接 id 和是否服务端这一对状态，用来选择 c2s 或 s2c 队列。
    pub fn get_local_tcp(&self) -> Option<(usize, bool)> {
        *self.local_tcp.exclusive_access()
    }

    // 标记这个 socket 已经接到本地 TCP 回环连接上。
    // 执行时把本地 TCP 连接 id 记录进 fd，对后续 read/write/close 走内核模拟队列。
    pub fn set_local_tcp(&self, conn_id: usize, server_side: bool) {
        *self.local_tcp.exclusive_access() = Some((conn_id, server_side));
    }
}

impl File for SocketFile {
    // socket fd 支持 read。
    // 执行时直接返回这个文件对象是否允许读取，不移动 offset，也不访问底层设备。
    fn readable(&self) -> bool {
        true
    }

    // socket fd 支持 write。
    // 执行时直接返回这个文件对象是否允许写入，不修改文件状态。
    fn writable(&self) -> bool {
        true
    }

    // 根据 TCP/UDP 类型从对应接收路径读取数据。
    // 执行时先取非阻塞标志和 handle；TCP 优先走本地回环连接，UDP 则带上缓存端点做收包匹配。
    fn read(&self, buf: &mut [u8]) -> isize {
        let nonblock = self.get_nonblock();
        let handle = self.get_handle();
        match self.socket_type {
            SocketType::Tcp => {
                if let Some((conn_id, server_side)) = self.get_local_tcp() {
                    super::syscall::local_tcp_recv(conn_id, server_side, nonblock, buf)
                } else {
                    super::syscall::tcp_recv(handle, nonblock, buf)
                }
            }
            SocketType::Udp => super::syscall::udp_recv_with_endpoints(
                handle,
                nonblock,
                buf,
                self.get_local_endpoint(),
                self.get_peer_endpoint(),
            ),
        }
    }

    // 根据 TCP/UDP 类型把数据送到对应发送路径。
    // 执行时把可变 buf 当作只读数据；TCP 本地连接写入对端队列，UDP 则按默认对端发送。
    fn write(&self, buf: &mut [u8]) -> isize {
        let nonblock = self.get_nonblock();
        let handle = self.get_handle();
        let data: &[u8] = buf;
        let result = match self.socket_type {
            SocketType::Tcp => {
                if let Some((conn_id, server_side)) = self.get_local_tcp() {
                    super::syscall::local_tcp_send(conn_id, server_side, nonblock, data)
                } else {
                    super::syscall::tcp_send(handle, nonblock, data)
                }
            }
            SocketType::Udp => super::syscall::udp_write_with_endpoint(
                handle,
                data,
                self.get_local_endpoint(),
                self.get_peer_endpoint(),
            ),
        };
        result
    }

    // socket 在 stat 里表现为 S_IFSOCK。
    // 执行时按这个对象当前状态组装 Kstat，必要时读取底层元数据，最后返回给系统调用层。
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFSOCK)
    }

    // 返回 O_RDWR/O_LARGEFILE 以及当前 O_NONBLOCK。
    // 执行时根据当前打开状态重新拼出用户态可见的 flag 位，最后返回给 fcntl 等调用。
    fn get_flags(&self) -> usize {
        const O_NONBLOCK: usize = 0x800;
        let mut flags = 0x8000 | 2; // O_LARGEFILE | O_RDWR
        if self.get_nonblock() {
            flags |= O_NONBLOCK;
        }
        flags
    }

    // 目前只从 fcntl 动态更新 O_NONBLOCK。
    // 执行时从用户传入的 flag 中挑出当前支持修改的位，写回文件对象内部状态。
    fn set_flags(&self, flags: usize) -> bool {
        const O_NONBLOCK: usize = 0x800;
        self.nonblock
            .store((flags & O_NONBLOCK) != 0, Ordering::Relaxed);
        true
    }

    // dup socket fd 时增加引用计数。
    // 执行时只递增 fd 层引用计数，最后一个引用关闭时才清理本地 TCP/UDP 状态。
    fn dup_ref(&self) {
        *self.ref_count.exclusive_access() += 1;
    }

    // 最后一个 fd 关闭时清理本地 TCP/UDP 状态。
    // 执行时先找到要关闭的对象，再释放引用或清理队列状态，最后返回关闭结果。
    fn close_ref(&self) {
        let should_close = {
            let mut ref_count = self.ref_count.exclusive_access();
            if *ref_count > 0 {
                *ref_count -= 1;
            }
            *ref_count == 0
        };
        if !should_close {
            return;
        }

        if let Some((conn_id, server_side)) = self.get_local_tcp() {
            super::syscall::local_tcp_close(conn_id, server_side);
        } else if self.socket_type == SocketType::Udp {
            if let Some(local) = self.get_local_endpoint() {
                super::syscall::local_udp_close(local, self.get_peer_endpoint());
            }
        } else if self.socket_type == SocketType::Tcp && self.was_listening.load(Ordering::Relaxed)
        {
            if let Some(local) = self.get_local_endpoint() {
                super::syscall::local_tcp_unlisten(local.port);
            }
        }
    }

    // 查询 socket 当前可读、可写和异常状态。
    // 执行时先处理本地 TCP 的队列状态，再查询 smoltcp socket，按 poll 需要返回读/写/异常三元组。
    fn poll(&self) -> (bool, bool, bool) {
        if self.socket_type == SocketType::Tcp {
            if let Some((conn_id, server_side)) = self.get_local_tcp() {
                return super::syscall::local_tcp_poll(conn_id, server_side);
            }
            if let Some(local) = self.get_local_endpoint() {
                if super::syscall::local_tcp_pending_on(local.port) {
                    return (true, true, false);
                }
            }
        }

        #[cfg(any(
            target_arch = "riscv64",
            target_arch = "aarch64",
            target_arch = "loongarch64"
        ))]
        {
            use smoltcp::socket::tcp::Socket as TcpSocket;
            use smoltcp::socket::udp::Socket as UdpSocket;
            let handle = self.get_handle();
            crate::net::with_net_iface(|iface| match self.socket_type {
                SocketType::Tcp => {
                    let sock: &TcpSocket = iface.sockets.get(handle);
                    let has_pending = sock.is_active()
                        && !sock.is_listening()
                        && self.was_listening.load(Ordering::Relaxed);
                    if self.was_listening.load(Ordering::Relaxed) {
                        (has_pending, true, false)
                    } else {
                        (
                            sock.can_recv() || !sock.may_recv(),
                            sock.can_send() || !sock.is_active(),
                            !sock.is_active() && !sock.is_listening() && sock.recv_queue() == 0,
                        )
                    }
                }
                SocketType::Udp => {
                    let sock: &UdpSocket = iface.sockets.get(handle);
                    let local_pending = self
                        .get_local_endpoint()
                        .map(|ep| super::syscall::local_udp_pending_on(ep.port))
                        .unwrap_or(false);
                    (sock.can_recv() || local_pending, true, false)
                }
            })
            .unwrap_or((false, false, false))
        }
        #[cfg(not(any(
            target_arch = "riscv64",
            target_arch = "aarch64",
            target_arch = "loongarch64"
        )))]
        {
            (false, false, false)
        }
    }

    // 给 fd 层做具体类型判断。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
