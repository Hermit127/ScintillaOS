# iperf3 网络带宽测试接入方案

## 概述

iperf3 是一个用于测量 TCP、UDP 和 SCTP 网络带宽的标准工具。本文档描述如何在本 OS 内核中实现完整的网络支持，使其能够运行 iperf3 测试。

---

## 当前状态

| 组件 | 状态 | 说明 |
|------|------|------|
| 网络驱动 | ❌ 无 | 只有 `virtio-blk`（块设备），无 `virtio-net` |
| TCP/IP 协议栈 | ❌ 无 | 未引入 `smoltcp` 等协议栈 crate |
| Socket 系统调用 | ❌ 无 | `syscall_ids.h` 定义了编号，内核未实现任何 socket 相关 |
| QEMU 网络配置 | ❌ 无 | Makefile 中无 `-netdev` / `-device virtio-net` 参数 |
| 网络相关依赖 | ❌ 无 | `Cargo.toml` 无 `smoltcp` 等依赖 |

当前唯一可用的驱动栈是 `virtio-blk`（块设备），通过 `virtio_drivers` crate 的 `MmioTransport` 访问。该 crate 也内置了 `VirtIONet`，传输层可直接复用。ext4 文件系统和进程管理已成熟，可作为网络功能的底层支撑。

---

## 目标架构

iperf3 在 OS 内核中的完整调用链，从上到下六层：

```
┌──────────────────────────────────────────────────────┐
│  用户空间层                                           │
│  iperf3 (musl-libc static binary)                    │
│    → socket / bind / connect / sendmsg / recvmsg      │
│    → poll / clock_gettime / getrandom                 │
│                       ↓ RISC-V ecall                   │
├──────────────────────────────────────────────────────┤
│  系统调用层 (~20 个新增)                               │
│  socket(198)   bind(200)     connect(203)              │
│  listen(201)   accept(202)   sendto(206)  recvfrom(207)│
│  sendmsg(211)  recvmsg(212)  shutdown(210)             │
│  setsockopt(208)  poll(73)  clock_gettime(113)         │
│  getrandom(278)                                           │
├──────────────────────────────────────────────────────┤
│  Socket 管理层                                         │
│  SocketTable (per-task)，独立于 fd_table             │
│  TcpSocket / UdpSocket 封装 smoltcp 的 socket 抽象      │
│  阻塞 I/O 通过 TaskStatus::Blocking + 事件唤醒          │
├──────────────────────────────────────────────────────┤
│  协议栈层 (smoltcp)                                    │
│  TCP / UDP / ICMP / DHCP / ARP                         │
│  Interface polling loop，驱动协议栈状态机前进            │
├──────────────────────────────────────────────────────┤
│  驱动层                                                │
│  VirtIO-Net 驱动，实现 smoltcp::phy::Device trait       │
│  MmioTransport 传输层（与 virtio-blk 共用）             │
│  网卡中断处理                                           │
├──────────────────────────────────────────────────────┤
│  虚拟化层                                              │
│  QEMU virtio-net-device + SLIRP 用户态网络              │
│  Host 端口转发 (hostfwd) 实现内外通信                   │
└──────────────────────────────────────────────────────┘
```

---

## Phase 1：网络驱动 + 协议栈初始化

> **目标**：网卡能够收发以太网帧，协议栈能通过 DHCP 获取 IP 地址，Host 能 ping 通 Guest。

### 1.1 引入 smoltcp 依赖

在 `os/Cargo.toml` 中添加 `smoltcp` crate，版本 0.11。启用以下 feature：

- `alloc`、`log`：内核环境基础支持
- `medium-ip`、`medium-ethernet`：IP 层 + 以太网层
- `socket-tcp`、`socket-udp`、`socket-icmp`：传输层 socket
- `proto-ipv4`、`proto-dhcpv4`：IPv4 + DHCP 客户端

### 1.2 实现 VirtIO-Net 驱动

**模块结构**：新增 `os/src/drivers/net/` 目录，包含 `mod.rs` 和 `virtio_net.rs`。

**核心要点**：

- 封装 `virtio_drivers::device::net::VirtIONet`，与现有的 `VirtIOBlk` 使用相同的 `MmioTransport` 传输层
- MMIO 地址暂定 `0x10003000`（riscv64 virtio-mmio-bus.2），后续可从设备树动态探测
- 实现 `smoltcp::phy::Device` trait：
  - `receive()`：从 virtio 接收队列取出数据包，包装为 RxToken
  - `transmit()`：返回一个 TxToken 供 smoltcp 写入待发送数据
  - `capabilities()`：声明 MTU=1500，介质类型 Ethernet
- **注意**：smoltcp 的 RxToken/TxToken 生命周期管理较复杂——token 持有缓冲区的可变引用，需要使用 `UPSafeCell` 或静态分配来管理缓冲区所有权

**中断处理**：

- 在 `kernel_interrupt` 中识别 virtio-net 的中断号（需从设备树或 QEMU 固定分配获取）
- 中断处理中调用 `ack_interrupt()` 确认中断，并设置标志位触发协议栈轮询

### 1.3 网络栈初始化

新增 `os/src/net/mod.rs` 作为网络子系统入口。

**初始化步骤**：

1. 创建 `VirtIONetAdapter` 实例
2. 创建 `smoltcp::iface::Interface`，配置 MAC 地址（可使用固定值如 `52:54:00:12:34:56`）
3. 添加 DHCP socket 到 `SocketSet`，自动获取 IP 地址
4. 配置默认路由（SLIRP 网络下网关固定为 `10.0.2.2`）

**全局状态**：使用 `lazy_static!` 创建全局 `NET_IFACE`，类型为 `UPSafeCell<NetworkState>`。`NetworkState` 包含：设备实例、Interface、SocketSet、时钟计数器。

**静态 IP 作为 fallback**：如果 DHCP 失败，使用 SLIRP 默认网段 `10.0.2.15/24`。

### 1.4 协议栈轮询

smoltcp 是事件驱动的，需要定期调用 `iface.poll()` 来推动协议状态机。有两种触发方式：

- **时钟中断驱动**：每次定时器中断（如每 10ms）调用一次 `poll()`。实现简单，但延迟较高
- **独立轮询**：在调度循环中高频调用 `poll()`，或使用网卡中断触发即时轮询。延迟低，但 CPU 占用高

推荐初始使用时钟中断驱动，后续可优化为中断 + 时钟的组合。

每次 `poll()` 后需要检查 socket 状态变化，唤醒因阻塞 I/O 而挂起的任务。

### 1.5 QEMU 网络配置

在 Makefile 中添加 SLIRP 用户态网络参数：

- `-netdev user,id=net0,hostfwd=tcp::5555-:5001`：创建 SLIRP 网络后端，将 Host 的 5555 端口转发到 Guest 的 5001 端口（iperf3 默认端口）
- `-device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2`：将 virtio-net 设备挂载到 MMIO bus 2

**SLIRP 网络说明**：

- 无需 root 权限
- Guest 默认 IP 范围 `10.0.2.0/24`，Guest 地址 `10.0.2.15`，网关 `10.0.2.2`，DNS `10.0.2.3`
- 性能受限于 QEMU 用户态网络栈，单流吞吐约 100-500 Mbps

**验证方式**：

- 内核启动日志中确认 DHCP 获取到 IP 和网关
- Host 执行 `ping 10.0.2.15` 验证 ICMP 可达
- Host 执行 `nc -v 127.0.0.1 5555` 验证 TCP 端口转发

---

## Phase 2：Socket 系统调用实现

> **目标**：用户程序可以通过标准 POSIX socket API 进行 TCP/UDP 通信，echo server 可以运行。

### 2.1 Socket 表数据结构

新增 `os/src/net/socket.rs`，核心数据结构：

**`SocketTable`**：每个 Task 的 socket 描述符表，使用 `BTreeMap<usize, SocketEntry>`。socket fd 与文件 fd 使用独立的命名空间，互不冲突。

**`SocketEntry`**：包含 `Socket` 枚举、`nonblock` 标志、`cloexec` 标志。

**`Socket` 枚举**：`Tcp(TcpSocket)` / `Udp(UdpSocket)`，封装 smoltcp 的原生 socket 类型。

**socket 生命周期管理**：smoltcp 要求所有活跃 socket 注册到全局 `SocketSet`。创建 socket 时通过 `SocketSet::add()` 获取 `SocketHandle`，后续操作通过 handle 索引访问。关闭 socket 时从 `SocketSet` 中移除。

### 2.2 与 fd_table 的关系

socket fd 与文件 fd **完全独立管理**，互不干扰：

- 文件 fd 由 `fd_table: Vec<Option<Arc<dyn File>>>` 管理（保持不变）
- socket fd 由 `socket_table: SocketTable` 管理（新增字段）
- 两者使用独立编号空间，fd 编号可重复但不冲突——具体属于哪个表由系统调用类型决定

需要同时处理两种 fd 的系统调用仅有 `close()`。修改 `sys_close` 使其先尝试 `fd_table`，失败时回退到 `socket_table`。未来实现 `poll()`/`ppoll()` 时同样遍历两个表。

### 2.3 核心 Socket 系统调用

以下按 iperf3 实际使用的调用路径列出：

#### 创建与绑定

| syscall | 编号 | 功能 |
|---------|------|------|
| `socket` | 198 | 创建 socket。domain 仅支持 `AF_INET`(2)，type 支持 `SOCK_STREAM`(1)/`SOCK_DGRAM`(2) |
| `bind` | 200 | 绑定本地地址和端口。iperf3 服务端调用，绑定到 `0.0.0.0:5001` |

#### TCP 连接

| syscall | 编号 | 功能 |
|---------|------|------|
| `connect` | 203 | 发起 TCP 连接。iperf3 客户端调用，阻塞等待三次握手完成 |
| `listen` | 201 | 标记 socket 为监听状态，设置 backlog 队列大小 |
| `accept` | 202 | 接受新连接，返回新的 socket fd。阻塞直到有连接到达 |

#### 数据收发

| syscall | 编号 | 功能 |
|---------|------|------|
| `sendmsg` | 211 | 通用发送，支持 `struct msghdr`（iperf3 主要使用此调用） |
| `recvmsg` | 212 | 通用接收，同上 |
| `sendto` | 206 | UDP 发送，指定目标地址 |
| `recvfrom` | 207 | UDP 接收，获取来源地址 |

**msghdr 结构解析**：

```
struct msghdr {
    void         *msg_name;      // 目标地址（对于 TCP 为 NULL）
    socklen_t     msg_namelen;
    struct iovec *msg_iov;       // 数据缓冲区数组
    size_t        msg_iovlen;    // iov 数组长度
    void         *msg_control;   // 辅助数据（可忽略）
    size_t        msg_controllen;
    int           msg_flags;
};
```

iperf3 使用 `sendmsg`/`recvmsg` 而非 `send`/`recv`，因此这两个调用为必须实现项。

#### 选项与关闭

| syscall | 编号 | 功能 | iperf3 使用 |
|---------|------|------|-----------|
| `setsockopt` | 208 | 设置 socket 选项 | 设置 `TCP_NODELAY`（禁用 Nagle） |
| `getsockopt` | 209 | 获取 socket 选项 | — |
| `shutdown` | 210 | 半关闭连接 | 优雅关闭 TCP 连接 |
| `getpeername` | 205 | 获取对端地址 | 日志输出 |
| `getsockname` | 204 | 获取本地地址 | 日志输出 |

iperf3 通过 `setsockopt` 设置的关键选项：

- **`TCP_NODELAY`**：禁用 Nagle 算法，iperf3 带宽测试必需（否则小包会被合并，降低测量精度）
- **`SO_REUSEADDR`**：允许重用 `TIME_WAIT` 状态的地址，服务端重启时必需
- **`SO_RCVBUF` / `SO_SNDBUF`**：调整收发缓冲区大小

### 2.4 阻塞 I/O 实现

iperf3 使用阻塞 socket（默认模式），需要实现阻塞语义：

**流程**：

1. 用户调用 `recvmsg()` → 内核检查 socket 接收缓冲区
2. 如果有数据 → 立即拷贝到用户空间，返回
3. 如果无数据且非阻塞标志未设置 → 设置 `TaskStatus::Blocking`，记录阻塞原因（等待该 socket 可读），调用 `schedule()` 让出 CPU
4. 网络轮询循环检测到 socket 变为可读 → 设置 `TaskStatus::Ready`
5. 下次调度到该任务时，`recvmsg` 重试读取

**`TaskControlBlockInner` 新增字段**：`block_reason: Option<BlockReason>`，枚举值包括 `SocketRead(fd)`、`SocketWrite(fd)`、`SocketAccept(fd)`。

**唤醒机制**：每次 `NET_IFACE.poll()` 之后，遍历所有阻塞任务，检查其等待的 socket 是否已就绪。就绪则恢复为 `Ready` 状态。

---

## Phase 3：iperf3 编译与附加系统调用

> **目标**：iperf3 二进制能在 OS 上成功启动、建立 TCP 连接。

### 3.1 iperf3 所需的非 socket 系统调用

| syscall | 编号 | 当前状态 | 作用 |
|---------|------|---------|------|
| `clock_gettime` | 113 | ❌ 未实现 | iperf3 带宽计算核心——使用 `CLOCK_MONOTONIC` 获取高精度时间 |
| `getrandom` | 278 | ❌ 未实现 | 生成随机测试数据（避免全零数据触发压缩） |
| `ppoll` | 73 | ❌ 未实现 | I/O 多路复用，等待多个 socket 就绪 |
| `writev` | 66 | ❌ 未实现 | 聚集写（iperf3 可能用于组合 header + payload） |
| `futex` | 98 | ❌ 未实现 | musl libc 同步原语底层实现，至少需要 stub |
| `madvise` | 233 | ❌ 未实现 | stub 返回 0 即可 |
| `sched_yield` | 124 | ✅ 已实现 | 让出 CPU |
| `exit_group` | 94 | ✅ 已实现 | 进程退出 |
| `brk` | 214 | ✅ 已实现 | 堆内存分配 |
| `mmap` / `munmap` | 222/215 | ✅ 已实现 | 内存映射 |

### 3.2 clock_gettime (113)

iperf3 测量带宽的核心依赖。它使用 `clock_gettime(CLOCK_MONOTONIC, &ts)` 获取纳秒级时间戳来计算传输速率。

**实现要点**：从 `polyhal::time::Time::now()` 获取当前时间，转换为 `struct timespec { tv_sec, tv_nsec }` 格式写入用户空间。支持 `CLOCK_REALTIME`(0) 和 `CLOCK_MONOTONIC`(1) 两种时钟类型。

### 3.3 getrandom (278)

iperf3 使用随机数据作为测试负载（而非全零），以模拟真实流量并避免链路压缩干扰测量结果。

**实现要点**：可使用简单的 PRNG（如 xorshift）或 SBI RNG 扩展。对于 iperf3 测试，伪随机数足够——重点是避免全零和重复模式。

### 3.4 ppoll (73)

iperf3 主事件循环使用 `poll()` 同时监听多个 socket 的读写事件。musl libc 将 `poll()` 实现为对 `ppoll()` 的包装。

**实现要点**：接收 `struct pollfd` 数组，检查每个 fd 是否就绪（可读/可写/错误）。如果所有 fd 都未就绪且 timeout > 0，挂起当前任务。timeout = 0 立即返回，timeout = -1 无限等待。

### 3.5 futex (98) stub

musl libc 内部使用 `futex` 实现线程同步。当前内核无多线程支持，busybox/iperf3 均为单线程运行。`futex` 需要一个基本的 stub：

- `FUTEX_WAIT` 返回 `-ENOSYS`（musl 会 fallback 到 busy-wait）
- `FUTEX_WAKE` 返回 `0`（无人等待）

### 3.6 交叉编译 iperf3

使用 `riscv64-linux-musl-gcc` 工具链（静态链接）：

- 下载 iperf3 源码（GitHub: esnet/iperf）
- 配置 `--host=riscv64-linux-musl --disable-shared --enable-static`
- 编译产出 `src/iperf3`（静态 ELF 二进制）
- 将二进制放入 ext4 文件系统镜像 `/bin/iperf3`

如果需要避免 pthreads 依赖（内核当前无线程支持），编译 iperf3 时指定 `--without-pthread` 以单线程模式运行——iperf3 在单线程下仍可正常测量 TCP/UDP 带宽，仅失去 `-P` 多并行流功能。

---

## Phase 4：集成测试与性能优化

> **目标**：iperf3 全功能验证通过，性能达到竞赛要求。

### 4.1 测试流程

#### Step 1：连通性验证

- 内核启动，确认 DHCP 获取 IP 成功，日志输出 IP 和网关
- Host 执行 `ping 10.0.2.15`，验证 ICMP echo reply
- Host 执行 `nc -v 127.0.0.1 5555`（通过 QEMU 端口转发），验证 TCP 端口可达

#### Step 2：TCP echo 功能测试

编写简单的 Rust 用户程序 `echoserver`，绑定 5001 端口，回显接收到的数据。Host 使用 `nc` 连接验证双向通信正确。

#### Step 3：iperf3 基本功能

- **服务端模式**（OS 内）：`/bin/iperf3 -s -p 5001`
- **客户端模式**（Host）：`iperf3 -c 127.0.0.1 -p 5555 -t 10`

验证输出包含正常的带宽、传输量、重传次数等信息。

#### Step 4：性能压力测试

- TCP 单流 30 秒测试
- UDP 固定带宽测试（`-u -b 100M`）
- 反向测试（OS 做客户端：`/bin/iperf3 -c 10.0.2.2 -R`）

### 4.2 性能优化项

| 优化项 | 方法 | 预期收益 |
|--------|------|---------|
| 禁用 Nagle 算法 | `TCP_NODELAY` socket 选项 | 消除小包延迟，提高带宽测量精度 |
| 增大 socket 缓冲区 | `SO_RCVBUF` / `SO_SNDBUF` > 256KB | 提高吞吐上限，匹配 BDP |
| 中断合并 | virtio-net `VIRTIO_NET_F_EVENT_IDX` 特性 | 减少中断开销，降低 CPU 占用 |
| 高频轮询 | 增短 `iface.poll()` 间隔（如 1ms） | 降低收发延迟 |
| 零拷贝 | 大块数据直接从用户页映射到 DMA 缓冲区 | 减少内核内存拷贝 |

### 4.3 预期性能

在 QEMU SLIRP 用户态网络下（单核 RISC-V，无硬件卸载）：

| 指标 | 基准值 | 优化后 | 备注 |
|------|--------|--------|------|
| TCP 单流吞吐 | ~50 Mbps | ~200-500 Mbps | 上限受 QEMU SLIRP 限制 |
| UDP 吞吐 | ~100 Mbps | ~500 Mbps | 无拥塞控制开销 |
| 连接建立延迟 | ~1-5 ms | ~0.5-2 ms | 全虚拟网络 |
| CPU 占用率 | ~100% (busy) | ~30-50% | 中断驱动优化 |

真实性能瓶颈在 QEMU 用户态网络（SLIRP），而非内核协议栈效率。如需更高吞吐，可改用 TAP 网络 + vhost 加速。

---

## 系统调用完整清单

### 需要新增的系统调用

| 阶段 | syscall | 编号 | 实现难度 | 说明 |
|------|---------|------|---------|------|
| Phase 2 | `socket` | 198 | ★★☆ | 创建 socket |
| Phase 2 | `bind` | 200 | ★★☆ | 绑定地址 |
| Phase 2 | `connect` | 203 | ★★★ | TCP 连接 + 阻塞等待 |
| Phase 2 | `listen` | 201 | ★☆☆ | TCP 监听 |
| Phase 2 | `accept` | 202 | ★★★ | TCP 接受连接 + 阻塞等待 |
| Phase 2 | `sendto` | 206 | ★★☆ | UDP 发送 |
| Phase 2 | `recvfrom` | 207 | ★★☆ | UDP 接收 |
| Phase 2 | `sendmsg` | 211 | ★★★ | 通用发送（iperf3 主用） |
| Phase 2 | `recvmsg` | 212 | ★★★ | 通用接收（iperf3 主用） |
| Phase 2 | `shutdown` | 210 | ★★☆ | 连接关闭 |
| Phase 2 | `setsockopt` | 208 | ★★★ | 设置 socket 选项 |
| Phase 2 | `getsockopt` | 209 | ★★☆ | 获取 socket 选项 |
| Phase 2 | `getpeername` | 205 | ★☆☆ | 获取对端地址 |
| Phase 2 | `getsockname` | 204 | ★☆☆ | 获取本地地址 |
| Phase 3 | `clock_gettime` | 113 | ★★☆ | 高精度计时 |
| Phase 3 | `getrandom` | 278 | ★★☆ | 随机数生成 |
| Phase 3 | `ppoll` | 73 | ★★★ | I/O 多路复用 |
| Phase 3 | `writev` | 66 | ★★☆ | 聚集写 |
| Phase 3 | `futex` | 98 | ★☆☆ | stub 即可 |
| Phase 3 | `madvise` | 233 | ★☆☆ | stub 即可 |

**总计：20 个新系统调用**（14 个 socket 相关 + 6 个辅助）

### 已实现的系统调用（无需改动）

| syscall | 编号 | 作用 |
|---------|------|------|
| `sched_yield` | 124 | 让出 CPU |
| `exit_group` | 94 | 进程退出 |
| `gettid` | 178 | 获取线程 ID |
| `nanosleep` | 101 | 纳秒级睡眠 |
| `brk` | 214 | 堆内存分配 |
| `mmap` | 222 | 内存映射 |
| `munmap` | 215 | 解除映射 |
| `read` | 63 | 文件读取 |
| `write` | 64 | 文件写入 |
| `close` | 57 | 关闭 fd（需扩展支持 socket fd） |
| `openat` | 56 | 文件打开 |

---

## 数据结构变更汇总

### TaskControlBlockInner 新增字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `socket_table` | `SocketTable` | socket fd 管理表（独立于 `fd_table`） |
| `block_reason` | `Option<BlockReason>` | 阻塞原因（等待 socket 可读/可写/可接受） |

### fd_table 保持不变

文件 fd 管理完全不动——`fd_table: Vec<Option<Arc<dyn File>>>` 维持原样，所有现有文件系统调用代码不受影响。socket fd 通过新增的 `socket_table` 独立管理，只在 `close()` 和未来的 `poll()` 中需要同时查询两个表。

### 新增模块

```
os/src/
├── net/
│   ├── mod.rs              # 网络栈初始化 + 全局 NET_IFACE 状态
│   ├── socket.rs           # SocketTable / SocketEntry / Socket 枚举
│   └── syscall.rs          # socket 系列系统调用实现
├── drivers/
│   └── net/
│       ├── mod.rs
│       └── virtio_net.rs   # VirtIO-Net 驱动
```

### 新增全局静态

| 变量 | 类型 | 说明 |
|------|------|------|
| `NET_IFACE` | `UPSafeCell<NetworkState>` | 全局网络状态（设备、Interface、SocketSet） |
| `NET_POLL_INTERVAL_MS` | `u64` | 协议栈轮询间隔（默认 10ms） |

---

## 实现阶段总览

```
Phase 1: 驱动 + 协议栈（2-3 天）
  ├─ smoltcp 依赖引入
  ├─ VirtIO-Net 驱动 + smoltcp::phy::Device trait 实现
  ├─ 网络栈初始化（Interface + DHCP + 路由）
  ├─ 中断注册 + 协议栈轮询循环
  ├─ QEMU 网络参数配置（SLIRP + hostfwd 端口转发）
  └─ 验证: DHCP 获取 IP，Host ping Guest 成功

Phase 2: Socket 系统调用（4-5 天）
  ├─ SocketTable 数据结构（独立于 fd_table）
  ├─ socket / bind / connect 实现
  ├─ listen / accept 实现（TCP 服务端）
  ├─ sendmsg / recvmsg 实现（iperf3 主 I/O 路径）
  ├─ sendto / recvfrom 实现（UDP 支持）
  ├─ setsockopt / shutdown 实现
  ├─ 阻塞 I/O 实现（BlockReason + 事件唤醒）
  └─ 验证: TCP echo server 用户程序，双向通信正确

Phase 3: 辅助 syscall + iperf3 编译（2-3 天）
  ├─ clock_gettime(113) 实现
  ├─ getrandom(278) 实现
  ├─ ppoll(73) 实现
  ├─ writev(66) 实现
  ├─ futex(98) / madvise(233) stub
  ├─ 交叉编译 iperf3 (riscv64-linux-musl, static)
  └─ 验证: /bin/iperf3 -s 能启动并监听 5001 端口

Phase 4: 集成测试 + 性能优化（2-3 天）
  ├─ TCP 全双工通信测试
  ├─ iperf3 基本带宽测试（TCP/UDP）
  ├─ 性能分析：TCP_NODELAY / 缓冲区 / 中断合并
  ├─ 多场景测试（反向、多流、长时间运行）
  └─ 验证: iperf3 输出正常带宽数据，无异常断开

总计: 10-14 天
```

---

## 关键技术决策

| 决策点 | 选择 | 理由 |
|--------|------|------|
| 协议栈 | smoltcp | Rust no_std 原生支持，生态最成熟，社区活跃 |
| 网络驱动 | virtio-net (MMIO) | 复用现有 `virtio_drivers` 库，与 virtio-blk 同源 |
| 虚拟网络模式 | QEMU SLIRP | 无需 root，port forwarding 即可实现 Host ↔ Guest 通信 |
| IP 配置 | DHCP + 静态 fallback | smoltcp 内置 DHCP 客户端，兜底静态 `10.0.2.15/24` |
| 阻塞 I/O 实现 | TaskStatus::Blocking + 轮询唤醒 | 复用现有调度器机制，无额外线程开销 |
| fd 编号策略 | 文件 fd 与 socket fd 独立管理 | `close()`/`poll()` 中先查 `fd_table`，再查 `socket_table` |

---

## 风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|---------|
| smoltcp 生命周期管理 | 高：RxToken/TxToken 的 borrow 约束与 UPSafeCell 交互复杂 | 使用静态分配的缓冲区池，参考 rCore 等成熟实现 |
| QEMU SLIRP 性能低 | 中：吞吐受限，可能不满足竞赛要求 | 预留 TAP 网络方案（需 root），配合 vhost 加速 |
| futex 缺失导致 musl 异常 | 中：musl 同步原语依赖 futex(98) | 至少提供 futex stub；iperf3 使用 `--without-pthread` 编译 |
| 设备中断号不确定 | 低：不同 QEMU 版本可能分配不同 IRQ | 从设备树解析，或固定使用已知地址轮询 |
| fd 编号重叠 | 低：`close()`/`poll()` 需处理两个表 | 先查 `fd_table`，再查 `socket_table`，按系统调用类型自然分流 |
| iperf3 二进制兼容性 | 低：musl 交叉编译可能遇到符号缺失 | 使用 `--disable-shared --enable-static` 确保全静态链接 |

---

## 参考资料

- [smoltcp 官方文档](https://docs.rs/smoltcp/latest/smoltcp/)
- [virtio-drivers crate](https://crates.io/crates/virtio-drivers)
- [iperf3 源码与文档](https://github.com/esnet/iperf)
- [rCore-Tutorial 网络栈章节](https://rcore-os.cn/rCore-Tutorial-Book-v3/chapter7/index.html)
- [RISC-V Linux syscall ABI 完整列表](https://git.musl-libc.org/cgit/musl/tree/arch/riscv64/bits/syscall.h.in)
- [QEMU 网络配置指南](https://wiki.qemu.org/Documentation/Networking)
- [VirtIO 规范 v1.2](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html)
