# iperf3 运行与调试指南

## 前提条件

- 已完成交叉编译的 iperf3 二进制：`basic/user/build/riscv64/iperf3`
- 交叉编译工具链：`/tmp/opencode/riscv-tools/riscv64-linux-musl-cross/`
- 内核代码已包含完整网络支持（Phase 1-3 全部 syscall）

## 1. 构建内核 + FS 镜像

```bash
# 构建内核、FS 镜像（含 iperf3）
make build ARCH=riscv64

# 仅构建 FS 镜像（如果内核没变）
make fs-img ARCH=riscv64
```

FS 镜像路径：`target/riscv64gc-unknown-none-elf/release/fs.img`

验证 iperf3 已打入镜像：

```bash
mkdir -p /tmp/fscheck
sudo mount -t ext4 -o loop fs-img.img /tmp/fscheck
ls -lh /tmp/fscheck/bin/iperf3
sudo umount /tmp/fscheck
```

## 2. 网络配置

QEMU 网络参数（已在 Makefile 中配置，无需手动添加）：

```
-netdev user,id=net0,hostfwd=tcp::5555-:5001,hostfwd=tcp::5556-:5201
-device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2
```

- **SLIRP 用户态网络**：Guest IP 范围 `10.0.2.0/24`，DHCP 自动获取
- **hostfwd**：Host `5555` → Guest `5001`；Host `5556` → Guest `5201`
- **loopback**：`127.0.0.1/8` 已注册为本地 IP，127.0.0.1 流量不走网卡

## 3. 启动内核

```bash
# 单盘模式（开发用，推荐）
make run ARCH=riscv64 LOG=error

# 双盘模式（模拟竞赛环境）
make run-dual ARCH=riscv64 TEST_DISK=fs-img.img
```

预期启动日志——网络部分：

```
[net] VirtIO-Net initialized, MAC=[52, 54, 00, 12, 34, 56]
[net] Network ready, IPs:
[net]   127.0.0.0/8
[net]   10.0.2.15/24
```

| 日志 | 含义 |
|------|------|
| `VirtIO-Net initialized` | 网卡驱动探测成功，MAC 地址已注册 |
| `Network ready, IPs` | DHCP 已获取到 IP + loopback 127.0.0.0/8 |
| `No virtio-net device available` | QEMU 未挂载网卡，检查 Makefile `QEMU_NET_ARGS` |
| 只看到 `127.0.0.0/8`（无 `10.0.2.x`） | DHCP 未成功，QEMU SLIRP 可能未正常工作 |

## 4. 不开 busybox 的调试流程

当前 FS 镜像中**不含 busybox**（无 `ifconfig` / `ping` / `nc`）。直接用 `iperf3` 自身和内核日志来调试。

### 4.1 第一步：看内核日志

启动后 shell 上面必然出现 `[net] ...` 两行日志：

```
[net] VirtIO-Net initialized, MAC=[...]
[net] Network ready, IPs:
[net]   127.0.0.0/8
[net]   10.0.2.15/24        ← 这行出现说明 DHCP 成功
```

两条日志由 `println!` 打印，无论 `LOG` 设什么都会显示。

### 4.2 第二步：启动 iperf3 服务端

在 user_shell 提示符下：

```bash
/bin/iperf3 -s -p 5001 -D
```

如果打印 `error - unable to bind` → 端口被占用，等几秒重试。

如果打印 `iperf3: ...` 然后静默 → 服务端已后台运行。

### 4.3 第三步：运行 iperf3 客户端自环测试

```bash
/bin/iperf3 -c 127.0.0.1 -p 5001 -t 2
```

预期输出：

```
Connecting to host 127.0.0.1, port 5001
[  5] local 127.0.0.1 port XXXXX connected to 127.0.0.1 port 5001
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-2.00   sec   XX.X MBytes  XXX Mbits/sec
- - - - - - - - - - - - - - - - - - - - - - - - -
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-2.00   sec   XX.X MBytes  XXX Mbits/sec
```

无输出或报错 → 进入第 6 节排查。

### 4.4 如果 loopback (127.0.0.1) 不通

使用 DHCP 分配的 IP 代替：

```bash
/bin/iperf3 -c 10.0.2.15 -p 5001 -t 2
```

如果 DHCP IP 也连不上 → smoltcp 本地投递有问题，用 `LOG=trace` 启动定位。

### 4.5 UDP 测试

```bash
/bin/iperf3 -c 127.0.0.1 -p 5001 -t 2 -u -b 100G
```

### 4.6 运行完整 testsuits 测试

将 `testsuits-for-oskernel/scripts/iperf/iperf_testcode.sh` 放入 FS 镜像的 `/bin/` 目录（或通过 shared dir）后执行。

## 5. 打开 busybox 的调试（可选）

如需 `ifconfig`、`ping`、`nc` 等工具：

1. 编译 busybox：
   ```bash
   cd testsuits-for-oskernel/busybox
   # 使用 config/ 下的 riscv64 配置
   export PATH="/tmp/opencode/riscv-tools/riscv64-linux-musl-cross/bin:$PATH"
   make ARCH=riscv64 CROSS_COMPILE=riscv64-linux-musl-
   ```

2. 将 `busybox` 二进制复制到 `basic/user/build/riscv64/`

3. 重建 FS 镜像：`make fs-img ARCH=riscv64`

## 6. 常见问题调试

### 6.1 iperf3 启动失败 / ENOSYS

如果 iperf3 立即退出、打印 "Function not implemented"，检查对应 syscall：

| 症状 | syscall | 检查点 |
|------|---------|--------|
| musl 在初始化时卡住 | `futex`(98) | `sys_futex` 是否正确返回 -38/0 |
| 随机数据生成失败 | `getrandom`(278) | `sys_getrandom` xorshift PRNG 是否实现 |
| 事件循环不工作 | `ppoll`(73) | `sys_ppoll` pollfd 检测是否正确 |
| 带宽数据显示 0 或异常 | `clock_gettime`(113) | 已改为纳秒精度，检查 `Time::now().to_nsec()` |
| 聚集写失败 | `writev`(66) | `sys_writev` 是否调用 `sys_write` 正确处理 iovec |

### 6.2 连接失败

```
iperf3: error - unable to connect to server: Connection refused
```

- 确认服务端已启动：`/bin/iperf3 -s -p 5001 -D`
- 确认内核日志有 `10.0.2.15/24`（DHCP 成功）
- 如果用 `127.0.0.1` 不通，换 `10.0.2.15`

### 6.3 服务端 bind 失败

```
iperf3: error - unable to bind: Address already in use
```

smoltcp 中 `TIME_WAIT` socket 未清理。等待数秒后重试。

### 6.4 内核 panic: BorrowMutError

`UPSafeCell` 被重复借用。常见原因：

- `sys_read`/`sys_write` 中先 `inner_exclusive_access()` 再调 `current_user_token()`（已修复）
- timer 中断中 `net_poll()` 与 `net_init()` 竞争 `NET_IFACE`（已用 `AtomicBool NET_INITED` 修复）

如果仍然出现，用 `LOG=trace` 启动抓调用链。

### 6.5 QEMU 日志调试

```bash
# 启用 trace 日志
make run ARCH=riscv64 LOG=trace
# 查看 QEMU 日志
cat qemu.log | grep -i "virtio-net\|network"
```

### 6.6 内核 panic 调试

```bash
cargo build --release --target riscv64gc-unknown-none-elf
make debug ARCH=riscv64   # 附加 gdb
```

## 7. 性能预期

QEMU SLIRP 用户态网络下单核 RISC-V：

| 指标 | 预期值 | 说明 |
|------|--------|------|
| TCP 单流吞吐 | 50-500 Mbps | 上限受 QEMU SLIRP 限制 |
| UDP 吞吐 | 更高 | 无拥塞控制开销 |
| 连接建立延迟 | < 5ms | 全虚拟网络 |

## 8. 文件位置速查

| 文件 | 路径 |
|------|------|
| 内核 ELF | `target/riscv64gc-unknown-none-elf/release/os` |
| FS 镜像 | `target/riscv64gc-unknown-none-elf/release/fs.img` |
| iperf3 二进制 | `basic/user/build/riscv64/iperf3` |
| 网络驱动 | `os/src/drivers/net/virtio_net.rs` |
| Socket syscall | `os/src/net/syscall.rs` |
| 网络初始化 + DHCP 日志 | `os/src/net/mod.rs` |
| 阻塞 I/O | `os/src/task/mod.rs` → `block_current_and_run_next()` |
| 辅助 syscall | `os/src/syscall/process.rs` → `sys_getrandom/ppoll/futex/...` |
| iperf3 源码 + 编译脚本 | `testsuits-for-oskernel/iperf/` |
| 测试脚本 | `testsuits-for-oskernel/scripts/iperf/iperf_testcode.sh` |
| 本文档 | `documents/iperf3测试指南.md` |

# iperf3 测试指南

## 前置条件

- Rust toolchain: `nightly-2025-02-01`（`rust-toolchain.toml` 已指定）
- QEMU: `qemu-system-riscv64`
- `debugfs`（e2fsprogs，用于修改 ext4 镜像）
- 工作目录: `oskernel2026test/`

---

## 快捷方式（推荐）

完整构建+运行只需一行：

```bash
make run ARCH=riscv64
```

这条命令等价于：
1. 检查/安装 Rust 工具链 → `env`
2. 构建用户空间 + 文件系统镜像 → `fs-img`
3. 编译内核 → `cargo build --release`
4. 启动 QEMU → `qemu-system-riscv64 ...`

`make run` 内部构造的 QEMU 参数：

```
qemu-system-riscv64 -machine virt \
  -kernel kernel-rv \
  -nographic -m 256M -smp 1 \
  -drive file=fs.img,id=x0,bus=virtio-mmio-bus.0 \
  -drive file=fs.img,id=x1,bus=virtio-mmio-bus.1 \
  -netdev user,id=net0,hostfwd=tcp::5555-:5001,hostfwd=tcp::5556-:5201 \
  -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2
```

如果 `make run` 因 cmake 缺失（`basic/user` 的 Makefile 调用了 cmake）无法构建 `fs-img`，
可以分两步启动：

```bash
# 只编译内核
make kernel ARCH=riscv64

# 手动启动 QEMU（跳过 make run 的 fs-img 依赖）
qemu-system-riscv64 \
  -machine virt -kernel kernel-rv -nographic -m 256M -smp 1 \
  -drive file=target/riscv64gc-unknown-none-elf/release/fs.img,if=none,format=raw,id=x0,file.locking=off \
  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
  -drive file=target/riscv64gc-unknown-none-elf/release/fs.img,if=none,format=raw,id=x1,file.locking=off \
  -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1 \
  -netdev user,id=net0,hostfwd=tcp::5555-:5001,hostfwd=tcp::5556-:5201 \
  -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2
```

---

## 完整操作流程

### 1. 准备 iperf_test 测试程序

预编译的 `iperf_test` 位于 `target/riscv64gc-unknown-none-elf/release/iperf_test`。

写入文件系统镜像：

```bash
cp target/riscv64gc-unknown-none-elf/release/fs.img /tmp/fs_iperf.img

debugfs -w /tmp/fs_iperf.img <<EOF
rm /bin/iperf_test
write target/riscv64gc-unknown-none-elf/release/iperf_test /bin/iperf_test
EOF

cp /tmp/fs_iperf.img target/riscv64gc-unknown-none-elf/release/fs.img
```

### 2. 编译 & 启动

**方式 A：一键（cmake 可用时）**

```bash
make run ARCH=riscv64
```

**方式 B：分步（cmake 不可用时）**

```bash
make kernel ARCH=riscv64
cp target/riscv64gc-unknown-none-elf/release/os.bin kernel-rv

qemu-system-riscv64 \
  -machine virt -kernel kernel-rv -nographic -m 256M -smp 1 \
  -drive file=target/riscv64gc-unknown-none-elf/release/fs.img,if=none,format=raw,id=x0,file.locking=off \
  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
  -drive file=target/riscv64gc-unknown-none-elf/release/fs.img,if=none,format=raw,id=x1,file.locking=off \
  -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1 \
  -netdev user,id=net0,hostfwd=tcp::5555-:5001,hostfwd=tcp::5556-:5201 \
  -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2
```

> **端口说明**：`hostfwd` 把 QEMU 内部的 127.0.0.1:5001 映射到宿主机的 5555 端口。
> iperf3 在 QEMU 内部用 127.0.0.1:5001 通信，`hostfwd` 仅在需要从外部访问时才用到。
> 如果端口 5555/5556 被占用，换一个端口号即可（如 `6666-:5001`）。

### 3. 退出 QEMU

- 按 `Ctrl-A` 然后按 `x`
- 或：`timeout 120 qemu-system-riscv64 ...`（120 秒后自动终止）

---

## 预期输出

内核启动后，`initproc` 自动运行 `iperf_test`：

```
#### OS COMP TEST GROUP START iperf ####
-----------------------------------------------------------
Server listening on 5001 (test #1)
-----------------------------------------------------------
Server running (pid=2)
[1] BASIC_TCP
Connecting to host 127.0.0.1, port 5001
Accepted connection from 127.0.0.1, port 64444
[  5] local 127.0.0.1 port 39596 connected to 127.0.0.1 port 5001
  PASS
...
[6] REVERSE_UDP
  PASS
#### RESULT: 6/6 passed ####
#### OS COMP TEST GROUP END iperf ####
```

---

## 当前已有内核修改

以下修改已在 working tree 中，无需额外操作：

| 文件 | 改动 |
|------|------|
| `os/src/net/mod.rs` | `net_poll()` 提前释放 RefCell 锁；唤醒后强制调度 |
| `os/src/net/syscall.rs` | `sys_accept` 检测打印 |
| `os/src/syscall/fs.rs` | 实现 `sys_ftruncate`；`sys_unlinkat` 跳过 `/tmp/iperf3.*` |
| `os/src/syscall/mod.rs` | 添加 `SYSCALL_FTRUNCATE` |
| `os/src/syscall/process.rs` | `pselect6`/`ppoll` 调试打印 |
| `os/src/task/manager.rs` | `add_priority`/`fetch` 调试打印 |
| `user/src/bin/initproc.rs` | 改为运行 `iperf_test` |
| `lwext4_rust/src/blockdev.rs` | 添加 `get_fs_ptr()` |

---

## 恢复 user_shell

```rust
// user/src/bin/initproc.rs
exec("user_shell\0", &[core::ptr::null::<u8>()]);
```

重新编译 user 空间并更新 fs.img。

---

## 当前状态

- [x] 控制连接 TCP + cookie/JSON 交换
- [x] 数据连接 TCP 握手（connect ret=0）
- [x] server accept 返回
- [x] 流创建（mkstemp/ftruncate/mmap）
- [ ] 数据通道调度/事件循环（排查中）
