# `iperf3` 运行失败：操作系统底层缺陷诊断与修复指南

本文档总结了当前操作系统底层代码中导致 `iperf3` 等复杂网络程序无法正常运行的几个致命系统调用（Syscall）设计缺陷和实现遗漏。

## 🚨 核心问题分析

### 1. 文件描述符（FD）命名空间冲突（最致命错误）
* **现象与原因**：在标准的类 Unix 系统中，普通文件和 Socket 必须共享同一个全局唯一的 FD 命名空间。但当前系统中，`fd_table`（普通文件）和 `socket_table`（Socket）的 FD 是**独立分配**的。
* **代码证据**：`SocketTable::new()` 将 `next_fd` 初始化为 `0`。当分配出 `fd = 0` 或 `1` 时，会与 `fd_table` 中的 `stdin`、`stdout` 发生冲突。在 `sys_write` 中，如果优先检查 `socket_table`，会导致向 `stdout` 写入的日志数据（如 `printf`）被错误地劫持并发送到 Socket 中，彻底破坏标准 I/O。

### 2. `fcntl` 缺少对 `F_SETFL` 的支持
* **现象与原因**：高性能网络工具极度依赖非阻塞 I/O，通常通过 `fcntl(fd, F_SETFL, flags | O_NONBLOCK)` 将 Socket 切换为非阻塞模式。
* **代码证据**：`sys_fcntl` 的实现仅处理了 `F_DUPFD`、`F_GETFD`、`F_SETFD` 和 `F_GETFL`，缺少对 `F_SETFL`（常量值为 4）的支持。导致 `iperf3` 尝试设置非阻塞时收到 `-22`（`EINVAL`）错误，程序误认为系统发生严重异常而退出。

### 3. 多路复用系统调用（I/O Multiplexing）不完整
* **现象与原因**：`iperf3` 是事件驱动程序，依赖 `select`、`poll` 或 `epoll` 同时监听控制和数据连接。
* **代码证据**：`SYSCALL_EPOLL_CREATE1` 直接返回 `-38` (`ENOSYS` 未实现)。系统仅实现了 `sys_ppoll`，**完全未实现 `sys_select` 或 `sys_pselect6`**。如果 C 库试图回退使用 `select` 或 `epoll`，将因系统调用未实现导致程序崩溃。

### 4. UDP Socket 不支持 `read`/`write` 操作
* **现象与原因**：在 POSIX 标准中，UDP Socket 在调用 `connect` 后，是可以使用普通的 `read()` 和 `write()` 收发数据的。
* **代码证据**：在 `sys_write` 和 `sys_read` 中存在硬编码检查，如果 `socket_type != Tcp` 则直接返回 `-95` (`EOPNOTSUPP`)。这使得 `iperf3` 的 UDP 测速模式无法工作。

### 5. `sys_fstat` 未覆盖 Socket 描述符
* **现象与原因**：某些 libc 的网络库在操作 fd 前会调用 `fstat` 检查文件类型。
* **代码证据**：`sys_fstat` 中仅检查 `fd_table`（`if fd >= inner.fd_table.len() { return -1; }`），完全忽略了 `socket_table`。这会导致 libc 内部断言失败或网络库行为异常。

---

## 🛠️ 修复建议 (Roadmap)

1. **统一 FD 空间（最高优先级）**：
   * **废弃** `socket_table.alloc_fd()`。
   * **重构**：在 `sys_socket` 中，统一使用 `inner.alloc_fd()` 从全局 `fd_table` 分配真实的 FD 编号，并将编号映射到 Socket 数据结构。
   * **建议方案**：使 `fd_table` 支持多态，例如使用 `enum FileDescriptor { File(Arc<dyn File>), Socket(SocketEntry) }` 或实现统一的 `File` Trait。
2. **补全 `fcntl` 系统调用**：
   * 在 `sys_fcntl` 中增加 `F_SETFL` 处理逻辑，允许程序动态修改 `SocketEntry` 的 `nonblock` 属性。
3. **完善事件驱动机制**：
   * 检查针对 `iperf3` 编译的 C 库多路复用机制。若是 `select`，需补充实现 `sys_pselect6`；如果库支持 `epoll`，则需实现相关的 epoll 机制。
4. **放开 UDP I/O 限制**：
   * 修改 `sys_read` 和 `sys_write`，移除针对 UDP 的硬编码拦截，将其路由到对应的 UDP 收发逻辑。
