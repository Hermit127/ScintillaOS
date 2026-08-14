# 2025 现场赛 RISC-V 实现、合并与验收指南

本文是 `on-site-final-2025-fixes` 分支的项目说明、实现原理、复现步骤和验收记录。后续即使不查看本次聊天，也应能据此知道赛题要求什么、为什么使用 Alpine/BusyBox、内核改了什么、怎样复测，以及哪些结论仍不能声称通过。

## 1. 结论与验证边界

现场赛不是要求在内核里重新实现 Git、Vim、GCC 或 Rustc。官方 Alpine Ext4 镜像已经包含这些原版 Linux 用户程序；参赛内核要提供足够兼容的 Linux 系统调用、Ext4、TTY、进程/信号/计时器和网络语义，使它们能够真实运行。

当前结论：

- QEMU RISC-V64 上，Git 帮助、本地仓库、完整 HTTPS clone、Vim 帮助和真实交互编辑、GCC/Rustc 帮助及编译运行均已在合并后内核通过。
- GitHub push/pull 已在合并前的同一套现场赛实现上手工通过；合并后的最终源码还应再手工输入 PAT 跑一遍，才能形成“当前提交完整 100 分”的严格证据。
- `main` 带入的 RISC-V 四核调度、亲和性、clone、futex、mprotect、共享内存、管道和信号定向回归，release 3/3、diagnostics 1/1 通过。
- LoongArch 当前不能声称构建或运行通过：本机缺少 `loongarch64-linux-gnu-gcc`，也没有 `qemu-system-loongarch64`、官方决赛大镜像或实体板卡。
- 完整 CAgent/BuildStorm 决赛流程需要官方决赛镜像，不能用 Alpine 现场赛镜像替代。

## 2. 关键 Git 历史与恢复点

本轮没有 rebase、squash 或改写历史，合并保留了双亲 merge commit。

| 含义 | 提交 |
|---|---|
| 合并前现场赛实现备份，已推送远程 | `a82b4ab9baeeb6238142538e302c5b3023aab66b` |
| 合入的 `origin/main` | `ed815ffc003ccc567d7b4b8da1fcdca349d12905` |
| 显式 merge commit，已进入远程历史 | `41462075aa60004260892ce410d67f7fa81b6faa` |
| 合并后文件写回和验证器修复，已进入远程历史 | `0d7dd76be39891f1818c595062e52e752b4b7690` |
| SMP4 MANIFEST/JSON 证据，当前远程 tip | `8803ab33cc0301a5c47d81a7704be0f8b47ad883` |
| 高位用户 sockaddr 安全复制修复，仅本地 | `0b9ef7445dc13703442b94f5bf03b6c68488a8dd` |

远程 `origin/on-site-final-2025-fixes` 当前停在 `8803ab3`。最新 sockaddr 修复及其文档提交仍只在本地，不应在未获明确授权时 push。

核对历史：

```bash
git switch on-site-final-2025-fixes
git log --oneline --decorate --graph -12
git show -s --format='%H%n%P%n%s' 41462075
git branch -vv
```

需要回看合并前精确状态时，用 `git show a82b4ab` 或另建恢复分支；不要对现有工作树执行 `git reset --hard`。

## 3. Alpine、BusyBox 与 chroot

`alpine-linux-riscv64-ext4fs.img` 是 RISC-V64 Linux 用户空间的 Ext4 磁盘镜像，包含 `/bin`、`/usr/bin`、动态链接器、musl libc、Git、Vim、GCC、Rustc 及其库。它不是另一个需要启动的 Linux 内核；QEMU 始终使用本项目的 `kernel-rv`。

`/busybox` 是项目测试盘中可直接执行的多调用工具，提供 `mount`、`chroot` 和 `sh` 等基础命令。它是进入 Alpine 前的救援工具箱，不是赛题要求实现的软件，也不是与 Alpine 二选一的操作系统。

启动层次为：

```text
QEMU
└── 本项目 RISC-V 内核
    ├── Rust user shell 与 /busybox（项目测试盘）
    └── vda：官方 Alpine Ext4 镜像
        └── chroot 后运行 Git/Vim/GCC/Rustc
```

`mount` 把 Alpine 文件系统接到 `/mnt`；`chroot /mnt /bin/sh` 只改变当前进程看到的根目录。此时命令和动态库来自 Alpine，但所有系统调用仍由本项目内核处理。

## 4. 镜像哈希、磁盘与启动拓扑

基础镜像默认位于项目相邻目录：

```text
../alpine-linux-riscv64-ext4fs.img
```

官方压缩包和解压后的 raw 镜像 SHA-256 为：

```text
.img.xz  0def50e343fa287b86c89346b53a835405c300fe45744a664833a4fbdb378a1c
.img     e3fe9eef8be9f35c23b4ca8b9d72af4f40744a275ab7f35782a4e235b3b6c231
```

现场赛启动使用两块 VirtIO 块设备和一块 VirtIO 网卡：

- `vda`：Alpine 基础镜像的临时 qcow2 覆盖层。
- `vdb`：项目生成的 `disk.img`，包含 Rust shell、BusyBox 和回归程序。
- VirtIO net：QEMU SLIRP，默认网关 `10.0.2.2`、DNS `10.0.2.3`。

验证脚本不会直接写官方 raw 镜像，而是每轮创建新覆盖层。四核 RISC-V 验证器的根盘必须用 `disk.img`；当前 `fs-img.img` 含 LoongArch 用户程序，不能用来冒充 RISC-V 根盘。

## 5. 现场赛实现内容与原理

### 5.1 POSIX timer 与信号

为 Vim 等程序补齐 `timer_create/gettime/getoverrun/settime/delete`。每进程维护独立 timer 表，支持 `CLOCK_REALTIME`、`CLOCK_MONOTONIC`、相对时间、`TIMER_ABSTIME`、单次和周期触发；到期走标准信号与任务唤醒路径。timer 不在 `fork` 中继承，并在 `exec/exit` 时清理；非法 clock、ID、时间或用户地址返回对应 errno。这修复了 Vim 的 `E1286: Could not set timeout: Function not implemented`，没有按应用名伪造结果。

### 5.2 TTY、termios 与 Enter/退格

控制台实现了：

- `/dev/tty` 可读写；持久保存 termios。
- `TCGETS`、`TCSETS`、`TCSETSW`、`TCSETSF`、窗口大小和前台进程组 ioctl。
- 普通文件/管道收到 TTY ioctl 返回 `ENOTTY`。
- 规范输入队列；`ICRNL` 开启时把 Enter 的 `CR` 转为程序需要的 `LF`。
- `DEL` 和 `Ctrl+H` 作为 `VERASE` 删除前一个字符并正确回显；非规范模式把字节交给 Vim。
- 有数据立即返回；`O_NONBLOCK` 无数据返回 `EAGAIN`；阻塞读取沿用 main 的定时睡眠而非忙等。

因此 Git 用户名/密码提示可用普通 Enter。VS Code 的 `Ctrl+J` 可能被宿主快捷键捕获，不能把它当成内核回车测试。

### 5.3 Ext4 rename、hard link 与 stat

Git 和 Vim 依赖比普通文件写入更严格的语义：Git 配置用 `.lock` 加覆盖 rename，loose object 用 `link(2)` 加 `unlink(2)` 发布；Vim 保存时截断重写，随后 `stat` 必须立刻返回新长度。

现场赛先修复目标已存在时覆盖 rename 返回 `EPERM`。合入 main 的 pending-write 缓存后又发现两个组合回归：

1. `read(2)` 能从缓存读出源码，但 `stat(2)` 从 lwext4 看到旧长度甚至 0，GCC 报源文件为空或 `shorter than expected`。
2. Git loose object 的源数据还在缓存中，硬链接发布出的对象可能为空，导致 commit 失败。

最终在 `os/src/fs/inode.rs` 中：path/open-file stat 先按正确锁序 flush，再以 `ext4_fsize` 为权威长度；flush 失败时用运行时 inode 长度兜底。`rename_at` 和 `link_at` 在物理命名操作前串行化目录/源文件修改并 flush，缓存失效使用已持锁版本，避免递归加锁。实现没有 Git/Vim 进程名特判。

### 5.4 VirtIO、DHCP、DNS 与 socket

网络路径完成 VirtIO TX/RX、DHCP 配置消费及应用所需 socket 语义：

- DHCP 实测地址 `10.0.2.15/24`、网关 `10.0.2.2`、DNS `10.0.2.3`；未完成 DHCP 时才用 SLIRP 默认值。
- `/etc/resolv.conf` 由用户态测试覆盖层写入，内核不修改官方 raw 镜像。
- 全局临时端口避免按 fd 重用同一源端口。
- 修复 `getsockname`、`SO_ERROR`、TCP FIN/EOF、UDP 真实源地址、`MSG_PEEK` 和 close/abort 生命周期。
- sockaddr 输入/输出通过任务页表的 `copy_user_value`/`write_user_value` 复制，不能在内核态直接解引用用户虚拟地址；这覆盖了 musl `getent` 使用的 `0x1000000000` 以上高位映射。
- 其余用户缓冲沿用 main 的安全准备与 `copy_to_task_user` 路径。

测试环境外网经过宿主 HTTP CONNECT 代理。代理中继只监听 loopback、使用有界缓冲且不记录载荷；它只转发连接，不伪造 Git 结果。

### 5.5 可重复验证器

Makefile 与 `scripts/verify_onsite_2025_*.py` 会强制构建最新 release 内核、为每轮创建干净 qcow2、保存哈希/QEMU 命令/串口/pcap，并在退出后运行 `qemu-img check`。C 源经 Base64 小块发送并在客体核对 SHA-256；每条命令使用拆分构造的唯一完成标记，防止 shell 命令回显被误判为成功。串口逐字节节流，diagnostics 等延迟输出安静后再发下一条命令。

## 6. main 带入的优化

`ed815ffc` 所在 main 历史可归纳为：

- SMP/任务：四核调度、任务生命周期、CPU affinity、动态内核栈、调度诊断和栈哨兵。
- 内存/同步：COW、futex、共享内存、mprotect、远程 TLB、锁顺序和丢唤醒修复。
- VFS/Ext4：dentry/VFS 缓存、目录操作、延迟写缓存、锁粒度和并发性能。
- IPC/信号：管道大数据、信号和多进程稳定性。
- 网络：socket 生命周期、安全用户内存复制和并发路径。
- 双架构/评测：RISC-V/LoongArch 构建、BuildStorm 产物、CAgent/BuildStorm 顺序评测及文档。

现场赛分支恢复了 Rust shell 的 `initproc`。若保留 main 的决赛自动入口，开机会自动跑 CAgent/BuildStorm 甚至关机，无法进行 Alpine 交互验收。

## 7. 五个冲突的最终选择

不能简单使用 `ours` 或 `theirs`，因为两侧各有不可丢失的能力。

| 文件 | main 侧保留内容 | 现场赛侧保留/融合内容 |
|---|---|---|
| `os/src/fs/inode.rs` | 新 VFS、锁、dentry/写缓存 | Ext4 实时长度、覆盖 rename，并补 pending-write 在 stat/link/rename 前持久化 |
| `os/src/fs/stdio.rs` | `O_NONBLOCK`、定时睡眠、调度优化 | 规范队列、`ICRNL`、DEL/Ctrl-H、termios、`/dev/tty` |
| `os/src/net/syscall.rs` | 安全用户缓冲、统一复制、可选 handle | `getsockname`、`SO_ERROR`、EOF、UDP 源地址、`MSG_PEEK`、全局临时端口和关闭清理 |
| `os/src/syscall/fs.rs` | `DeviceLock`、`TaskGuard`、`VfsLock` | `/dev/tty` 与全部 ioctl |
| `os/src/task/task.rs` | 动态内核栈、诊断哨兵 | `PosixTimer` 与进程 timer 表 |

此外复核了自动合并但风险较高的 timer 扫描/信号、DHCP/socket 关闭、Makefile 双架构目标和 README，并恢复交互 Rust shell。

## 8. 从零构建与静态检查

```bash
cd ~/OS/oskernel2026test
git switch on-site-final-2025-fixes
git diff --check
rg -n '^(<{7}|={7}|>{7})( |$)' --glob '!artifacts/**' .
cargo check -p os --target riscv64gc-unknown-none-elf
make kernel-rv
```

RISC-V 上述检查已经通过。构建存在不少历史 warning，但退出码为 0；warning 不等于本轮失败。

双架构完整命令为：

```bash
cargo check -p os --target loongarch64-unknown-none
make kernel-la
make all
```

本机实际在 lwext4 CMake 阶段报告找不到 `loongarch64-linux-gnu-gcc`，所以 `make kernel-la`/`make all` 不能完成。安装交叉 C 工具链后仍需重跑；之后还要有 `qemu-system-loongarch64` 与正确镜像才能做动态验证。

## 9. main 四核回归

Release 三轮命令：

```bash
scripts/validate_smp4.py \
  --kernel kernel-rv \
  --root-disk disk.img \
  --test-disk test_disk.img \
  --suite directed \
  --rounds 3 \
  --build \
  --output-dir artifacts/smp4-validation-onsite-merge-release
```

本轮最终源码的权威 release 证据：

```text
artifacts/smp4-validation-onsite-merge-final-release-v2/MANIFEST.md
Result: 3/3 passed
kernel SHA-256: 3ca6e4ee556e19c4dacd26755f0c8af8a34f4f2f2cb394404248ba8e391403c1
```

每轮要求 `online mask 0xf`、`observed=0xf`，并通过 affinity、clone、futex、mprotect、shm、pipe Iter 4 及两组 signal 测试；不得出现 panic、段错误、lockdep/锁序反转、栈哨兵损坏或调度停滞。

Diagnostics 一轮命令：

```bash
scripts/validate_smp4.py \
  --kernel kernel-rv \
  --root-disk disk.img \
  --test-disk test_disk.img \
  --suite directed \
  --rounds 1 \
  --diagnostics \
  --build \
  --output-dir artifacts/smp4-validation-onsite-merge-diagnostics
```

权威 diagnostics 证据：

```text
artifacts/smp4-validation-onsite-merge-final-diagnostics-v5/MANIFEST.md
Result: 1/1 passed
kernel SHA-256: bb62f0862845f8b8c856fe45a16d6ee2c58c40ef9396a61967e7f4e08940de38
```

名称带 v2/v3/v4 的旧目录是定位串口命令注入竞态时保留的失败证据，不是最终结果。

## 10. 现场赛自动回归

以下命令都会先构建最新 release 内核，并从只读 raw 基础镜像创建临时 qcow2 覆盖层：

```bash
make verify-onsite-2025-vim
make verify-onsite-2025-regressions
make verify-onsite-2025-network
```

基础回归通过条件包括：客体 C 源 SHA-256 正确、`EXT4_RENAME_OK`、`TIMER_REGRESSION_OK`、TTY/ICRNL/原始 CR/DEL/Ctrl-H/ENOTTY 全部成功，且覆盖层无错误。

Vim 回归还会执行真实 Git init/add/commit/log，检查四个软件帮助命令，真正进入交互 Vim 编辑保存，比较截短后的 stat/read 长度，并用 GCC/Rustc 编译运行新文件。不得用 `vim -es`、旧二进制或版本输出替代功能测试，也不得出现 `shorter than expected`、`GIT_LOCAL_FAILED` 或 `VIM_TRUNCATE_STAT_FAILED`。

本轮合并后实测中，Git 真实生成 root commit；Vim 文件 stat/read 均为 81 字节；GCC 与 Rustc 均编译新文件并输出 `Hello, World!`，所有覆盖层检查无错误。

基础回归会检查 termios `ICRNL`：开启时普通回车 `CR` 转成程序所需的 `LF`，关闭时保留原始 `CR`。它还会在规范模式下分别以 `DEL` 和 `Ctrl+H` 作为 `VERASE` 删除前一个字符并检查擦除回显，同时确认非规范模式把 `DEL` 原样交给 Vim 等应用。网络回归会启动只监听 `127.0.0.1:27899` 的一次性服务，检查未绑定和已连接 socket 的 `getsockname`，并检查阻塞与非阻塞 `recv` 在 TCP FIN 后都返回 0。

网络实测标记为 `NETWORK_SOCKNAME_OK address=10.0.2.15 ports=49152,49153` 和 `NETWORK_TCP_EOF_OK`；这同时证明连续连接没有退回按 fd 固定临时端口的旧实现。

完整 Git clone 需要先在一个终端启动宿主代理中继：

```bash
make onsite-2025-proxy-relay
```

中继只监听 `127.0.0.1:27898`，默认转发到宿主 `127.0.0.1:7897`，不记录载荷。每个方向使用最大 32 MiB 的有界缓冲，避免慢速 QEMU 对宿主代理施加长时间反向背压。然后在另一终端运行：

```bash
make verify-onsite-2025-git-clone
```

只有看到 `GETENT_DNS_OK`、`GIT_CLONE_RC=0`、有效提交号、`GIT_CLONE_TREE_OK` 和 `qemu-img check` 无错误才算 clone 通过。

合并后完整 clone 已通过，官方仓库 HEAD 为：

```text
f5dea58cc1057f2b076cdb90b446c2c21d91171e
```

## 11. 手工 push 和 pull 验收

先在终端 A 保持代理中继运行：

```bash
make onsite-2025-proxy-relay
```

在终端 B 启动带持久验收覆盖层和完整日志的 QEMU：

```bash
make run-onsite-2025-rv
```

进入 Rust shell 后挂载 Alpine，并配置由 DHCP 返回的 DNS 和宿主代理：

```sh
/busybox mount -t ext4 vda /mnt
/busybox chroot /mnt /bin/sh
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
printf 'nameserver 10.0.2.3\n' > /etc/resolv.conf
export http_proxy=http://10.0.2.2:27898
export https_proxy=$http_proxy
export GIT_PAGER=cat
```

完整克隆个人仓库并提交。该仓库已经包含前一次验收历史，因此不要再次从官方仓库建立会发生非快进冲突的新 main：

```sh
git clone https://github.com/lyzhcm/xv6-riscv-onsite25.git /tmp/xv6-final
cd /tmp/xv6-final
git config user.name 'lyzhcm'
git config user.email 'lyzhcm@users.noreply.github.com'
printf '\nONSITE_FINAL_PUSH_<唯一标记>\n' >> README
git add README
git commit -m 'Verify onsite 2025 after main merge'
git rev-parse HEAD
git push origin HEAD:main
```

GitHub 用户名输入 `lyzhcm`；密码提示处交互输入仅授予该仓库 Contents 读写权限的 fine-grained PAT。不得把 PAT 写入 URL、脚本、Git 配置或验收日志。push 通过后，在客体执行以下只读核验，网页显示的提交号也必须相同：

```sh
git rev-parse HEAD
git ls-remote origin refs/heads/main
```

随后在 GitHub 网页编辑 README，新增唯一一行 `ONSITE_FINAL_PULL_<唯一标记>` 并提交。回到客体执行：

```sh
git pull origin main
grep -n 'ONSITE_FINAL_PULL_<唯一标记>' README
git log --oneline -3
git status --short
```

只有 pull 退出码为 0、`grep` 找到唯一标记且工作区干净，才算 pull 通过。

Alpine 根分区约 690 MiB，不要同时保留多个完整 xv6 clone；先用 `df -h /tmp` 检查空间。PAT 是 GitHub fine-grained personal access token，不是 GitHub 登录密码，权限只授予该仓库 `Contents: Read and write`。

已有合并前手工证据：

- guest push 提交：`77e9917d143ce64d015661a243c0cb23cc3c2f84`。
- 网页修改后 pull 提交：`592ebfc89a1f771cfa1d4fa161d3d767e5fc4f67`。
- 标记：`ONSITE_FINAL_PUSH_20260814_PORTFIX_OK`、`ONSITE_FINAL_PULL_20260814_PORTFIX_OK`。
- 证据目录：`artifacts/onsite2025/20260814T104036Z/`。

这证明现场赛网络和认证路径完整成功过，但严格的最终交付仍需对当前合并后 HEAD 再手工跑一次。本自动化过程不能代替用户输入 PAT，也不会保存 PAT。

## 12. 100 分计分点核对

| 计分点 | 分值 | 当前证据 |
|---|---:|---|
| `git -h` | 5 | 合并后自动通过 |
| Git init/add/commit/log | 20 | 合并后自动通过，真实对象和工作区已检查 |
| 完整 Git clone | 10 | 合并后自动通过 |
| Git push | 10 | 合并前手工通过；合并后应再手工验收 |
| Git pull | 10 | 合并前手工通过；合并后应再手工验收 |
| `vim -h` | 5 | 合并后自动通过 |
| Vim 交互编辑保存 | 10 | 合并后自动通过，含 truncate/stat 一致性 |
| `gcc --help` | 5 | 合并后自动通过 |
| GCC 编译运行 | 10 | 合并后自动通过 |
| `rustc -h` | 5 | 合并后自动通过 |
| Rustc 编译运行 | 10 | 合并后自动通过 |

版本字符串或帮助输出只能覆盖“加载”计分点，不能替代实际编辑、编译、clone、push 或 pull。

## 13. 证据与退出

`make run-onsite-2025-rv` 会在 `artifacts/onsite2025/<UTC 时间>/` 保存基础镜像哈希、内核哈希、当前提交号、QEMU 命令、串口日志、pcap 和 qcow2 覆盖层。退出 QEMU 使用 `Ctrl-A` 后按 `X`。最后检查对应覆盖层：

```bash
qemu-img check artifacts/onsite2025/<UTC 时间>/alpine-overlay.qcow2
```

通过输出必须包含 `No errors were found on the image.`。手工证据 `20260814T104036Z` 已得到该结果。退出 QEMU 是先按 `Ctrl+A`，松开后再按 `X`，不要把两个键同时按下。

## 14. 常见故障排查

### `disk.img: Permission denied`

检查是否有旧 QEMU 仍打开磁盘、文件权限，以及当前入口是否直接写 raw。同一镜像不能由两个 QEMU 同时写；现场赛入口应使用独立 qcow2 覆盖层。

### Git/Vim 显示众多文件修改

先运行 `git status --short` 和 `git diff --stat`。构建可能更新 lwext4 静态库，日志、镜像和覆盖层也不应提交。只 add 明确的源码/文档，不使用未经检查的 `git add -A`。

### `No space left on device`

执行 `df -h /tmp`。每轮只保留一个 clone；最可靠的恢复方式是退出并从干净覆盖层重开，而不是在 99% 满的文件系统上继续测试。

### Git 网络卡住或超时 300 秒

依次检查：内核 DHCP 日志、`/etc/resolv.conf`、宿主代理、`make onsite-2025-proxy-relay` 是否仍在运行、客体两个 proxy 环境变量。先通过 network 回归，再做完整 clone。

### `getent` 触发 `read page fault outside a valid user task`

若 `sepc` 落在 `sys_connect` 且 `bad` 是类似 `0x1000091558` 的高位用户地址，说明网络系统调用直接解引用了用户 sockaddr。提交 `0b9ef744` 已改为经任务页表安全复制。复测必须同时出现 `GETENT_DNS_OK`、完整 clone 成功和网络回归的 `NETWORK_SOCKNAME_OK`/`NETWORK_TCP_EOF_OK`，不能只确认内核不 panic。

### Git 用户名提示后 Enter 无反应

普通 Enter 应经 `ICRNL` 转成 LF。确认使用最新内核且 TTY 回归通过。VS Code 可能截获 `Ctrl+J`，不要用它替代 Enter。

### Shell 的 `read` 第一次读到空串

粘贴多行命令或 chroot 前可能残留换行。用一行完整命令测试，并在提示后输入内容再只按一次 Enter：

```sh
printf 'Username test: ' >/dev/tty; read -r name </dev/tty; printf '\nRESULT=<%s>\n' "$name"
```

### Vim 重复渲染、错位或不清屏

进入 Alpine 后执行 `export TERM=xterm-256color` 和 `stty rows 24 cols 80`。自动驱动必须逐字节节流；一次向 PTY 注入大量按键会造成 UART 丢字节和重绘异常。

### GCC 报 `shorter than expected`

比较 `stat -c %s file` 和 `wc -c < file`。若复制文件后恢复，检查 5.3 节的 pending-write、truncate 和 stat flush 路径。

### Git 报 `Operation not permitted` 或对象损坏

普通 `printf`/`mv` 成功不足以证明 Git 可用。分别检查覆盖 rename、hard-link 发布和真实 `git init/add/commit/log`。

### 四核验证启动错误架构程序

RISC-V 定向回归必须用 `--root-disk disk.img`，不要使用当前含 LoongArch `/bin/user_shell` 的 `fs-img.img`。

## 15. 提交前检查与未完成项

```bash
git diff --check
rg -n '^(<{7}|={7}|>{7})( |$)' --glob '!artifacts/**' .
git status --short
git diff --stat
git log --oneline --decorate --graph -12
```

确认没有冲突标记、PAT/密码/认证头，没有把 `.img`、qcow2、pcap、串口日志或 `artifacts/onsite2025/` 加入 Git。当前还需：

1. 对合并后 HEAD 手工重跑一次 GitHub push/pull。
2. 安装 LoongArch C 交叉工具链后重跑静态构建。
3. 准备 LoongArch QEMU/镜像后做动态回归。
4. 获得官方决赛镜像后单独完成 CAgent/BuildStorm 长测。
5. 用户明确授权后，才把 `8803ab3` 之后的 sockaddr fix/docs 提交 push 到远程。
