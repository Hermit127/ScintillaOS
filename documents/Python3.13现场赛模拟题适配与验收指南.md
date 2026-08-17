# Python 3.13 现场赛模拟题适配与验收指南

本文记录一次按 2025 现场赛形式设计的 Python 3.13 模拟题，以及 ScintillaOS 为运行官方镜像内未经修改的 Python 所做的真实适配。目标不是“让 `python3 --version` 打印成功”，也不是在内核里识别 Python 进程，而是补齐 Python、glibc、SQLite、asyncio 和网络库共同依赖的通用 Linux ABI。

本文描述的工作分支是 `on-site-python-compat`。官方 raw Ext4 镜像、qcow2 覆盖层和串口日志都不进入 Git。

## 1. 模拟题

### 1.1 题目名称

在参赛操作系统上运行官方 Python 3.13 用户态环境。

### 1.2 统一环境

- 架构：RISC-V64 和 LoongArch64。
- 用户态：官方 2026 决赛 Ext4 镜像中的 Python 3.13.5、glibc 和标准库。
- Python、SQLite 和标准库文件不得修改。
- 官方 raw 镜像只读；每轮从 raw 镜像创建新的 qcow2 覆盖层。
- RISC-V 正式并发配置为 8 核，LoongArch 为 12 核；开发时可以先以 4 核分项排错。

镜像位置：

```text
testsuits-for-oskernel-final/image/sdcard-rv-pub.img
testsuits-for-oskernel-final/image/sdcard-la-pub.img
```

已校验的 raw 镜像 SHA-256：

```text
d74e436522f5946ca17280a7a25f17dbb6604b71fe675bb8a021ce8e849b334c  sdcard-rv-pub.img
d1410544e677e11efb1c240be6ffb201c89d6de58c9675e73314a696e4cefdc5  sdcard-la-pub.img
```

### 1.3 建议计分表（100 分）

| 项目 | 分值 | 通过条件 |
|---|---:|---|
| Python 加载 | 5 | `python3 -h` 退出码为 0，帮助文本非空 |
| 真实 REPL | 5 | 串口进入 `>>>`，执行表达式得到正确输出，并由 Ctrl-D 正常退出 |
| 标准库与数据处理 | 10 | JSON、SHA-256、gzip 往返结果正确，关键模块可导入 |
| 字节码与模块导入 | 5 | 真实生成 `.pyc`，由新进程导入并读取正确变量 |
| venv | 5 | 创建不带 pip 的 venv，并由 venv 内解释器证明 prefix 已切换 |
| Ext4 原子文件操作 | 10 | fsync、覆盖 rename、硬链接、符号链接、truncate 和 stat/read 长度一致 |
| SQLite | 10 | WAL、双连接、提交、回滚、checkpoint 和新进程重开均正确 |
| subprocess | 5 | stdin/stdout pipe、exec、wait 和退出码正确 |
| threading | 10 | 8 线程通过 Lock/Condition 完成确定计数，无丢唤醒 |
| multiprocessing | 10 | `fork`、`spawn`、Pipe、进程回收和结果均正确 |
| epoll/asyncio | 10 | pipe epoll 和 asyncio socketpair/定时任务均正确 |
| DNS | 5 | glibc `getent` 与 Python `getaddrinfo` 得到真实 IPv4 地址 |
| HTTPS | 5 | 启用 CA 校验下载固定不可变 URL，内容 SHA-256 完全匹配 |
| 远端写入副作用 | 5 | 客体 POST 唯一随机令牌，宿主服务真实收到并返回同一令牌 |

版本输出、导入一个纯 Python 模块、关闭证书校验或只访问 loopback 都不能替代上述功能。

## 2. 测试基础设施

### 2.1 文件职责

| 文件 | 职责 |
|---|---|
| `tests/onsite_python/python_regression.py` | 在官方客体中执行断言和打印通过标记 |
| `scripts/verify_onsite_python.py` | 建立覆盖层、启动 QEMU、传入测试、驱动 REPL、核验标记和副作用 |
| `scripts/verify_onsite_python_docker.sh` | 在官方竞赛 Docker 中运行 LoongArch QEMU |
| `Makefile` | 提供分项、全量和 LoongArch 验证入口 |
| `.gitignore` | 排除 `artifacts/onsite-python/` 中的大型运行证据 |

验证器会保存：

- Git 提交和工作树状态；
- kernel、根盘、官方盘和测试脚本 SHA-256；
- QEMU 版本、完整命令、架构、内存和核数；
- 未剪辑串口日志；
- 两个 qcow2 覆盖层；
- 两个 `qemu-img check` 结果。

网络验收中的 HTTPS URL固定到 xv6-riscv 的不可变提交，预期 SHA-256 固定为：

```text
05342be05e75f9a75e70fc0bfabd223e7c755e373b6ac584b0918331ea5ae9a2
```

POST 使用每轮随机令牌。只有串口出现正确响应且宿主 `ReceiptServer` 确实收到同一令牌，`PYTHON_REMOTE_SIDE_EFFECT_OK` 才算有效。

### 2.2 通过标记

```text
PYTHON_LOAD_OK
PYTHON_REPL_OK
PYTHON_STDLIB_OK
PYTHON_VENV_OK
PYTHON_FS_ATOMIC_OK
PYTHON_SQLITE_OK
PYTHON_PROC_SYMLINK_OK
PYTHON_SUBPROCESS_OK
PYTHON_THREAD_OK
PYTHON_MULTIPROCESS_OK
PYTHON_ASYNCIO_OK
PYTHON_DNS_OK
PYTHON_HTTPS_OK
PYTHON_REMOTE_SIDE_EFFECT_OK
PYTHON_ONSITE_ALL_OK
PYTHON_DRIVER_DONE rc=0
```

标记是断言完成后的摘要，不是测试替身。验证器还会检查 CPU online mask、宿主 POST 事件和覆盖层完整性。

## 3. 实测暴露的兼容性缺口

### 3.1 venv 因 xattr 返回 ENOSYS 失败

最初核心测试在 `venv.EnvBuilder.create()` 失败：

```text
shutil.copy2 -> copystat -> _copyxattr -> os.listxattr
OSError: [Errno 38] Function not implemented
```

Python 的 `shutil` 会复制扩展属性。Linux 上文件没有扩展属性时，`listxattr` 应返回长度 0，而不是 `ENOSYS`。Python 只会忽略特定的不支持错误，不能把任意未知系统调用当成“空列表”。

修复位置：

- `os/src/syscall/mod.rs`
- `os/src/syscall/fs.rs`

实现了 asm-generic 编号 5–16 的 `set/get/list/remove xattr` 及 fd、跟随/不跟随符号链接变体。当前文件系统没有持久 xattr 后端，因此采用清晰的兼容边界：

- 合法目标的 `listxattr` 返回 0，表示空属性列表；
- `getxattr/removexattr` 返回 `ENODATA`；
- `setxattr` 返回 `ENOTSUP`；
- 仍严格验证路径、fd、名称、flags 和用户缓冲区，并保留 `ENOENT`、`EBADF`、`EFAULT`、`ERANGE` 等错误边界。

这不是伪造 xattr 存储。当前实现只声明“该文件没有属性，且此文件系统不支持写入属性”。如果未来赛题要求真正保存 `user.*` 属性，需要在 Ext4/VFS 增加持久后端，不能继续以本兼容层冒充。

### 3.2 `os.truncate(path, length)` 返回 ENOSYS

原系统已经支持 `ftruncate(2)` 和打开时截断，但缺少 asm-generic syscall 45 `truncate(2)`。Python 的 `os.truncate` 因此直接失败。

新增 `sys_truncate` 后，通过正常 `openat(O_WRONLY)`、`ftruncate` 和 `close` 路径实现。这样复用了已有的：

- 路径解析和写权限检查；
- 只读挂载检查；
- tmpfs 空间记账；
- Ext4 截断和 inode 长度刷新；
- 共享映射失效处理。

它没有另写一套 Ext4 逻辑，因此 `truncate(path)` 与 `ftruncate(fd)` 不会逐渐产生两种语义。

### 3.3 asyncio 无法接管 socketpair fd

Python 的 `socket.socketpair()` 先获得两个底层 fd，再 `detach()` 并构造高级 `socket` 对象。构造过程查询 socket 类型和地址。原内核的 AF_UNIX socketpair 存在于 VFS fd 表中，而 `getsockopt`、`getsockname`、`getpeername` 只识别 smoltcp Internet socket，于是返回 `EBADF`：

```text
asyncio -> _make_self_pipe -> socket.socketpair
OSError: [Errno 9] Bad file descriptor
```

修复位置：

- `os/src/fs/unix_socket.rs`
- `os/src/net/syscall.rs`

补齐了：

- `SOL_SOCKET/SO_TYPE`，stream 返回 1，seqpacket 返回 5；
- `SOL_SOCKET/SO_ERROR`，健康的本地 socketpair 返回 0；
- 未命名 AF_UNIX socketpair 的 `getsockname/getpeername`，返回只有两字节 family 的地址。

修复后 asyncio 使用真实 epoll、非阻塞 socketpair 和定时器运行，没有改用忙轮询。

### 3.4 glibc DNS 无法发送 UDP 包

2025 Alpine/musl 的 `getent` 可以工作，但官方 2026 glibc 镜像中的 `getent` 和 Python `getaddrinfo` 都稳定返回 `EAI_AGAIN`。DHCP 已正确给出 `10.0.2.15/24`、网关 `10.0.2.2` 和 DNS `10.0.2.3`，所以不是配置文件缺失。

诊断内核没有记录未知 syscall。比较调用模型后发现两个通用 UDP 错误：

1. glibc 先 `connect()` UDP socket，再以空目的地址调用 `sendto/sendmsg`。Linux 应使用已连接 peer，原实现却返回 `EDESTADDRREQ`。
2. 未显式 `bind()` 的 UDP socket 在 `connect` 或首次发送时应自动绑定临时端口。原实现把未绑定 socket 直接交给 smoltcp，发送被拒绝，DNS 包没有离开客体。

`os/src/net/syscall.rs` 现在：

- 为 connected UDP 的空目的 `sendto/sendmsg` 使用记录的 peer；
- 在 UDP `connect` 时隐式绑定全局分配的临时端口；
- 对未连接但显式给出目的地址的首次发送也执行隐式绑定；
- 保持真实 DNS 响应源地址、`MSG_PEEK`、非阻塞和原有临时端口分配语义。

这项修改适用于任何 glibc UDP 客户端，不是 DNS 或 Python 域名特判。

## 4. 自动复测

### 4.1 静态检查和构建

```bash
git diff --check
python3 -m py_compile \
  scripts/verify_onsite_python.py \
  tests/onsite_python/python_regression.py
sh -n scripts/verify_onsite_python_docker.sh
cargo check -p os --target riscv64gc-unknown-none-elf
make kernel-rv
```

LoongArch 使用官方容器工具链重新构建。若普通 UID 无法访问容器内 `/root` 工具链，可以用容器默认用户构建，但退出前必须把生成文件所有权恢复给宿主 UID；不要在仓库中留下 root 所有权文件。

### 4.2 RISC-V 分项测试

无需网络代理的三组可以直接运行：

```bash
make verify-onsite-python-core PYTHON_ONSITE_SMP=4
make verify-onsite-python-fs PYTHON_ONSITE_SMP=4
make verify-onsite-python-concurrency PYTHON_ONSITE_SMP=4
```

网络模式先在单独终端启动代理中继：

```bash
make onsite-2025-proxy-relay
```

再运行：

```bash
make verify-onsite-python-network PYTHON_ONSITE_SMP=4
```

网络测试结束后以 Ctrl-C 停止中继。中继默认只监听宿主 `127.0.0.1:27898`，上游是 `127.0.0.1:7897`。如果本机代理端口不同，显式修改中继参数或 `PYTHON_ONSITE_PROXY`，不要把代理地址硬编码进内核。

### 4.3 RISC-V 完整测试

四核开发验收：

```bash
make verify-onsite-python-all PYTHON_ONSITE_SMP=4
```

平台上限八核验收：

```bash
make verify-onsite-python-all PYTHON_ONSITE_SMP=8
```

### 4.4 LoongArch 完整测试

先确保 `kernel-la` 是从当前源码构建的，再运行：

```bash
make verify-onsite-python-la-docker \
  PYTHON_ONSITE_MODE=all \
  PYTHON_ONSITE_MEMORY=2G
```

Docker wrapper 使用 host network，使容器内 QEMU 可以访问宿主代理中继。正式配置固定 `-smp 12`。

### 4.5 旧功能防回退

本次修改触及 VFS、fd、AF_UNIX 和 UDP，因此必须同时运行旧回归：

```bash
make verify-onsite-2025-regressions QEMU_SMP=4
make verify-onsite-2025-network QEMU_SMP=4
make verify-onsite-2026-abi QEMU_SMP=4
```

网络回归会在宿主绑定临时 TCP 端口。在受限沙箱中失败时，应在允许本机临时监听的环境重跑；不要把宿主 `EPERM` 归因于客体内核。

## 5. 手工测试

自动验证之外，可以启动官方盘并在 shell 中检查关键路径。以下命令假定官方盘挂载为 `vda`：

```sh
/busybox mount -t ext4 vda /mnt
/busybox chroot /mnt /bin/bash
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export TERM=xterm-256color
mount -t proc proc /proc 2>/dev/null || true
printf 'nameserver 10.0.2.3\n' > /etc/resolv.conf
```

解释器和 REPL：

```sh
python3 -h >/tmp/python-help.txt
echo $?
python3
>>> import sys, hashlib
>>> sys.version
>>> hashlib.sha256(b'onsite').hexdigest()
```

用 Ctrl-D 退出 REPL。

venv：

```sh
python3 -m venv --without-pip /tmp/pyenv
/tmp/pyenv/bin/python -c 'import sys; print(sys.prefix != sys.base_prefix)'
```

SQLite：

```sh
python3 - <<'PY'
import sqlite3
p='/tmp/manual.db'
c=sqlite3.connect(p)
print(c.execute('pragma journal_mode=wal').fetchone())
c.execute('create table if not exists t(x integer)')
c.execute('insert into t values (2026)')
c.commit()
c.close()
print(sqlite3.connect(p).execute('select * from t').fetchall())
PY
```

DNS 与 HTTPS 需要宿主代理中继：

```sh
export https_proxy=http://10.0.2.2:27898
getent ahostsv4 github.com
python3 -c 'import socket; print(socket.getaddrinfo("github.com",443,socket.AF_INET))'
python3 -c 'import urllib.request; print(urllib.request.urlopen("https://github.com",timeout=60).status)'
```

手工输出用于定位，最终结论仍以固定 URL 哈希、POST 副作用和全新覆盖层的自动验收为准。

## 6. 已完成的动态验证

2026-08-18 已完成以下全量运行：

| 架构 | 核数 | 结果 | 证据目录 |
|---|---:|---|---|
| RISC-V64 | 4 | 全部通过 | `artifacts/onsite-python/20260817T194329Z-riscv64-all` |
| RISC-V64 | 8 | 全部通过，online mask `0xff` | `artifacts/onsite-python/20260817T194620Z-riscv64-all` |
| LoongArch64 | 12 | 全部通过，online mask `0xfff` | `artifacts/onsite-python/20260817T195246Z-loongarch64-all` |

关键 kernel SHA-256：

```text
5f937cb03d70f380b3cd8cc11956dc2c9375c60d2be7f90580381e15ffdacd11  RISC-V 全量测试内核
bdca79905206712f0dc76a7612032d6f76b63bcce8153f86f3faa33e001a00cb  LoongArch 全量测试内核
```

三个全量证据目录内的 `root-overlay-check.txt` 和 `test-overlay-check.txt` 均以 `No errors were found on the image.` 开头。

旧功能回归结果：

- 2025 timer/TTY/Ext4 四核回归通过；
- 2025 网络 `getsockname`、连续临时端口与 TCP EOF 四核回归通过；
- 原有 Git/Vim/GCC/Rustc 适配未被本次测试脚本或官方镜像改写。

## 7. 当前边界

- 官方镜像没有预装 pip 和 Python 开发头文件，所以本题验证 `venv --without-pip`，不能据此声称 pip 安装第三方 C 扩展通过。
- xattr 当前实现为空属性/不支持写入的兼容层，不具备持久属性存储。
- HTTPS 依赖官方镜像已有 CA 证书和宿主代理；内核不会修改 `/etc/resolv.conf` 或代理配置。
- 测试证明的是官方 Python 3.13.5 与所列标准库工作负载，不代表任意第三方 Python 包都已兼容。
- 证据目录被 Git 忽略；提交源码前不要强制加入 qcow2、raw 镜像、串口凭据或本机代理配置。

## 8. 维护规则

以后修改以下任一部分，至少重跑对应分项，合并前重跑两架构 `all`：

| 修改区域 | 最小回归 |
|---|---|
| VFS、Ext4、rename、truncate、xattr | `core`、`fs`、2025 regressions |
| clone、exec、wait、futex、pipe、epoll | `concurrency`、2026 ABI |
| AF_UNIX、fd table、fcntl、getsockopt | `concurrency`、2025 network |
| VirtIO-net、UDP、DHCP、socket syscall | `network`、2025 network |
| 调度器、任务生命周期、多核锁 | RISC-V 8 核 `all` 和 LoongArch 12 核 `all` |

如果出现新失败，保留失败覆盖层和完整串口日志，先建立最小复现，再按 Linux 语义修复通用接口。不要删除断言、放宽标记或根据程序名伪造成功。
