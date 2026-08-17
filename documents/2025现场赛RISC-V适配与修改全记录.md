# 2025 现场赛 RISC-V 适配与修改全记录

本文记录 ScintillaOS 从原始基线 `90803c5ed151634064f2523b414e61e142a77b66` 到现场赛分支 `on-site-final-2025-fixes@a236e1e` 的完整适配过程。它回答六个问题：原系统为什么不能直接完成 2025 现场赛；每次故障怎样复现和定位；内核实际修改了什么；这些修改为什么符合 Linux 语义；合并最新 `main` 时怎样保留两侧成果；哪些结果已经有证据，哪些结果仍不能声称通过。

本文侧重技术原理和个人复盘。需要直接启动 QEMU、输入命令或按计分顺序验收时，请配合阅读《[现场赛2025 RISC-V 验收指南](现场赛2025_RISCV验收指南.md)》。

## 1. 最终结论和验证边界

2025 现场赛题目不是要求参赛者重新编写 Git、Vim、GCC 和 Rustc。官方提供的 RISC-V Alpine Ext4 镜像已经包含这些未经项目修改的 Linux 用户程序、musl 动态链接器和依赖库。参赛操作系统的任务是提供足够准确的 Linux ABI、Ext4、TTY、信号、计时器和网络语义，让这些程序真实完成工作。

本分支已经在 QEMU RISC-V64 上动态验证：Git 帮助、本地仓库和完整 HTTPS clone，Vim 帮助和真实交互编辑，GCC/Rustc 帮助、编译及运行均可完成。GitHub push/pull 也曾在同一套现场赛实现上手工完成并验证远端副作用；但合并 `main` 后的当前提交若要形成严格的“本提交 100 分”证据，仍应再次交互输入 PAT 重跑 push/pull。验证不是只看程序能启动，也检查退出码、输出、Git 工作树和提交、Vim 保存后的文件字节数、编译产物以及远端仓库副作用。

结论有以下严格边界：

- 现场赛功能的完整动态证据属于 QEMU RISC-V64。
- 现场赛最初以单核运行，合并后又在四核下完整复测；四核启动可见 `online mask 0xf`。
- 脚本参数当前接受 `QEMU_SMP=1` 到 `12`，但 RISC-V 的 `PLATFORM_CPUS` 是 `8`，所以这不等于 RISC-V 支持十二个在线 hart。当前完整现场赛动态验证只做到四核。
- 合入 `main` 后，RISC-V 四核定向回归 release 通过 3/3，diagnostics 通过 1/1。这是合并兼容性的证据，不应倒推为现场赛分支原创了 `main` 的 SMP、BuildStorm 或双架构优化。
- 当前环境缺少完整 LoongArch QEMU、工具链、官方镜像和实体板卡，本文不声称 LoongArch 动态通过。
- BuildStorm/CAgent 是后续决赛工作，不属于本次现场赛验收；不能拿 Alpine 镜像替代对应决赛镜像后声称通过。

## 2. 版本范围、归因和恢复点

### 2.1 关键提交

| 角色 | 提交 | 说明 |
| --- | --- | --- |
| 原始基线 | `90803c5ed151634064f2523b414e61e142a77b66` | 尚未完成本次现场赛专属适配 |
| 现场赛集中提交 | `a82b4ab9baeeb6238142538e302c5b3023aab66b` | POSIX timer、TTY、Ext4、网络及验证基础设施；合并前已形成恢复点 |
| 合入的主线 | `ed815ffc003ccc567d7b4b8da1fcdca349d12905` | `main` 当时最新状态，包含 SMP、BuildStorm、VFS 等后续优化 |
| 显式合并提交 | `41462075aa60004260892ce410d67f7fa81b6faa` | 双亲 merge commit，未 rebase、未 squash |
| 合并后文件修复 | `0d7dd76be39891f1818c595062e52e752b4b7690` | pending write、文件长度和验证器稳定性 |
| 四核证据 | `8803ab33cc0301a5c47d81a7704be0f8b47ad883` | 保存 release/diagnostics 结果 |
| 用户地址修复 | `0b9ef7445dc13703442b94f5bf03b6c68488a8dd` | 修复高位用户 `sockaddr` 被内核直接解引用 |
| 可选 SMP | `bd87f185ae377f411ecf70481d2690a976a1655c` | 现场赛启动和验证入口接受显式核数 |
| 本文描述终点 | `a236e1e` | 包含四核手工验证资料的当前分支状态 |

从 `90803c5` 到合并前 `a82b4ab` 的差异为 29 个文件，约 2878 行新增、192 行删除。这个范围最适合判断“为了现场赛专门改了什么”。`4146207` 之后的改动必须另行归因，不能把合入的 `main` 内容算作现场赛原创成果。

### 2.2 三类内容必须分开

```text
90803c5 原始系统
   │
   ├── 现场赛专属适配 ─────────────── a82b4ab
   │      timer / TTY / Ext4 / 网络 / 验证器
   │
main@ed815ffc ────────────────────────┐
   │      SMP / BuildStorm / VFS / 锁等主线优化 │
   └────────────────────────────── 4146207 merge
                                           │
                                           ├── 合并语义修复与安全复制
                                           └── a236e1e 当前状态
```

本文正文以第一类现场赛专属适配为主体；第 11 章单独说明 `main` 合并和合并后修复；第 12 章再说明验证基础设施。这样能够避免两种常见误判：把自动测试脚本当成内核功能，或者把主线已有能力当成现场赛新实现。

## 3. 赛题计分点与它们真正依赖的内核能力

| 计分点 | 表面命令 | 真正依赖的内核语义 | 严格通过条件 |
| --- | --- | --- | --- |
| Git 加载 5 | `git -h` | ELF、动态链接、文件和终端基础 ABI | 退出码 0 且显示帮助 |
| Git 本地 20 | `init/add/commit/log` | Ext4 创建、覆盖 rename、链接、时间、权限、锁文件发布 | 每步成功，提交存在，工作区干净 |
| Git clone 10 | 完整 clone | VirtIO 网卡、DHCP/DNS、UDP、TCP、poll、TLS 所需 socket ABI | 完整工作树和有效 `HEAD` |
| Git push 10 | 推送个人仓库 | 连续 TCP 连接、TTY 凭据输入、HTTPS、远端副作用 | GitHub 网页 commit hash 一致 |
| Git pull 10 | 网页修改后 pull | fetch、对象写入、覆盖 rename、快进更新 | 本地出现网页唯一标记 |
| Vim 加载 5 | `vim -h` | 动态链接和基础终端能力 | 退出码 0 |
| Vim 编辑 10 | `vim hello.c` | POSIX timer、termios、窗口大小、规范/非规范输入、truncate/write/stat | 人工插入、保存、退出，内容和长度正确 |
| GCC 加载 5 | `gcc --help` | 进程、文件、管道、内存基础 ABI | 退出码 0 |
| GCC 编译 10 | C Hello World | fork/exec/wait、临时文件、链接器、Ext4 一致性 | 新产物输出正确且退出码 0 |
| Rustc 加载 5 | `rustc -h` | 同上，并涉及更复杂的 mmap/线程/文件访问 | 退出码 0 |
| Rustc 编译 10 | Rust Hello World | 编译器和链接器全链路 | 新产物输出正确且退出码 0 |

版本输出只说明程序入口运行了一小段代码，不能证明 Git 原子更新、Vim 交互、编译器创建产物或网络协议已经正确。因此本项目采用“命令成功、结果正确、副作用可验证”三个同时成立的标准。

## 4. 运行环境：Alpine、BusyBox、双盘和 chroot

### 4.1 Alpine 镜像是什么

`alpine-linux-riscv64-ext4fs.img` 是一个 RISC-V64 用户空间根文件系统。它包含 `/bin`、`/usr/bin`、musl libc、动态链接器以及 Git、Vim、GCC、Rustc 等工具。它不是第二个内核，也不是把本项目替换成 Alpine Linux。QEMU 的 `-kernel` 参数始终加载本项目构建出的内核，Alpine 程序发出的每个 syscall 最终仍由 ScintillaOS 处理。

### 4.2 BusyBox 为什么出现

项目自己的测试盘提供 `/busybox`。BusyBox 是一个多调用用户程序，同一二进制可扮演 `mount`、`chroot`、`sh` 等命令。它在这里承担“救援工具箱”和进入 Alpine 的引导角色；它不是现场赛需要重新实现的目标，也不是另一个操作系统。

```text
QEMU virt
├── CPU：执行 ScintillaOS kernel-rv
├── vda：Alpine 基础镜像的临时 qcow2 覆盖层
├── vdb：项目 disk.img，含 Rust shell、BusyBox 和回归程序
└── VirtIO net：QEMU SLIRP 用户网络

Rust user shell
  └── /busybox mount -t ext4 vda /mnt
      └── /busybox chroot /mnt /bin/sh
          └── Alpine Git / Vim / GCC / Rustc
```

`mount` 把 vda 接到 `/mnt`；`chroot` 只改变当前进程看到的根目录。进入 chroot 后，程序和动态库来自 Alpine，内核、设备、进程调度和网络栈仍来自本项目。

### 4.3 为什么要用覆盖层

早期直接使用 raw 镜像时，旧文件、错误仓库和上次 Vim 保存结果会污染下一轮，甚至可能损坏官方基准。现在每轮基于只读 raw 镜像创建 qcow2 覆盖层：

```text
官方 raw 镜像（保持不变）
          │ backing file
          ▼
本轮 alpine-overlay.qcow2（所有写入只落这里）
```

退出后可以用 `qemu-img check` 验证覆盖层结构，失败时保留它和完整串口日志复盘；下一轮重新创建覆盖层即可获得干净环境。

## 5. 排查时间线：从现象到修复

本节按实际认识过程记录。每个问题都采用“现象—最小复现—证据—根因—修复—结果”结构。

### 5.1 双盘启动时 `disk.img: Permission denied`

**现象。** QEMU 已能读取 Alpine 镜像，却在第二块盘 `disk.img` 上报 `Could not open 'disk.img': Permission denied`。

**最小复现。** 运行旧的 `make run-dual-prebuilt`，确认 QEMU 命令中 vda 指向 Alpine、vdb 指向项目 `disk.img`。

**定位证据。** 报错发生在宿主 QEMU 打开文件阶段，客体内核尚未接管该设备，因此不是 Ext4 或 VirtIO 驱动错误。它暴露了旧入口依赖当前目录中已有 `disk.img`、权限和旧构建产物的问题。

**最终处理。** 新现场赛入口先构建最新 `kernel-rv` 和测试盘，校验输入文件，再使用明确的双盘参数启动；Alpine 使用新覆盖层。这样既避免旧内核，也把宿主文件问题与客体内核问题分开。

### 5.2 Rust user shell 中直接执行 Alpine 命令会 panic

**现象。** 在 `Rust user shell` 中直接输入 `gcc` 或生成的 `/tmp/hello-c`，`user_shell.rs` 落入 `unreachable`；进入 Alpine chroot 后同样的 GCC 和 Rustc 编译却能成功。

**根因。** 项目 Rust shell 是用于测试盘内置程序的轻量命令分发器，不是完整 POSIX shell。它只认识预置用户程序，并不能替代 Alpine `/bin/sh` 的路径搜索、重定向和任意 ELF 执行流程。

**正确方式。** 先用 `/busybox mount` 挂载 vda，再用 `/busybox chroot /mnt /bin/sh`。此后设置 Alpine 的 `PATH`，所有计分命令都在 `/bin/sh` 中执行。这个现象不是 GCC 本身失败，也不应通过给 Rust shell硬编码 `gcc`、`git` 等程序名解决。

### 5.3 Git `init/config` 报 `Operation not permitted`

**现象。** 普通文件可创建，`.git/manual.lock` 也能写入并改名；但 `git init` 或 `git config` 更新已有 `.git/config` 时失败。最小复现进一步显示：

```sh
printf 'old\n' > .git/replace
printf 'new\n' > .git/replace.lock
mv -f .git/replace.lock .git/replace
# mv: Operation not permitted
```

**定位证据。** 新文件移动到不存在的目标成功，覆盖已存在目标失败，说明不是目录权限，也不是 Git 身份配置。Git 使用“写临时 lock 文件—fsync/close—覆盖 rename 到正式文件”的原子发布协议，正好触发缺失的 Ext4 覆盖 rename 语义。

**根因。** VFS/Ext4 rename 路径没有完整处理目标已存在时的替换、目录项和 inode 生命周期；hard link 和缓存写回也有相似发布顺序风险。

**修复。** 补齐覆盖 rename、目标回收、hard link 前的源文件刷新和 mutation 期间的 pending write 处理。Git 不再被特殊识别；修复的是所有程序共同使用的文件系统原子更新语义。

**结果。** `git init/add/commit/log` 全部成功，覆盖 rename C 回归输出 `EXT4_RENAME_OK`，完整 clone/push/pull 也能发布对象和引用。

### 5.4 Vim 报 `E1286: Could not set timeout`

**现象。** `vim -h` 可以显示帮助，但真实 `vim hello.c` 报 `E1286: Could not set timeout: Function not implemented`。

**最小复现。** 编写小程序只调用 `timer_create`、`timer_settime` 并等待信号，能够稳定得到 `ENOSYS`，排除 Vim 配置和文件内容影响。

**根因。** Vim 的交互事件和超时路径使用 POSIX timer；原内核没有实现 Linux 对应的五个 timer syscall。帮助输出没有走到该路径，所以“Vim 能加载”不能替代“Vim 能编辑”。

**修复。** 实现进程级 POSIX timer 表、五个 syscall、单次/周期/绝对时间、到期信号投递和生命周期清理。回归先证明 timer 语义，再使用真实交互 Vim 验收；没有使用 `vim -es` 绕开终端和 timer。

### 5.5 Enter、退格和 Git 凭据提示异常

**现象。** Git 卡在 `Username for ...`，普通 Enter 不结束输入；用 `Ctrl+J` 可能触发 VS Code 快捷键关闭终端。退格产生奇怪字符或不能删除，用户名甚至混入 `%2C%7F...`。

**最小复现。** 在 Alpine shell 中执行 `read -r name </dev/tty`，分别输入 CR、LF、DEL 和 Ctrl-H；再切换 Vim 非规范模式观察原始按键。

**根因。** 旧控制台只是简单字节流，没有持久 termios、规范行缓冲、`ICRNL` 转换和 `VERASE` 处理。宿主终端常把 Enter 发送为 CR；shell 规范模式期待换行完成一行。DEL 被当普通字节后会污染 Git 用户名。

**修复。** 实现 `/dev/tty`、termios、规范输入队列、CR/LF 映射、DEL/Ctrl-H 擦除及正确回显，并在非规范模式把字节原样交给 Vim。VS Code 是否捕获 Ctrl+J 属于宿主 UI 行为，不再被当作客体内核回车方案。

**结果。** `TTY_ICRNL_OK` 和 `TTY_ERASE_OK` 通过；Git 可用普通 Enter 提交用户名和 PAT，Vim 也能接收方向键和编辑命令。

### 5.6 Vim 屏幕重复渲染

**现象。** Vim 内容和 `~` 行重复出现在串口历史中，看起来像屏幕不断叠加。

**分析。** `-nographic` 的宿主终端是串口字节流，终端转义序列只有在 `TERM`、窗口尺寸和控制台 ioctl 一致时才能被宿主正确解释。若 `TERM` 缺失、`TIOCGWINSZ` 不正确或输出被并发打散，Vim 的“移动光标后覆盖”会在日志里表现为重复文本。

**修复和操作配合。** 内核提供窗口大小、termios 和串行化控制台输出；用户态设置 `TERM=xterm-256color`、`stty rows 24 cols 80`。验收以实际可编辑、保存内容和退出为准，同时保留串口历史供复盘，不把转义序列后的日志外观误判为文件内容重复。

### 5.7 Vim 保存后 `stat=71`、实际读取 69 字节

**现象。** `cat file | wc -c` 得到 69，`stat -c %s file` 却得到 71；GCC 警告 `file is shorter than expected`。复制文件后，新文件大小恢复为 69，说明源文件 inode 长度陈旧。

**最小复现。** 先创建较长文件，再用 Vim 截短并保存，分别比较 `stat`、`wc -c`、复制件和 GCC 行为。

**根因。** VFS 缓存的 inode size 与 lwext4 中实时 `ext4_fsize` 在 truncate/pending write 后没有同步。读取按真实 EOF 返回 69 字节，但 `stat` 报告缓存中的 71；编译器根据 stat 预期读取更多，因此发出 stale-size 警告。

**修复。** mutation 和 stat 前按正确锁顺序刷新 pending writes，使用实时 `ext4_fsize` 作为打开 Ext4 文件长度的权威来源，并调用 `refresh_handle_size` 更新缓存；truncate 提交新长度。修复中特别避免持有外层 VFS 锁时再次嵌套获取文件/设备锁。

**结果。** Vim 保存后 `stat` 与 `wc -c` 一致，GCC 不再警告，自动验证输出 `VIM_TRUNCATE_STAT_OK`。

### 5.8 DNS 恢复后 Git 仍在 `getsockname()` 失败

**现象。** 官方镜像初始 `/etc/resolv.conf` 为空。写入 `nameserver 10.0.2.3` 后 `getent` 能解析 GitHub，但 Git HTTPS clone 曾报 `getsockname() failed with errno 22`；另一阶段访问网关 TCP 会长时间阻塞。

**分层定位。** 使用 QEMU `filter-dump` 保存 pcap，按 DHCP、ARP、DNS、TCP 顺序观察，而不是直接把所有失败归为“没网”：

```text
VirtIO TX/RX
   ↓
DHCP 地址、网关、DNS
   ↓
ARP 到 10.0.2.2
   ↓
DNS UDP 到 10.0.2.3
   ↓
TCP connect / poll / SO_ERROR
   ↓
TLS 与完整 Git smart HTTP
```

**根因集合。** VirtIO RX 曾把 virtio-net header 一并交给 smoltcp，导致协议层起始位置错误；DHCP 配置事件未完整发布到接口；socket 使用按 fd 推导的端口会在连续连接时冲突；未绑定 socket 的 `getsockname`、非阻塞 connect、`SO_ERROR`、TCP EOF、UDP 源地址和 close 清理均不完整。

**修复。** RX 使用数据包区域而不是包含设备头的整个 buffer；消费 DHCP event，更新 IP 和默认路由；全局分配临时端口；补齐 socket 状态查询、poll 唤醒、UDP 来源、`MSG_PEEK`、EOF 和关闭清理。`/etc/resolv.conf` 仍由用户态测试覆盖层写入，内核不偷偷修改官方镜像。

### 5.9 `getent` 触发内核高地址页故障

**现象。** 合入 `main` 后，`getent ahostsv4 github.com` 曾触发：

```text
[fault-diag] ... user=false access=read ... bad=0x1000091558
read page fault outside a valid user task
```

**定位证据。** fault 发生在内核态，坏地址位于用户高地址空间，说明 syscall 路径把用户指针转换为 Rust 引用后直接解引用。它不是用户程序自己的非法访问，因此不能简单给进程发 `SIGSEGV`。

**根因。** 某些 socket 地址输入/输出路径绕过页表感知的用户复制辅助函数。低地址或恰好连续映射时可能侥幸工作，高位 `sockaddr` 才稳定暴露问题。

**修复。** `sys_connect` 等输入路径用 `copy_user_value` 拷贝固定大小结构，输出路径用 `write_user_value` 或等价的任务用户空间复制；先验证长度和地址族，再访问内核副本。无效用户地址返回 `EFAULT`，不能升级为内核 panic。

**结果。** `getent`、DNS socket 回归和完整 clone 均再次通过。这一修复属于合并后安全边界修复，提交为 `0b9ef74`。

## 6. POSIX timer 与信号实现

### 6.1 数据模型

每个进程在 `TaskControlBlock` 中维护独立 POSIX timer 表。核心记录 `PosixTimer` 保存：

- `clock_id`：`CLOCK_REALTIME` 或 `CLOCK_MONOTONIC`；
- `signal`：到期时投递的信号；
- `target_tid`：进程信号或 `SIGEV_THREAD_ID` 的指定线程；
- `value`：随信号携带的 `sigval`；
- `expire_us`：下一次到期的单调时钟微秒值；
- `interval_us`：周期；零表示单次 timer；
- `overrun`：扫描晚于截止时间时累计的超期次数。

```text
timer_settime
   │ 用户 itimerspec
   ▼
校验并换算为单调时钟 deadline
   │
   ▼
进程 posix_timers 表
   │ 调度/时钟路径 check_itimers 扫描
   ├── 未到期：保留
   └── 已到期：PendingSignalInfo → 目标线程/进程 → 唤醒
                     └── 周期 timer 计算下一 deadline
```

### 6.2 五个 syscall

`os/src/syscall/process.rs` 实现并由 syscall 分发接入：

- `timer_create`：校验 clock 和 sigevent，分配进程内 timer ID；空 sigevent 默认投递 `SIGALRM`。
- `timer_settime`：支持相对时间和 `TIMER_ABSTIME`，返回旧值时安全写回用户地址。
- `timer_gettime`：报告距下一次到期的剩余时间和周期。
- `timer_getoverrun`：返回上一次投递关联的 overrun。
- `timer_delete`：删除 timer，后续访问该 ID 返回错误。

支持 `SIGEV_SIGNAL`、`SIGEV_NONE` 和 `SIGEV_THREAD_ID`。Realtime 绝对时间先按当前 realtime 与 monotonic 的偏移换算，内部统一保存 monotonic deadline，避免墙上时钟表达和调度时基混用。

### 6.3 到期、周期和生命周期

`check_itimers` 在已有定时扫描和任务唤醒机制中工作。若一次扫描跨过多个周期，不是把下一截止时间简单设为“当前时间加周期”，而是计算错过的 expirations、更新 overrun，并把下一 deadline 推进到未来，避免长期漂移。

timer 的所有权属于进程：fork/clone 创建的新进程 timer 表为空，exec 清理旧程序 timer，exit 同样释放；目标线程不存在时返回或投递对应错误语义。这样不会让旧地址空间建立的异步事件泄漏到新程序。

### 6.4 错误边界

非法 clock、flags、时间值、sigevent 类型和 timer ID 返回 `EINVAL`；坏用户指针返回 `EFAULT`；指定线程不存在返回 `ESRCH`；ID/槽位耗尽返回 `EAGAIN`。这些 errno 是用户库判断是否降级或失败的协议，不能一律返回 `-1`。

### 6.5 对应回归

`tests/onsite2025/timer_regression.c` 覆盖单次、周期、绝对时间、删除、非法参数和信号唤醒。自动验证要求出现 `TIMER_REGRESSION_OK hits=4`。之后还必须真实启动 Vim，因为小程序通过只证明 timer，不证明终端和文件系统组合正确。

## 7. TTY、termios 与控制台语义

### 7.1 规范输入不是简单 `getchar`

控制台状态由 `ready` 队列和 `canonical` 行缓冲组成：

```text
UART 输入字节
   │
   ├── 输入映射：IGNCR / INLCR / ICRNL
   │
   ├── ICANON=1
   │      ├── VERASE：DEL 或配置字符，删除前一字节
   │      ├── VKILL：清除整行
   │      └── 收到换行：canonical 整行发布到 ready
   │
   └── ICANON=0：字节直接进入 ready
              ↓
       read(/dev/tty)
```

规范模式下，一行在换行前不能过早交给 shell；非规范模式下，Vim 必须及时收到按键和转义序列。规范缓冲设有边界，防止无限输入占用内核内存。

### 7.2 Enter 和退格

默认 `ICRNL` 把 CR 转成 LF，因此宿主普通 Enter 能完成 shell/Git 的一行。`VERASE` 默认 DEL，同时兼容 Ctrl-H；开启 `ECHOE` 时擦除回显为“退格、空格、退格”，屏幕和内核缓冲同步。`ECHO`、`ECHOK`、`ECHONL` 决定普通字符、kill 和换行是否回显。

这个实现解释了早期的两个表象：空 `read` 是 CR 没被转换成行结束；Git 用户名中的 `%7F` 是 DEL 被当作数据送进了 URL 编码路径。

### 7.3 `/dev/tty` 和 ioctl

`/dev/tty` 与 `/dev/console` 作为字符设备打开和 stat。全局控制台状态持久保存 termios、前台进程组和默认 `24x80` 窗口。实现的关键 ioctl 包括：

| 类别 | ioctl | 语义 |
| --- | --- | --- |
| termios | `TCGETS` | 读取当前 termios |
| termios | `TCSETS/TCSETSW/TCSETSF` | 立即、等待输出或刷新后设置 |
| 窗口 | `TIOCGWINSZ/TIOCSWINSZ` | 查询/设置行列数 |
| 作业控制 | `TIOCGPGRP/TIOCSPGRP` | 查询/设置前台进程组 |
| 队列 | `FIONREAD` | 查询当前可读字节数 |
| 非阻塞 | `FIONBIO` | 设置 fd 非阻塞状态 |
| 兼容 | `TCSBRK/TCFLSH/TIOCSCTTY/TIOCNOTTY` | 提供程序需要的控制语义 |

TTY ioctl 对普通文件和管道返回 `ENOTTY`，坏用户指针返回 `EFAULT`，无效前台进程组返回 `EINVAL`。不能为了让 Vim 继续而对所有 fd 假装成功。

### 7.4 阻塞、非阻塞和多核输出

读取规则是：有数据立即返回；`O_NONBLOCK` 且无数据返回 `EAGAIN`；阻塞读取使用任务睡眠/唤醒路径等待。合并 `main` 后保留其定时睡眠和调度优化，没有退回忙等。控制台输出还使用 owner/busy 序列化，避免多核下不同任务的 ANSI 转义序列互相穿插。

### 7.5 对应回归

- `tty_regression.c`：termios、窗口、`FIONREAD`、非 TTY 的 `ENOTTY`，标记 `TTY_REGRESSION_OK`。
- `tty_icrnl_regression.c`：CR/LF 和擦除，标记 `TTY_ICRNL_OK`、`TTY_ERASE_OK`。
- 人工 Vim：必须在交互界面插入、保存、退出，不能用 `vim -es` 代替。

## 8. Ext4/VFS：让 Git 和 Vim 的文件语义可靠

### 8.1 Git lock 文件发布

Git 不直接覆盖重要文件，而是先写 `config.lock`、`index.lock` 或引用 lock，完成后用 rename 原子替换正式文件：

```text
write new contents → fsync/close lock → rename(lock, target)
                                            │
                         目标存在时仍必须原子替换
```

原实现只支持无目标 rename，因此目录看似可写，Git 仍报 `EPERM`。修复覆盖目标的目录项替换和 inode 生命周期后，Git 的原子协议自然工作。没有针对 `.git` 路径或进程名作特判。

### 8.2 hard link 和 pending write

Git 对象和工具链会组合使用链接、rename、truncate 与延迟写回。发布 hard link 前必须保证源 inode 的 pending data 已落到 Ext4 可见状态，否则新目录项可能指向元数据已存在但内容尚不一致的 inode。mutation 期间使用专门的刷新路径，避免普通 flush 再次取得已持有锁。

### 8.3 文件长度的权威来源

打开的 Ext4 文件可能同时有 VFS 缓存 size、file handle size 和 lwext4 `ext4_fsize`。truncate 后真实 EOF 已变化而缓存尚未刷新，便出现 `stat=71`、读取 69 的矛盾。

最终规则是：在需要对外承诺长度的 stat/truncate 路径先提交 pending writes，再以实时 `ext4_fsize` 为权威，通过 `OSInode::refresh_handle_size` 同步缓存。锁顺序保持 VFS/mutation、文件句柄、lwext4 设备的一致方向，避免用“多加一把锁”换来死锁。

### 8.4 回归对应

- 覆盖 rename：旧目标内容必须被新文件替换，lock 不残留，输出 `EXT4_RENAME_OK`。
- Git 本地流程：提交存在且工作区干净，输出 `GIT_LOCAL_OK`。
- Vim 截短：`stat`、`wc -c` 和实际内容完全一致，输出 `VIM_TRUNCATE_STAT_OK`。
- GCC：编译同一文件不得再出现 `shorter than expected`，新生成程序退出码为 0。

## 9. VirtIO、DHCP 与 socket 全链路

### 9.1 VirtIO 数据边界

VirtIO net 接收缓冲前部含设备协议头，后面才是 Ethernet frame。旧代码把 `rx_buf.as_bytes()` 的全部内容交给 smoltcp，网络栈便把 virtio header 误当成 Ethernet 目的 MAC。修复后使用 `rx_buf.packet()`，确保 smoltcp 看到的第一个字节就是 Ethernet frame。

```text
旧： [virtio_net_hdr | Ethernet | IP | UDP/TCP] ──全部──> smoltcp（错误）
新： [virtio_net_hdr | Ethernet | IP | UDP/TCP]
                     └──────── packet() ────────> smoltcp
```

这类错误可能让 TX 看似有通知、驱动也有 RX 中断，但 ARP/DHCP 无法正确解析，必须结合 pcap 判断包到达了哪一层。

### 9.2 DHCP 和用户态 DNS

smoltcp DHCP event 被消费后，内核更新接口 IPv4 地址和默认路由，并打印：

```text
[net] DHCP address=10.0.2.15/24 gateway=Some(Address([10, 0, 2, 2]))
[net] DHCP DNS=10.0.2.3
```

DHCP 尚未完成时才使用 QEMU SLIRP 的回退值：地址 `10.0.2.15/24`、网关 `10.0.2.2`、DNS `10.0.2.3`。`/etc/resolv.conf` 属于用户态发行版配置，测试覆盖层显式写入 `nameserver 10.0.2.3`；内核不会暗中篡改官方 raw 镜像。

### 9.3 临时端口和连续连接

早期按 fd 推导本地端口，fd 被重用后新连接容易撞上旧 socket 的四元组和 TIME_WAIT 状态。改为全局 `NEXT_EPHEMERAL_PORT` 原子分配，从 49152 开始递增；测试连续 socket 应观察不同端口，例如 `49152,49153`。这也是 clone 后继续 push/pull 不应无故卡死的前提。

### 9.4 Linux socket 兼容语义

现场赛补齐或修正：

- 未显式 bind 的 socket 在自动选端口后，`getsockname` 返回真实本地地址和端口；
- `getpeername` 返回已连接对端，未连接状态返回正确错误；
- 非阻塞 `connect` 进入进行中状态，`poll/ppoll` 在完成或失败时唤醒；
- `getsockopt(SO_ERROR)` 消费连接错误，而不是固定返回成功；
- UDP `recvfrom/recvmsg` 返回数据报真实来源，DNS 响应不能误报成本地绑定地址；
- `MSG_PEEK` 查看数据但不消费；
- 对端正常关闭后 TCP `recv` 返回 0 表示 EOF；
- close/abort 从 socket set 清理 handle 和本地 endpoint，并唤醒等待者。

### 9.5 代理中继的角色

QEMU SLIRP 让客体看到宿主网关 `10.0.2.2`。验证环境中，宿主 `scripts/onsite_2025_proxy_relay.py` 可把 `127.0.0.1:27898` 转发到已有代理 `127.0.0.1:7897`，客体设置：

```sh
export http_proxy=http://10.0.2.2:27898
export https_proxy=$http_proxy
```

中继只解决实验环境的外网出口，不替代客体 TCP/TLS：Git 仍在客体中建立连接、执行 HTTPS 和写完整对象库。中继限制单方向缓存，不记录凭据或 payload。若现场网络无需代理，应直接使用可达出口。

## 10. 安全用户内存复制

系统调用传入的地址是用户虚拟地址，内核不能把它强制转换为 `&SockAddr` 后直接读写。用户页可能位于高地址、跨页、未映射、只读，或在当前 hart 内核页表中不能直接访问。

```text
错误：user_ptr ── cast ──> Rust reference ──内核直接访存──> kernel page fault

正确：user_ptr
        ├── 长度/范围校验
        ├── 按任务页表 copy_user_value
        ▼
      内核局部副本 ──解析和使用
        │
        └── write_user_value ──> 用户输出缓冲
```

`0b9ef74` 把网络地址输入改为 `copy_user_value`，输出改为 `write_user_value` 或同级复制函数。解析只针对内核副本进行；复制失败返回 `EFAULT`。这一边界不仅修复 `getent`，也防止恶意或异常程序借 socket syscall 让整个内核 panic。

合入 `main` 后的 `net/syscall.rs` 已有更统一的安全用户缓冲准备逻辑，冲突处理以它为主体，再保留现场赛 socket 语义，避免一次 syscall 中重复复制或把任务锁跨网络阻塞持有。

## 11. 合入 `main@ed815ffc` 的复盘

### 11.1 为什么使用 merge commit

现场赛提交 `a82b4ab` 已先推送为恢复点，再用 `--no-ff` 合并 `origin/main`。显式 merge commit `4146207` 的两个 parent 同时保留两条历史，便于回答某一行来自现场赛还是主线；不使用 rebase 或 squash，避免破坏已经验证和推送的提交身份。

### 11.2 五个文本冲突

| 文件 | `main` 侧必须保留 | 现场赛侧必须保留 | 最终选择 |
| --- | --- | --- | --- |
| `os/src/fs/inode.rs` | 新 VFS、锁、dentry/pending write 结构 | `ext4_fsize` 权威长度和避免锁嵌套 | 在 main 结构上重接实时长度与刷新 |
| `os/src/fs/stdio.rs` | `O_NONBLOCK`、定时睡眠和调度优化 | 规范队列、ICRNL、DEL/Ctrl-H、termios、`/dev/tty` | 统一为有数据即返、非阻塞 `EAGAIN`、阻塞睡眠 |
| `os/src/net/syscall.rs` | 安全用户缓冲、复制 API、可选 socket handle | getsockname、SO_ERROR、EOF、UDP 来源、PEEK、临时端口、清理 | 以 main 安全结构为骨架合并现场赛协议语义 |
| `os/src/syscall/fs.rs` | `DeviceLock`、`TaskGuard`、`VfsLock` 新协议 | `/dev/tty` 与完整 ioctl | 合并 import 和锁协议，保留 TTY 分发 |
| `os/src/task/task.rs` | 动态内核栈、诊断哨兵和任务字段变化 | `PosixTimer` 与进程 timer 表 | 两套状态并存，按各自生命周期初始化/清理 |

这些冲突不能直接 `ours` 或 `theirs`：选择 ours 会丢失主线并发和安全修复，选择 theirs 会让 Vim、Git 或网络重新失败。每个冲突都必须按不变量重组，而不是按文本块数量决定。

### 11.3 自动合并后的高风险语义检查

即使没有文本冲突，以下组合仍需人工审计和回归：timer 扫描与新调度锁；DHCP/socket close 与新网络生命周期；Makefile 的双架构入口和现场赛目标；README 文档索引；initproc 启动入口。合并分支保留 Rust shell，避免启动即自动运行 CAgent/BuildStorm 或关机，确保现场赛仍可人工进入 Alpine。

### 11.4 `main` 带入但不归为现场赛原创的内容

`main@ed815ffc` 带入动态内核栈和哨兵、SMP 调度/任务生命周期、futex/COW/共享内存/管道/信号改进、BuildStorm 文件系统与并发优化、网络安全复制框架、LoongArch 相关修复和双架构构建入口。这些能力提高了合并后系统的稳定性，但本文不会将其计入 `90803c5..a82b4ab` 的现场赛改动统计。

### 11.5 合并后新增修复

- `0d7dd76`：修复主线 pending write 结构与现场赛 Ext4 长度语义组合后暴露的问题，并提高验证器稳定性。
- `0b9ef74`：修复 `getent` 高位用户 sockaddr 触发的内核态缺页。
- `bd87f18`：把现场赛 QEMU 核数变成可选参数，便于用同一套功能验证多核组合。

## 12. 可重复验证基础设施

### 12.1 启动器保存什么

`scripts/run_onsite_2025_rv.sh` 在每轮运行前强制构建最新 RISC-V release 内核，创建新 qcow2 覆盖层，并把以下信息写入时间戳证据目录：

- 当前 Git commit 和工作树状态；
- Alpine raw 镜像、内核的 SHA-256；
- 完整 QEMU 命令和 `qemu_smp`；
- 未剪辑串口日志；
- 可选的网络 pcap 及摘要；
- 本轮 overlay，退出后供 `qemu-img check`。

基础镜像 SHA-256 已核对为：压缩包 `0def50e343fa287b86c89346b53a835405c300fe45744a664833a4fbdb378a1c`，解压 raw 镜像 `e3fe9eef8be9f35c23b4ca8b9d72af4f40744a275ab7f35782a4e235b3b6c231`。

### 12.2 四个自动入口

| 入口 | 核心范围 | 关键标记 |
| --- | --- | --- |
| `make verify-onsite-2025-regressions` | timer、TTY、ICRNL/erase、Ext4 rename，本地 Git | `TIMER_REGRESSION_OK`、`TTY_REGRESSION_OK`、`EXT4_RENAME_OK`、`GIT_LOCAL_OK` |
| `make verify-onsite-2025-vim` | 真实 Vim 交互、截短、GCC/Rustc | `VIM_TRUNCATE_STAT_OK` 和编译/运行 RC 0 |
| `make verify-onsite-2025-network` | DHCP/DNS、sockname、端口、EOF | `GETENT_DNS_OK`、`NETWORK_SOCKNAME_OK`、`NETWORK_TCP_EOF_OK` |
| `make verify-onsite-2025-git-clone` | 官方 xv6 完整 HTTPS clone | `GIT_CLONE_RC=0`、`GIT_CLONE_TREE_OK` |

自动验证不输入 GitHub PAT，所以 push/pull 仍由人工完成；PAT 只能在 Git 的密码提示中交互输入，不能写进 URL、脚本、Git config 或串口日志。

### 12.3 pcap 的诊断方法

`scripts/summarize_onsite_2025_pcap.py` 帮助按协议层查看证据。判断顺序固定为：

1. 没有 DHCP/ARP 请求：看 VirtIO TX descriptor 和 queue notify。
2. 有请求无响应入栈：看 RX buffer 回收、轮询和中断确认。
3. DHCP 有响应无地址/路由：看 DHCP event 消费和接口更新。
4. DNS 成功而 TCP 停滞：看 nonblocking connect、poll、`SO_ERROR` 和任务唤醒。
5. TCP 建立仍 clone 失败：再看 TLS、代理、EOF 和文件系统，不倒退为“DNS 不通”。

### 12.4 为什么测试不能降级

- `git -h` 不能替代 `init/add/commit/log`，更不能替代完整 clone/push/pull。
- `git ls-remote` 不能替代完整 clone，因为它不解包对象和写工作树。
- loopback TCP 不能替代 guest 经 VirtIO 到网关的连接。
- `vim -es` 绕过交互 TTY，不能作为 Vim 编辑分。
- 运行旧的 `/tmp/hello-rs` 不能证明刚才失败的 rustc 已恢复；必须先删除产物、重新编译再运行。
- 针对进程名返回成功会掩盖缺失 ABI，也会在工具版本或执行路径改变后失效，禁止使用。

## 13. 计分点、内核修改和证据映射

| 计分点 | 主要修改 | 自动证据 | 人工证据 |
| --- | --- | --- | --- |
| Git 加载 | 基础 ABI/TTY | 帮助命令 RC 0 | 完整帮助输出 |
| Git 本地 | Ext4 overwrite rename、hard link、pending write | `EXT4_RENAME_OK`、`GIT_LOCAL_OK` | `git log`、干净 status |
| clone | VirtIO RX、DHCP、DNS、socket | `GIT_CLONE_RC=0`、`GIT_CLONE_TREE_OK` | xv6 `HEAD=f5dea58...` 和完整工作树 |
| push | TTY Enter/erase、连续端口、HTTPS、Ext4 | socket/TTY/Git 本地回归 | GitHub 网页显示客体提交 hash |
| pull | 同上，加 fetch/rename | clone 与 Ext4 回归 | 网页唯一标记出现在本地 README |
| Vim 加载 | timer/TTY 基础 | help RC 0 | 帮助输出 |
| Vim 编辑 | POSIX timer、termios、winsize、truncate/stat | `VIM_TRUNCATE_STAT_OK` | 交互录入及保存后的精确内容 |
| GCC | Ext4 长度、进程/文件 ABI | 新 C 产物 build/run RC 0 | `Hello, World!` |
| Rustc | 同上 | 新 Rust 产物 build/run RC 0 | `Hello, World!` |

人工 GitHub 证据中，客体曾推送提交 `77e9917d143ce64d015661a243c0cb23cc3c2f84`，随后网页提交并 pull 到 `592ebfc89a1f771cfa1d4fa161d3d767e5fc4f67`，唯一标记 `ONSITE_FINAL_PULL_20260814_PORTFIX_OK` 在本地出现。凭据本身没有写入仓库。

## 14. 多核现场赛验证

现场赛启动默认单核，以减少定位变量；需要检查与主线 SMP 的组合时显式设置 `QEMU_SMP=4`。完整四核现场赛流程已验证 DHCP、timer/TTY/Ext4、Vim、GCC、Rustc、DNS、连续 socket 和完整 clone，并看到 `online mask 0xf`。

这里必须区分三个数字：

- 参数解析范围：脚本接受 1–12；
- RISC-V 平台编译上限：`PLATFORM_CPUS=8`，而全局 `MAX_CPUS=12`；
- 已完成的完整现场赛动态证据：4 核。

因此文档不能写“RISC-V 12 核通过”。给 QEMU 传 12 可能让 OpenSBI 枚举 12 个 hart，但内核最多让 RISC-V 平台配置允许的 8 个上线，`online mask` 只会到 `0xff`。若未来要声称 8 核或更高通过，必须用相同计分顺序重新跑全套，并保留相应 metadata、serial log 和 overlay check。

主线定向 SMP 验证结果为：release 3/3，证据目录 `artifacts/smp4-validation-onsite-merge-final-release-v2/`；diagnostics 1/1，证据目录以最终 diagnostics v5 结果为准。通过条件包括 `online mask 0xf`、`observed=0xf`、全部定向标记存在，且无 panic、锁序反转、调度停滞或段错误。`artifacts/smp4-manual-check/` 的 3/3 仅是额外人工检查，不应把其中零计数指标扩张解释为所有子项都有独立命中。

## 15. 复现和复测入口

详细命令、按键和故障排查见配套验收指南。维护者至少应按以下顺序执行：

```bash
# 1. 静态检查和 RISC-V 构建
git diff --check
cargo check -p os --target riscv64gc-unknown-none-elf
make kernel-rv

# 2. 单核现场赛基础回归
make verify-onsite-2025-regressions
make verify-onsite-2025-vim
make verify-onsite-2025-network

# 3. 启动宿主代理中继后做完整 clone
make onsite-2025-proxy-relay
make verify-onsite-2025-git-clone

# 4. 四核重跑同一组合
make verify-onsite-2025-regressions QEMU_SMP=4
make verify-onsite-2025-vim QEMU_SMP=4
make verify-onsite-2025-network QEMU_SMP=4
make verify-onsite-2025-git-clone QEMU_SMP=4

# 5. 主线 SMP 定向回归
scripts/validate_smp4.py \
  --kernel kernel-rv --root-disk disk.img --test-disk test_disk.img \
  --suite directed --rounds 3 --build \
  --output-dir artifacts/smp4-validation-onsite-merge-release
```

每个现场赛覆盖层退出后执行：

```bash
qemu-img check artifacts/onsite2025/<时间戳>/alpine-overlay.qcow2
```

`No errors were found on the image` 只证明 qcow2 结构一致，不单独证明应用功能；它必须与串口标记和应用副作用一起保存。

## 16. 合并前 29 个改动文件职责

以下清单严格来自 `git diff --name-status 90803c5..a82b4ab`。

| 文件 | 职责和现场赛改动摘要 |
| --- | --- |
| `.gitignore` | 排除现场赛运行产生的 overlay、pcap、日志等大体积/临时证据 |
| `Makefile` | 增加构建、手工启动、代理及四类验证入口 |
| `README.md` | 增加现场赛文档和入口索引 |
| `documents/现场赛2025_RISCV验收指南.md` | 操作、验收和证据说明 |
| `os/src/drivers/net/virtio_net.rs` | 正确剥离 VirtIO net header，把 Ethernet packet 交给协议栈 |
| `os/src/fs/inode.rs` | Ext4 rename/link/truncate/stat、实时长度和刷新处理 |
| `os/src/fs/mod.rs` | `/dev/tty` 等特殊路径/文件系统模块接线 |
| `os/src/fs/stdio.rs` | 规范输入、termios 状态、Enter/erase、阻塞读取和控制台输出 |
| `os/src/net/mod.rs` | DHCP event、接口地址、默认路由、网络轮询接线 |
| `os/src/net/socket.rs` | socket 状态、endpoint、EOF/close 和唤醒相关语义 |
| `os/src/net/syscall.rs` | connect、sockname、SO_ERROR、UDP 来源、PEEK 等 syscall ABI |
| `os/src/syscall/fs.rs` | `/dev/tty` 打开、TTY ioctl、用户结构复制和 fs syscall 接线 |
| `os/src/syscall/mod.rs` | 新 syscall 号与分发 |
| `os/src/syscall/process.rs` | 五个 POSIX timer syscall |
| `os/src/task/mod.rs` | timer 扫描/任务模块导出和生命周期接线 |
| `os/src/task/signal.rs` | timer 到期信号信息和投递支持 |
| `os/src/task/task.rs` | `PosixTimer`、进程 timer 表及初始化/清理 |
| `scripts/onsite_2025_proxy_relay.py` | 宿主受限 TCP 代理中继，不记录 payload |
| `scripts/run_onsite_2025_rv.sh` | 最新内核、干净 overlay、双盘网卡、哈希和日志启动器 |
| `scripts/summarize_onsite_2025_pcap.py` | 对 DHCP/ARP/DNS/TCP 抓包作分层摘要 |
| `scripts/verify_onsite_2025_git_clone.py` | 自动执行完整 xv6 HTTPS clone 并校验工作树 |
| `scripts/verify_onsite_2025_regressions.py` | 驱动 timer/TTY/Ext4/Git 本地回归 |
| `scripts/verify_onsite_2025_vim.py` | 通过 PTY 操作真实 Vim并校验截短、C/Rust 编译 |
| `tests/onsite2025/README.md` | C 回归程序构建和标记说明 |
| `tests/onsite2025/network_sockname_regression.c` | 本地地址、连续临时端口和 socket 错误回归 |
| `tests/onsite2025/network_tcp_eof_regression.c` | 对端关闭后 TCP EOF 返回 0 |
| `tests/onsite2025/timer_regression.c` | POSIX timer 单次、周期、绝对时间、错误和信号 |
| `tests/onsite2025/tty_icrnl_regression.c` | ICRNL、DEL/Ctrl-H 与规范行缓冲 |
| `tests/onsite2025/tty_regression.c` | termios、winsize、FIONREAD、ENOTTY |

## 17. syscall、ioctl 与 errno 速查

### 17.1 POSIX timer

| 接口 | 关键成功语义 | 主要错误 |
| --- | --- | --- |
| `timer_create` | 进程内分配 ID，记录通知方式 | `EINVAL/EFAULT/EAGAIN/ESRCH` |
| `timer_settime` | 相对或 `TIMER_ABSTIME`，可返回旧值 | `EINVAL/EFAULT` |
| `timer_gettime` | 返回剩余时间和周期 | `EINVAL/EFAULT` |
| `timer_getoverrun` | 返回上次关联的 overrun | `EINVAL` |
| `timer_delete` | 取消并释放 ID | `EINVAL` |

### 17.2 TTY

| 条件 | 返回/行为 |
| --- | --- |
| TTY 有数据 | 立即返回可用字节 |
| TTY 无数据且 `O_NONBLOCK` | `EAGAIN` (`-11`) |
| TTY 无数据且阻塞 | 任务睡眠，输入到来后唤醒 |
| 普通文件/管道执行 TTY ioctl | `ENOTTY` (`-25`) |
| ioctl 用户指针无效 | `EFAULT` (`-14`) |
| 前台进程组参数无效 | `EINVAL` (`-22`) |

### 17.3 socket

| 场景 | 兼容语义 |
| --- | --- |
| 非阻塞 connect 未完成 | 返回进行中错误，之后 poll + SO_ERROR 判定 |
| 未绑定 socket 自动选端口 | `getsockname` 返回真实 endpoint |
| UDP 接收 | 返回真实远端 source address |
| `MSG_PEEK` | 返回数据但不推进接收队列 |
| TCP 对端正常关闭 | `recv` 返回 0 |
| 无效用户 sockaddr | `EFAULT`，不得内核 panic |

## 18. 关键通过标记和已保存证据

| 范围 | 预期标记/结果 |
| --- | --- |
| timer | `TIMER_REGRESSION_OK hits=4` |
| TTY 通用 | `TTY_REGRESSION_OK` |
| Enter/erase | `TTY_ICRNL_OK`、`TTY_ERASE_OK` |
| Ext4 rename | `EXT4_RENAME_OK` |
| Git 本地 | `GIT_LOCAL_OK`，status 为空 |
| Vim | `VIM_TRUNCATE_STAT_OK`，保存内容与长度一致 |
| GCC/Rustc | 本轮新构建产物输出 Hello World，build/run RC 均为 0 |
| DHCP | 地址 `10.0.2.15/24`、网关 `10.0.2.2`、DNS `10.0.2.3` |
| socket | `NETWORK_SOCKNAME_OK address=10.0.2.15 ports=49152,49153` |
| TCP EOF | `NETWORK_TCP_EOF_OK` |
| DNS | `GETENT_DNS_OK` |
| clone | `GIT_CLONE_RC=0`、`GIT_CLONE_TREE_OK` |
| 官方 xv6 | 验证时 `HEAD=f5dea58cc1057f2b076cdb90b446c2c21d91171e` |
| SMP | `online mask 0xf`、`observed=0xf`，release 3/3、diagnostics 1/1 |
| 磁盘 | `qemu-img check` 报 `No errors were found on the image` |

日志中的单个标记只覆盖其命名语义。例如 `GETENT_DNS_OK` 不证明 Git 写盘，`qemu-img check` 不证明 HTTPS，`online mask 0xf` 不证明四核下每个现场赛功能都跑过。最终结论来自同一次或可追溯提交上的组合证据。

## 19. 已知限制和不得夸大的结论

1. 当前脚本接受 1–12 核，但 RISC-V `PLATFORM_CPUS=8`；只完成四核全功能动态验证，不能声称 12 核通过。
2. LoongArch 缺少本机完整动态环境。即使某次 `cargo check` 通过，也只能写“编译检查通过”，不能写“现场赛运行通过”。
3. BuildStorm/CAgent 使用不同镜像、负载和评分协议，不属于本文现场赛证明。
4. GitHub push/pull 依赖个人仓库和 PAT。PAT 权限应最小化为目标仓库 `Contents: Read and write`，仅在密码提示中输入；任何串口日志公开前都要再次检查凭据。
5. QEMU SLIRP、宿主代理和真实互联网是外部变量。网络失败要先按 pcap 分层定位，不能在没有证据时都归咎于内核。
6. `TERM` 和宿主终端会影响 Vim 显示；判断内核正确性仍需结合 TTY 回归、保存后的文件和退出码。

## 20. 后续维护者的强制回归清单

修改以下任一模块后，不应只跑该模块单测：

| 修改区域 | 至少必须重跑 |
| --- | --- |
| timer、信号、调度、任务锁 | timer 全回归、真实 Vim、单核和四核；检查 exec/exit 后无残留信号 |
| TTY、stdio、UART、poll | TTY 两组 C 回归、Git 凭据输入、Vim、非阻塞读取、四核输出 |
| inode、dentry、Ext4、页缓存、锁 | overwrite rename、Git 本地、Vim 截短/stat、GCC/Rustc、完整 clone |
| VirtIO net、DHCP、smoltcp | DHCP/ARP/DNS pcap、network 两组 C 回归、完整 clone |
| socket syscall、用户复制 | sockname/端口、TCP EOF、getent 高地址、TLS clone、无效指针 errno |
| 调度/SMP/内核栈 | 主线 release 3 轮、diagnostics 1 轮，再跑四核现场赛组合 |
| Makefile/验证脚本 | 检查最新内核确实重建、metadata 哈希正确、覆盖层可检查、失败不会误报成功 |

最终复测应从全新 overlay 开始，按计分顺序完整重跑，并保存 Git commit、镜像/内核哈希、QEMU 命令、未剪辑串口日志、pcap（需要时）、`qemu-img check` 和人工网页证据。只有这些证据共同指向同一源码状态，才能将“曾经成功”提升为“当前提交可复现地通过”。

## 21. 个人复盘：这次适配真正解决了什么

本次工作最重要的结果不是“能显示四个工具的版本”，而是把一组此前模糊的 Linux 兼容问题变成了可复现的不变量：Git 的 lock 必须能原子覆盖发布；Vim 的超时、termios 和截短文件必须遵守标准语义；网络要从 VirtIO descriptor 一直闭环到 DHCP、DNS、TCP、TLS 和完整对象写盘；用户地址永远不能被内核随意解引用；多核合并不能以牺牲单核现场赛功能为代价。

排查方法也由“看到应用报错就补一个 syscall”转变为分层验证：先最小复现，再确定失败属于宿主、驱动、协议栈、syscall、VFS 还是应用；随后实现通用语义，最后用小回归和真实负载双重证明。正因为没有用应用名特判、伪造返回值、版本输出或非交互模式代替真实行为，这些修改才具有超出单一赛题的维护价值。
