# netperf 测试差距分析与执行指南

## 一、测试概述

netperf 是网络性能基准测试工具，测试 TCP/UDP 的吞吐量和请求-响应延迟。比赛测试全程走 **环回地址 `127.0.0.1:12865`**，不涉及外部网络通信。

### 测试脚本

位于 `testsuits-for-oskernel/scripts/netperf/netperf_testcode.sh`，执行 5 个子测试：

| 测试项 | 说明 | 参数 |
|--------|------|------|
| UDP_STREAM | UDP 单向吞吐量 | `-s 16k -S 16k -m 1k -M 1k` |
| TCP_STREAM | TCP 单向吞吐量 | `-s 16k -S 16k -m 1k -M 1k` |
| UDP_RR | UDP 请求-响应延迟 | `-s 16k -S 16k -m 1k -M 1k -r 64,64 -R 1` |
| TCP_RR | TCP 请求-响应延迟 | `-s 16k -S 16k -m 1k -M 1k -r 64,64 -R 1` |
| TCP_CRR | TCP 连接-请求-响应 | `-s 16k -S 16k -m 1k -M 1k -r 64,64 -R 1` |

执行流程：
```
1. 后台启动 netserver: ./netserver -D -L 127.0.0.1 -p 12865 &
2. 依次运行 5 个 netperf 测试（每个 -l 1 秒）
3. kill -9 结束 netserver
```

输出标记：`#### OS COMP TEST GROUP START netperf ####` / `#### OS COMP TEST GROUP END netperf ####`

---

## 二、当前内核网络支持现状

### 2.1 已实现的组件

| 组件 | 状态 | 位置 |
|------|------|------|
| VirtIO-Net 驱动 | 已实现 (riscv64/aarch64) | `os/src/drivers/net/virtio_net.rs` |
| smoltcp 协议栈 | 已集成 (v0.11) | `os/src/net/mod.rs` |
| 本地 TCP 快速路径 | 已实现 | `os/src/net/syscall.rs:526-780` |
| 本地 UDP 快速路径 | 已实现 | `os/src/net/syscall.rs:387-497` |
| QEMU 网络配置 | 已配置 | `Makefile:374-385` (SLIRP + hostfwd) |

### 2.2 已实现的 Socket 系统调用

| 系统调用 | 编号 | 状态 |
|---------|------|------|
| socket (198) | AF_INET, SOCK_STREAM/SOCK_DGRAM | 已实现 |
| bind (200) | | 已实现 |
| listen (201) | | 已实现 |
| accept (202) | | 已实现 |
| accept4 (242) | | 已实现 |
| connect (203) | | 已实现 |
| getsockname (204) | | 已实现 |
| getpeername (205) | | 已实现 |
| sendto (206) | | 已实现 |
| recvfrom (207) | | 已实现 |
| setsockopt (208) | SO_REUSEADDR, SO_RCVBUF/SNDBUF, SO_KEEPALIVE, TCP_NODELAY | 已实现 |
| getsockopt (209) | SO_ERROR, SO_RCVBUF/SNDBUF, TCP_NODELAY, TCP_INFO, TCP_MAXSEG, TCP_CONGESTION | 已实现 |
| shutdown (210) | | 已实现 |
| sendmsg (211) | | 已实现 |
| recvmsg (212) | | 已实现 |

### 2.3 其他关键系统调用

| 系统调用 | 状态 | netperf 是否需要 |
|---------|------|-----------------|
| clone (220) / fork (401) | 已实现 | 是 (netserver fork 子进程) |
| exec (221) | 已实现 | 可能 |
| kill (129) | 已实现 | 是 (kill -9 结束 netserver) |
| sigaction (134) | 已实现 | 是 (信号处理) |
| sigprocmask (135) | 已实现 | 是 |
| pselect6 (72) | 已实现 | 是 (I/O 多路复用) |
| ppoll (73) | 已实现 | 可能 |
| clock_gettime (113) | 已实现 | 是 (计时) |
| epoll_create1 (20) | 返回 -38 (ENOSYS) | 低概率 (netperf 通常用 select) |

### 2.4 本地通信路径分析

netperf 测试全程使用 `127.0.0.1`，内核已实现专用的本地快速路径：

- **本地 TCP**：`LOCAL_TCP_CONNS` (BTreeMap) + 双向 `VecDeque<u8>` 缓冲区，大小 1MB
- **本地 UDP**：`LOCAL_UDP_QUEUE` (VecDeque)，最大 256 个包
- `connect()` 检测到 `127.x.x.x` / `0.0.0.0` / `10.0.2.15` 时自动走本地路径

---

## 三、缺失组件清单

### 3.1 阻塞性缺失（必须解决）

| 缺失项 | 严重程度 | 说明 |
|--------|---------|------|
| **netperf/netserver 二进制** | 阻塞 | 源码 tarball 在 `testsuits-for-oskernel/netperf/netperf-2.7.0.tar.gz`，但没有预编译的 riscv64 静态链接二进制 |
| **Makefile rootfs 集成** | 阻塞 | `fs-img` 目标未将 netperf/netserver 二进制和测试脚本复制到 rootfs |

### 3.2 潜在风险项（可能需要修复）

| 风险项 | 严重程度 | 说明 |
|--------|---------|------|
| TCP_CRR 临时端口耗尽 | 中 | 每次请求重新 connect()，临时端口从 49152 递增，65535 后回绕。1秒内高频连接可能耗尽或冲突 |
| netperf 使用 glibc 动态链接 | 中 | 默认 `./configure && make` 生成动态链接二进制，需要确保静态编译或 rootfs 中有 glibc 动态库 |
| socketpair (199) | 低 | 仅有桩函数，netperf 可能不依赖 |
| sysinfo (179) | 低 | 返回 -38，netperf 可能查询系统信息 |
| loongarch64 不支持 | 已知 | LoopbackOnlyAdapter 的 receive() 始终返回 None，网络测试无法运行 |

---

## 四、交叉编译 netperf

### 4.1 方式一：在 testsuits Docker 容器中编译

```bash
cd testsuits-for-oskernel
# musl 静态链接版本（推荐）
PREFIX=riscv64-linux-musl- DESTDIR=sdcard/riscv/musl make -f Makefile.sub netperf

# glibc 版本
PREFIX=riscv64-linux-gnu- DESTDIR=sdcard/riscv/glibc make -f Makefile.sub netperf
```

编译过程：
1. 解压 `netperf/netperf-2.7.0.tar.gz`
2. `./configure --build=$(gcc -dumpmachine) --host=riscv64-linux-musl`
3. `make -j 8`
4. 生成 `netperf/netperf-2.7.0/src/netperf` 和 `netperf/netperf-2.7.0/src/netserver`

### 4.2 方式二：手动编译（需安装交叉编译工具链）

```bash
# 安装 musl 交叉编译器
sudo apt install musl-tools gcc-riscv64-linux-gnu
# 或使用 musl-cross-make 获取 riscv64-linux-musl-gcc

cd testsuits-for-oskernel/netperf
rm -fr netperf-2.7.0
tar zxf netperf-2.7.0.tar.gz
cd netperf-2.7.0

# 静态编译
CC="riscv64-linux-musl-gcc -static" \
  ./configure --build=$(gcc -dumpmachine) --host=riscv64-linux-musl
make -j$(nproc)

# 验证
file src/netperf src/netserver
# 应输出: ELF 64-bit LSB executable, UCB RISC-V, statically linked
```

### 4.3 编译产物放置

```bash
# 放到 tmp/ 目录（与 iperf3 等二进制一致的模式）
cp netperf-2.7.0/src/netperf ../../tmp/netperf-musl-riscv64
cp netperf-2.7.0/src/netserver ../../tmp/netserver-musl-riscv64
cp ../../testsuits-for-oskernel/scripts/netperf/netperf_testcode.sh ../../tmp/
```

---

## 五、Makefile 集成

在 `Makefile` 的 `fs-img` 目标中添加 netperf 二进制和测试脚本的复制逻辑。

在 riscv64 分支的文件复制部分（约第 184 行 `cp -f tmp/busybox-musl` 之后）添加：

```makefile
	cp -f tmp/netperf-musl-riscv64 $(ROOTFS_DIR)/netperf 2>/dev/null || true; \
	cp -f tmp/netserver-musl-riscv64 $(ROOTFS_DIR)/netserver 2>/dev/null || true; \
	cp -f tmp/netperf_testcode.sh $(ROOTFS_DIR)/netperf_testcode.sh 2>/dev/null || \
	  cp -f testsuits-for-oskernel/scripts/netperf/netperf_testcode.sh $(ROOTFS_DIR)/netperf_testcode.sh 2>/dev/null || true; \
```

---

## 六、执行测试

### 6.1 自动测试模式（MachineTest 分支）

`tmp/init.sh` 已包含 netperf 测试逻辑：
```sh
[ -f ./netperf_testcode.sh ] && busybox sh ./netperf_testcode.sh
```

只要 rootfs 中存在 `netperf`、`netserver`、`netperf_testcode.sh`，测试会自动执行。

```bash
# 构建并运行
make fs-img ARCH=riscv64
make run ARCH=riscv64
make run-dual ARCH=riscv64 TEST_DISK=testsuits-for-oskernel/sdcard-rv.img
```

### 6.2 手动测试模式（main 分支 shell）

进入 shell 后手动执行：

```sh
# 挂载测试盘（如果 netperf 在测试盘上）
mount -t ext4 /dev/vda2 /mnt
cd /mnt/musl   # 或 /mnt/glibc

# 或者如果 netperf 在 rootfs 根目录
cd /

# 执行测试
busybox sh ./netperf_testcode.sh
```

### 6.3 预期输出

```
#### OS COMP TEST GROUP START netperf ####
====== netperf UDP_STREAM begin ======
MIGRATED UDP STREAM TEST from 127.0.0.1 (127.0.0.1) port 0 AF_INET to 127.0.0.1 () port 0 AF_INET
Socket  Message  Elapsed      Messages
Size    Size     Time     OK  Errors   Throughput
bytes   bytes    secs            #      10^6bits/sec
...
====== netperf UDP_STREAM end: success ======
...（其他 4 个测试）
#### OS COMP TEST GROUP END netperf ####
```

---

## 七、调试建议

### 7.1 syscall 追踪

在 `os/src/syscall/mod.rs` 的 dispatch 函数中，对未识别的 syscall 添加日志输出，观察 netperf 调用了哪些未实现的 syscall：

```
[syscall] unimplemented id=XXX
```

### 7.2 常见问题排查

| 现象 | 可能原因 | 排查方式 |
|------|---------|---------|
| netserver 启动后立即退出 | fork 实现问题或信号处理问题 | 检查 sys_clone 是否正确返回 |
| connect 返回 -111 (ECONNREFUSED) | netserver 尚未 listen 或 LOCAL_TCP_LISTEN_PORTS 未注册 | 在 sys_listen 中加日志确认端口注册 |
| TCP_CRR 后期失败 | 临时端口耗尽 | 检查 NEXT_EPHEMERAL_PORT 是否回绕后冲突 |
| 吞吐量为 0 | 本地 TCP 缓冲区死锁或数据未传递 | 检查 local_tcp_send/recv 逻辑 |
| netperf 报 "establish control: are you sure there is a netperf..." | netserver 控制连接建立失败 | 检查 socket→bind→listen→accept→connect 全流程 |

### 7.3 单步验证

可以先用 busybox 验证网络基础功能：

```sh
# 在 shell 中测试 TCP 回环
busybox nc -l -p 12345 &
echo "hello" | busybox nc 127.0.0.1 12345
```

---

## 八、总结

| 维度 | 现状 |
|------|------|
| 内核网络栈 | 已就绪（smoltcp + 本地快速路径） |
| Socket 系统调用 | 已就绪（15 个核心 syscall 全部实现） |
| VirtIO-Net 驱动 | 已就绪（riscv64/aarch64） |
| netperf 二进制 | **未编译** — 需要交叉编译 |
| Makefile 集成 | **未完成** — 需要修改 fs-img 目标 |
| 内核功能完整性 | **基本就绪** — 可能需要根据实际运行情况微调 |

**核心结论**：内核侧的网络功能已经比较完整，主要差距在于用户态工具链（交叉编译 netperf 二进制）和构建系统集成（Makefile 复制到 rootfs）。解决这两个问题后即可尝试运行测试。
