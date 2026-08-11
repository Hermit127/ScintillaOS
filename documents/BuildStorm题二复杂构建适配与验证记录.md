# BuildStorm 题二复杂构建适配与验证记录

## 1. 目标与约束

本阶段面向决赛题 2.2 的真实复杂软件构建负载。验证环境保持官方要求的
`-smp 8 -m 8G`，同时挂载官方 BuildStorm 数据盘和 ScintillaOS 根文件系统盘。
RISC-V 使用 MMIO VirtIO 块设备和网络设备，LoongArch 使用 PCI VirtIO 块设备和
网络设备。测试过程中不移除虚拟网卡、不降低核数、不改成 `-j1`，也不修改官方
Cargo 工作负载。

本轮修改遵循增量原则：优先补齐 Linux 可观察语义和降低通用 VFS 开销，不通过
删设备、改测试脚本、伪造时间或面向单个测试结果返回固定值来绕过问题。
`user/src/bin/initproc.rs` 没有修改。

本记录的最终验收对象是题 2.2，而不是题 2.1。`rustc --version`、
`cargo --version` 和最小 Hello World 构建只作为定位动态链接、线程同步、进程回收与
文件系统基础语义的前置检查。只有不计时的 `cargo build -p tg-xtask` 完成后，正式
执行 `cargo xtask arceos build -p arceos-helloworld` 返回成功，并生成不小于
500 KiB 的目标文件，才能认定题 2.2 已通过；当前尚未达到这一结论。

## 2. 官方工作负载复核

本地保存的官方脚本
`/private/tmp/buildstorm_testcode_final2026.sh` 的关键流程如下：

1. 加载 `nightly-2026-05-28` 的 `rustc` 和 `cargo`。
2. 使用 `cargo new`、`cargo build` 和实际运行生成的 Hello World 验证 2.1。
3. 进入 `/work/tgoskits`，执行不计时的 `cargo build -p tg-xtask`。
4. 从 `/proc/uptime` 读取起始时间。
5. 执行 `cargo xtask arceos build -p arceos-helloworld --arch "$AXARCH"`。
6. 再次读取 `/proc/uptime`，检查返回码以及不小于 500 KiB 的构建产物。

因此 `/proc/uptime` 不是可选兼容项。此前内核已有少量 proc 伪文件，但没有
`/proc/uptime`，官方脚本即使完成编译也无法得到有效的 guest 内耗时。

## 3. 本轮增量实现

### 3.1 单调运行时间与实时时钟分离

`os/src/time.rs` 保留两类时钟：

- `realtime_us()` 使用平台 RTC 校准，服务于 `CLOCK_REALTIME`、`date` 和文件
  时间戳。
- `uptime_us()` 直接返回 PolyHAL 单调计时器自虚拟机启动以来的时间，不叠加 RTC
  epoch 偏移。

这样可以同时满足 Debian 用户态对真实日期的要求和 BuildStorm 对单调编译耗时的
要求。实现没有改写计时倍率，也没有根据测试进程或测试路径调整时间。

### 3.2 实现 `/proc/uptime`

`os/src/fs/proc.rs` 新增 `ProcUptimeFile`。每次打开文件时从
`crate::time::uptime_us()` 截取一致快照，按 Linux 文本格式输出：

```text
<启动后秒数> <累计空闲秒数>
```

首字段保留百分之一秒精度。当前内核尚未维护每 CPU idle accounting，因此第二字段
明确输出 `0.00`，而不是伪造空闲时间。文件是 `0444` 的普通只读文件，支持分段
读取、独立文件偏移和 `fstat`。

`os/src/syscall/fs.rs` 将 `/proc/uptime` 接入 proc 路径分派，并补充路径 stat
语义。该路径在 chroot 后仍通过绝对 proc 路径访问，符合官方脚本的调用方式。

### 3.3 减少 inode 元数据查询的重复路径遍历

`VfsSuperBlock::get_inode_info()` 原来为一次 stat 类查询分别调用 mode、raw inode、
owner、atime、mtime、ctime，并再次 open/close 文件或目录取得大小和 inode 号。
复杂构建会产生大量 `stat/openat`，这些重复调用都需要进入 lwext4 并重新遍历路径。

现在一次调用 `ext4_raw_inode_fill` 获取一致的磁盘 inode 快照，再从中解码：

- inode 号、类型和权限；
- 普通文件的 64 位大小；
- uid、gid 和链接数；
- atime、mtime、ctime；
- 实际分配块数。

已有的 VFS owner 缓存仍具有优先级，避免破坏 `chown` 等现有语义。目录和符号链接
不再为了 stat 额外执行 open/close。该修改是通用 VFS 优化，不依赖 Cargo 路径或
crate 名称。

### 3.4 保留双架构 VirtIO 网络设备

所有 q31、q32、q33 验证都保留虚拟网卡：

```text
RISC-V:
-netdev user,id=net0
-device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2

LoongArch:
-netdev user,id=net0
-device virtio-net-pci,netdev=net0
```

两架构启动日志都确认 MAC 为 `52:54:00:12:34:56`，并初始化
`127.0.0.0/8` 和 `10.0.2.15/24`。虚拟网卡与两块磁盘、8 个 hart 同时存在，
没有通过删除设备规避 MMIO 或 PCI 资源冲突。

## 4. 静态验证

以下命令均成功：

```sh
cargo check -p os --target riscv64gc-unknown-none-elf
cargo check -p os --target loongarch64-unknown-none
make kernel-rv
make kernel-la
git diff --check
```

构建仍有仓库原有的 unused import、vendor cfg 和 future incompatibility warning，
没有由本轮 uptime 或 inode 修改引入的编译错误。

## 5. q33 双架构运行验证

q33 使用两个独立的稀疏 qcow2 覆盖层，官方 14 GiB 原始镜像只作为只读 backing
file。两个 QEMU 同时以 `-smp 8 -m 8G` 启动，并保留上述虚拟网卡。

### 5.1 启动与设备

RISC-V 和 LoongArch 均输出：

```text
Boot HART Count           : 8
[smp] online mask 0xff
[net] VirtIONetAdapter initialized successfully
[net] Static IPs: loopback 127.0.0.0/8 + 10.0.2.15/24
```

两架构均识别两块 VirtIO 块设备，成功挂载官方盘并 chroot 到 Debian 用户态。

### 5.2 `/proc/uptime` 运行结果

RISC-V：

```text
48.63 0.00
mode=8124 size=11 type=regular file
T0=50.06 T1=53.26
UPTIME_DELTA=3.20
```

LoongArch：

```text
71.11 0.00
mode=8124 size=11 type=regular file
T0=73.02 T1=78.40
UPTIME_DELTA=5.38
```

测试在两次读取之间执行 `sleep 2`。差值大于 2 秒是双重架构模拟下命令启动和调度
开销造成的真实墙钟推进；关键结论是两边首字段均单调递增，且不是固定值或 realtime
epoch。

### 5.3 官方 2.1 回归

同一 q33 配置中，两架构均成功输出：

```text
rustc 1.98.0-nightly (57d06900f 2026-05-27)
cargo 1.98.0-nightly (fbb61be30 2026-05-26)
BUILDSTORM_MINIBUILD ok
```

`BUILDSTORM_MINIBUILD ok` 只有在 `cargo new`、真实编译和运行生成的二进制都成功
后才会出现，因此本轮 inode 与 uptime 修改没有破坏已经通过的 2.1。

### 5.4 2.2 预构建边界

两架构随后按官方参数执行：

```sh
cargo build -p tg-xtask
```

Cargo 成功完成依赖图解析并进入约 446 个构建单元的编译，串口显示
`proc-macro2`、`quote`、`libc`、`cfg-if`、`log`、`shlex` 等真实 crate。
运行期间：

- RV 约持续使用 4 至 6 个宿主 CPU，块写入增长到约 171 MiB；
- LA 约持续使用 2 至 4 个宿主 CPU，块写入增长到约 84 MiB；
- 两边均没有用户态 heap corruption、内核 panic、lwext4 断言或网卡异常；
- qcow2 覆盖层持续增长，说明构建仍在产生文件。

本机是 arm64 macOS，Docker 镜像为 amd64，容器内又由 QEMU 模拟 RISC-V/LoongArch，
属于双重指令翻译。预构建在连续十分钟没有新的串口进度行后按本地测试约定停止。
因此当前只能确认复杂构建已进入真实并行编译，不能声称 2.2 完成、产物达到 500 KiB
或获得 40/120 分。

## 6. 当前结论

已确认：

- 虚拟网卡无需删除，能与 8 核、8 GiB 和双盘同时初始化并承受构建负载；
- RV/LA 的官方 2.1 在新内核上均通过；
- 官方 2.2 所需的 `/proc/uptime` 已按单调时钟语义实现并通过双架构运行验证；
- inode 元数据查询改为一次路径解析，双架构高负载期间没有出现语义回归或 panic；
- `tg-xtask` 预构建能够进入真实多 crate 并行编译。

尚未确认：

- `cargo build -p tg-xtask` 在本地完成；
- `cargo xtask arceos build -p arceos-helloworld` 返回 0；
- 最终产物大小和官方时间分。

下一步应在原生 amd64 Linux 或资源更充足的评测机上继续同一官方命令，记录第一个真实
syscall、VFS 或内存语义错误。当前不应仅根据 Mac 双重模拟的耗时重构调度器，也不应
为了缩短本地时间删除网卡、降低核数或改变 Cargo 并行度。

## 7. 2026-08-07：2.2 持续调试进展

### 7.1 目标边界说明

当前主目标一直是题 2.2，即在官方 `-smp 8 -m 8G` 环境中完整执行
`cargo xtask arceos build -p arceos-helloworld`，并生成大小不小于 500 KiB
的产物。调试期间反复运行最小 `cargo new`/`cargo build` 不是将目标退回
题 2.1，而是用更小、更快、可重复的负载分离题 2.2 中的通用内核故障。
如果最小 Rust 构建都可在某个并发时序下停滞，数百 crate 的题 2.2 构建
必然会更容易触发同一问题。

### 7.2 ext4 rename 链接计数修复

在一份已损坏的 BuildStorm 写时覆盖层中，曾观察到两个路径同时指向
inode `90279`，但两个路径读出的 `st_nlink` 都是 1。这说明原有普通文件
rename 流程先创建新目录项，却没有按硬链接语义增加 inode 链接计数。如果
后续删除源目录项失败或中途中断，inode 可被提前释放并重用，而新目录项
仍然引用它，最终表现为 Cargo 临时文件、增量指纹或 rlib 被随机破坏。

`lwext4_rust/c/lwext4/src/ext4.c` 中的 `ext4_frename` 和 `ext4_frename2`
现在对普通文件使用 `ext4_create_hardlink`，然后通过普通
`ext4_unlink` 移除源目录项。这使中间状态的链接计数和 Linux rename 所需
的 inode 生命周期一致。目录 rename 仍保留原有专用路径，避免将普通文件
硬链接规则错误套用到目录。

### 7.3 `ftruncate` 后文件大小语义修复

`os/src/fs/inode.rs` 原先在生成 stat 信息时，使用当前文件偏移作为
`st_size` 的下界。这与 Linux 语义冲突：`ftruncate(fd, smaller_size)` 改变
文件长度，但不移动该打开文件描述的偏移。例如先写入 1 MiB，再截断到
0，偏移仍可保持在 1 MiB，但 `st_size` 必须为 0。

现在 stat 直接使用 ext4 inode 元数据中的真实文件长度，不再与打开文件偏移
取最大值。该修复对 Cargo 的临时文件重用、链接器覆盖输出和通用 POSIX
程序都有效，不包含任何针对测例路径的特殊分支。

### 7.4 双架构 8 核文件系统压力验证

为避免每次都等待完整 BuildStorm，本地新增了不进入提交的临时压力脚本。
该负载使用唯一文件名完成 960 次 rename，使用已存在目标完成 320 次
覆盖 rename，并保留打开 fd 检查 rename 前后的 inode 和内容语义；随后执行
16 轮约 1 MiB 写入、`O_TRUNC` 与 `ftruncate`，最后检查孤儿目录项和
链接计数。

RISC-V64 和 LoongArch64 都在 `-smp 8` 下通过，关键输出为：

```text
FS_QUICK_INODE_PASS files=960
FS_QUICK_TRUNCATE_PASS rounds=16
BUILDSTORM_FS_QUICK_PASS
```

这不能替代官方 2.2 全链路，但可以说明已发现的 rename 链接计数错误和
truncate/stat 不一致已能被专门回归负载覆盖，且暂未在双架构多核下复现。

### 7.5 当前真实 BuildStorm 停滞位置

修复文件系统语义后，LoongArch 官方 glibc 盘上的工具链版本查询和
`cargo new` 正常，`cargo build -vv` 能够启动真实 `rustc`。一次开启完整
SMP 诊断的运行还进一步观察到：

1. `rustc` 启动多个工作线程。
2. `rustc` 启动 `/usr/bin/cc`。
3. `cc` 启动 `collect2` 和 `/usr/bin/ld`。
4. `ld` 成功退出并被父进程回收。

但普通低日志内核仍可在最小 Rust 构建中停滞。从不依赖 `fork`
的 `/proc/<pid>/stat` 快照可见，Cargo 主线程、Cargo 工作线程、rustc 主线程和
rustc 工作线程均进入睡眠，且 CPU 时间不再增长。高密度诊断打印会改变
时序，使链接路径继续执行，这是典型的并发竞态特征，不是单纯的
Rust 编译器性能不足。

因此当前还不能宣称题 2.2 已通过。目前的进度是：已进入真实编译和
链接阶段，已修复两个可重现的文件系统语义错误，当前主阻塞已收敛
到 SMP 同步和线程生命周期。

### 7.6 待排查的通用语义缺口

当前按证据优先级排列的原因如下：

1. **futex requeue 与超时清理**：等待者被 `FUTEX_REQUEUE` 移到新 key
   后，当前超时恢复路径只从原 key 删除自身，可能在新队列留下过期
   `TaskControlBlock`，并在之后产生错误唤醒。
2. **`CLONE_FILES` 与管道 EOF**：当前内核通过多份 fd 表镜像 Linux 共享
   `files_struct`。引用计数大体配对，但仍需验证 clone/exec/exit 并发时
   是否会残留管道写端，导致 Cargo 或 rustc 永远等待 EOF。
3. **线程退出与 `clear_child_tid`**：快照中出现过已消失的创建者线程
   仍作为存活线程父节点的现象。需要确认线程组退出、robust-list 清理、
   `clear_child_tid` 置零及 futex wake 的顺序与 Linux 一致。
4. **procfs 完整性**：`/proc/self/stat` 可以读取，但 `/proc/self` 和
   `/proc/<pid>` 目录尚不完整，导致 Debian `ps` 不能用于常规诊断。这是
   应补齐的 Linux 兼容性，但现有证据尚不支持它是此次停滞的直接原因。
5. **POSIX timer**：GNU `timeout` 会调用系统调用 107（`timer_create`），目前
   内核返回 `ENOSYS`。本地诊断脚本已不再用 `timeout` 包裹 Cargo，避免将
   诊断工具本身的兼容问题误判为官方构建问题。

### 7.7 后续实施顺序

下一步先修复 futex 等待者被 requeue 后的超时清理，并使用低开销事件环
记录等待、唤醒、requeue 和 `clear_child_tid`，避免逐事件串口打印改变
竞态时序。然后构造包含 pthread condvar、定时 futex wait、requeue 和线程
退出的通用压力负载。修复通过后，按以下顺序验证：

1. LoongArch64 最小 Rust 构建多轮重复运行。
2. RISC-V64 最小 Rust 构建多轮重复运行。
3. 双架构 `cargo build -p tg-xtask`。
4. 双架构完整 `cargo xtask arceos build` 和产物大小检查。
5. 全部通过后再记录 `/proc/uptime` 的真实耗时和加速比。

截至本节记录时，上述修复和诊断均仅存在于本地 `main` 工作区，没有
commit，也没有 push。

## 8. 2026-08-07：最小 Rust 构建永久等待的根因与修复

### 8.1 本阶段不是将目标降为题 2.1

本阶段最终验收目标仍是题 2.2：在官方 `-smp 8 -m 8G` 配置下完整执行
`cargo xtask arceos build -p arceos-helloworld`，并生成不小于 500 KiB 的
产物。之所以先反复运行 `cargo new`、`cargo build` 和生成程序，是因为该最小负载
已经能够稳定复现题 2.2 中的线程同步故障。它将数百 crate、复杂文件系统访问和长时间
编译缩减为一次 rustc、cc、collect2、ld 与 pthread join 链路，便于区分性能问题与
内核语义错误。最小构建通过只是进入完整 2.2 验证的前置条件，不代表题 2.2 已完成。

### 8.2 诊断手段

在 `smp-diagnostics` 构建中加入容量固定的 futex 事件环，记录 wait、wake、requeue、
线程 `clear_child_tid` 清零和实际唤醒返回值。事件仅写入预分配的原子数组，在检测到
rustc 线程组收缩到少量阻塞线程时一次性输出，避免逐系统调用串口打印大幅改变 SMP
时序。`/proc/<pid>/status` 的诊断字段同时记录每个线程当前的阻塞原因，`Threads`
字段则改为根据 PID registry 中同一 TGID 的存活任务动态统计。

诊断还修正了两个相关的通用线程语义：futex waiter 经 `FUTEX_REQUEUE` 移动后，超时
或中断清理会从所有 key 队列中按 TID 删除，而不是只检查原 key；`CLONE_THREAD`
创建的线程继承调用线程的真实父进程，符合 Linux 的隐含 `CLONE_PARENT` 关系，避免
创建线程退出后留下失效的 parent 指针。这两项提高了线程生命周期的一致性，但没有
单独消除本次永久等待。

### 8.3 可重复的根因证据

修复前，RV 与 LA 都能完成动态加载、`rustc --version`、`cargo new`，并进入真实链接。
`ld` 和承担链接协调的 rustc 工作线程退出后，rustc 主线程永久睡眠在该工作线程的
`clear_child_tid` futex 上。清零内存值成功，但 wake 使用的 key 与等待时不同。

LoongArch 的一轮证据为：

```text
waiter: namespace=0x0 addr=0x0a332630 tids=[74]
clear : virtual=0x100f7fc630 shared-key=0x0a358630 value=0 wake=0
```

RISC-V 的一轮证据为：

```text
waiter: namespace=0x0 addr=0x95e10070 tids=[74]
clear : virtual=0x10147fd070 shared-key=0x95232070 value=0 wake=0
```

两架构中虚拟 `clear_child_tid` 地址保持不变，内存也确实写成 0，但物理页号在等待期间
发生变化。旧实现把所有未设置 `FUTEX_PRIVATE_FLAG` 的 futex 都按当前物理地址建 key，
因此等待者留在旧物理 key，退出线程只会唤醒新物理 key。这正是诊断日志会改变结果、
普通 release 容易卡死的竞态窗口。

### 8.4 按 Linux VMA 语义修复 futex key

`os/src/mm/memory_set.rs` 新增只读 VMA 属性查询，判断给定地址是否属于真正的
`MAP_SHARED` 映射。`os/src/syscall/process.rs` 的 futex key 选择据此调整：

- 私有 VMA 无论调用者是否显式设置 `FUTEX_PRIVATE_FLAG`，都使用稳定的
  `address_space_id + virtual_address`；
- 只有真正的 `MAP_SHARED` VMA 才使用可跨地址空间匹配的共享 key；
- `clear_child_tid` 和 robust-list 退出清理仍同时走统一 key 生成与唤醒路径，不添加
  rustc、Cargo、路径或测例名称判断。

这与 Linux 的核心原则一致：`FUTEX_PRIVATE_FLAG` 是允许内核采用更快私有查找的提示，
不是把私有匿名 VMA 强行解释为共享物理页的条件。私有页可能因 COW、缺页处理或映射
维护而更换物理帧，futex 身份不能随物理帧变化。修复也覆盖 pthread join、条件变量和
robust mutex 的同类丢唤醒风险。

### 8.5 双架构验证结果

修复后先在带完整 SMP 诊断的内核上验证，随后重新构建不含诊断 feature 的正常
release 内核，所有运行都使用官方 Docker、官方 glibc 数据盘、`-smp 8 -m 8G`，且
RV 与 LA 同时运行。

诊断内核结果：

```text
LoongArch: BUILDSTORM_MINIBUILD_CARGO_END rc=0 uptime=183.26 0.00
LoongArch: BUILDSTORM_MINIBUILD ok
RISC-V:    BUILDSTORM_MINIBUILD_CARGO_END rc=0 uptime=169.41 0.00
RISC-V:    BUILDSTORM_MINIBUILD ok
```

正常 release 内核结果：

```text
LoongArch: Finished dev profile in 1m 19s
LoongArch: BUILDSTORM_MINIBUILD_CARGO_END rc=0 uptime=128.69 0.00
LoongArch: BUILDSTORM_MINIBUILD ok
RISC-V:    Finished dev profile in 1m 06s
RISC-V:    BUILDSTORM_MINIBUILD_CARGO_END rc=0 uptime=114.38 0.00
RISC-V:    BUILDSTORM_MINIBUILD ok
```

成功标记之前实际执行了新建项目、完整 rustc/cc/ld 构建和生成程序输出校验，不是通过
超时、伪造返回值或跳过命令获得。诊断与 release 都通过，说明修复不是高密度日志带来
的时序假阳性。

### 8.6 当前边界与下一步

截至本节写入时，题 2.2 的最小同步阻塞已经修复，双架构正常 release 可完成最小 Rust
构建。官方完整 `cargo build -p tg-xtask`、`cargo xtask arceos build`、最终产物大小与
真实耗时仍需继续验证，因此尚不能宣称题 2.2 得分。下一步直接运行本地保存的官方脚本
等价路径 `BUILDSTORM_FULL=1`；若出现新错误，将继续记录第一个真实内核语义缺口并按
Linux 通用行为修复。

本阶段修改仍只存在于本地 `main` 工作区，没有 commit，也没有 push；
`user/src/bin/initproc.rs` 保持 shell 入口未改。

## 9. 2026-08-07：完整预构建验证与空闲热路径优化

### 9.1 当前验证对象

在最小 Rust 构建稳定通过后，本地同时启动 RISC-V64 与 LoongArch64 的
`cargo build -p tg-xtask`。两端均保持官方 `-smp 8 -m 8G`、双磁盘和 VirtIO
网卡配置。该命令包含 446 个构建单元，是进入正式
`cargo xtask arceos build` 前的官方不计时自举步骤。当前运行已经离开最小构建，进入
真实多 crate 编译；它仍需越过旧版本约 `40/446` 的 Cargo 指纹 `ENOENT` 和
`47/446` 的内核堆 OOM 故障点，因此本节不把“仍在推进”写成题 2.2 已通过。

本地 Mac 只有 8 个宿主逻辑 CPU，同时运行两个各含 8 个 vCPU 的跨架构 TCG 虚拟机，
会形成明显超额订阅；诊断内核还会每 60 秒读取一次任务阻塞状态。因此该轮主要判断
进程是否持续更替、编译单元是否增长以及是否出现确定的内核错误，不把其耗时作为官网
加速分依据。

### 9.2 网络轮询时基修复

LoongArch 的硬件定时中断周期为 1 ms，但用户调度时间片为 10 ms。旧路径在每次
1 ms 中断都执行一次完整 VirtIO/smoltcp 轮询，同时把 smoltcp 内部时钟固定增加
10 ms。这既使网络时钟相对真实单调时间快进十倍，也在无网络流量的离线编译阶段每秒
产生一千次设备锁和队列检查。

现在 CPU0 仍以 1 ms 精度检查睡眠和到期的 interval timer，只在 10 ms 用户调度节拍到期时
执行周期网络轮询。RISC-V 的硬件定时器本来就是 10 ms，因此行为不变。smoltcp 时间戳
改为直接读取 PolyHAL 单调硬件时钟，不再按调用次数累加。系统调用主动触发的网络轮询
没有删除，虚拟网卡和网络兼容能力也保持不变。

### 9.3 按最近截止时间触发 interval timer 扫描

旧 `check_itimers()` 在 CPU0 的每次定时中断中都调用 `task_snapshot()`，随后逐个获取
所有任务的锁，即使系统中没有任何任务调用 `setitimer`。复杂构建可同时存在上百个
Cargo、rustc、cc、collect2 和 ld 任务；在 LoongArch 1 ms 中断下，这会形成每秒约
十万次没有语义效果的任务检查，并持续竞争 PID registry 和任务锁。

仅统计“是否存在活动 timer”仍不够：官方命令由 `timeout` 包裹，长时间存在的一个
timer 就会使全任务扫描在整个构建期间重新开启。内核现在维护所有活动 interval timer
的最近单调时钟截止时间。绝大多数时钟中断只比较一次原子截止时间；只有真正到期时才
取得任务快照、检查 timer、产生 SIGALRM，并重新计算下一次最早截止时间。

`setitimer` 发布更早截止时间和到期扫描的精确重算都经过 `SchedulerLock` 串行化，避免
并发设置被旧扫描结果覆盖。关闭或替换当前最早 timer 时允许暂时保留一个更早的保守
提示，它最多造成一次无害的提前扫描，绝不会推迟真实 timer。周期 timer 到期后按当前
单调时间加 interval 形成下一截止点；任务锁中的 `itimer_expire` 仍是真实语义来源，
原子值只承担唤醒提示，不替代 timer 状态。

### 9.4 静态验证与边界

上述网络和 interval timer 修改已经通过：

```text
cargo check -p os --target riscv64gc-unknown-none-elf
cargo check -p os --target loongarch64-unknown-none
```

两条命令均返回 0，仅保留仓库已有警告。本节最初记录时，正在运行的旧诊断内核尚未
包含这些优化；该轮证据收集结束后已经重新构建 RV/LA release，并在下一节所述的新鲜
RV 覆盖层中启用收窄后的诊断版本。当前依然没有 commit 或 push。

## 10. 2026-08-07：顺序长测与 Cargo 指纹文件诊断

### 10.1 本机并行长测的资源边界

本机 Docker 容器可用内存上限约为 3.8 GiB。此前同时启动两个均声明
`-smp 8 -m 8G` 的 QEMU 时，LoongArch 进程以退出码 137 被容器宿主终止；退出前没有
guest panic 或内核错误。这不是 LoongArch 语义验证失败，而是两个 QEMU 竞争本地容器
内存产生的无效测试结果。

后续 RV 与 LA 都必须验证，但改为逐个运行。这个调整只改变本地执行顺序，不改变每个
guest 的官方参数：每轮仍使用 8 个 vCPU、8 GiB guest RAM、两块 VirtIO 磁盘和虚拟
网卡。官网或资源充足的 Linux 主机仍可并行启动两架构。

### 10.2 一次续跑覆盖层中的 `EISDIR`

从一份被外部中止后保留下来的 RV qcow2 覆盖层继续编译时，Cargo 报告：

```text
failed to write .../.fingerprint/libc-428f4da3e549a98e/build-script-build-script-build
Is a directory (os error 21)
```

只读检查官方基础镜像确认该路径原本不存在。对出错覆盖层的文件系统检查则确认该名字
物理上已经是链接数为 2 的 ext4 目录，而在正常 Linux Cargo 输出中它应是十余字节的
普通指纹文件。因此该错误不能按用户态误报处理，但也不能直接据此修改 Cargo 或删除
异常目录后继续计分。

### 10.3 低扰动文件类型追踪

本地增加了仅由 `buildstorm-diagnostics` feature 编译的诊断点，跟踪目标名称的
`openat(O_CREAT)`、`mkdirat`、`linkat` 和 `renameat`。第一版诊断还打印链接器对
`build_script_build-*` 输出文件的每次写入，产生大量串口 I/O 并显著改变构建速度；
现已移除逐写入、逐关闭和逐截断日志，只保留目录项类型变化事件。正常 release 不包含
这些打印。

在从官方基础镜像创建的新覆盖层上，Cargo 对相关路径执行的是：

```text
openat(..., O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666)
traced-vfs-create name=build-script-build-script-build dir=false
traced-open-created ... dir=false
```

`quote`、`proc-macro2`、`serde_core`、两个 `libc` 实例和
`parking_lot_core` 的同名指纹均被创建为普通文件，没有出现针对该名字的 `mkdirat`。
新鲜覆盖层已经越过旧覆盖层在约 16 个构建单元处的 `EISDIR` 失败点。现有证据更支持
“外部强制终止后保留的覆盖层未正常卸载或恢复不完整”，尚不支持把它定性为稳定的
Cargo 文件创建语义错误。仍需让新鲜覆盖层完整运行，确认后续不会自然发生类型损坏。
诊断日志收窄时这份覆盖层也曾被中止并随后续跑，但它仍保持上述指纹文件类型正确；
因此不能把“所有非正常停机都会触发该错误”当作已证实结论，更准确的表述是旧覆盖层
已经发生实际损坏，而损坏的精确触发窗口仍未复现。

### 10.4 本轮热路径优化

本轮同时完成了两项不依赖测例名称的通用优化。CPU0 的周期网络轮询现在只在 10 ms
用户调度节拍执行，smoltcp 时间戳直接读取平台单调时钟，避免 LoongArch 1 ms 硬件
中断把网络时间快进十倍。interval timer 则维护最近截止时间的原子提示；没有 timer
到期时，中断路径不再每毫秒复制全部任务并逐个取得任务锁。`setitimer` 发布截止时间和
到期后的精确重算通过调度锁串行化，不会因优化漏发或延迟 `SIGALRM`。

这些修改已通过 RV 与 LA 的 `cargo check`，并分别成功生成正常 release 内核。当前
RV 长测使用收窄后的诊断内核继续执行 `cargo build -p tg-xtask`；这只是题 2.2 的官方
不计时自举阶段。只有它返回 0 后继续完成正式 `cargo xtask arceos build`，并验证
目标文件至少 500 KiB，才会把题 2.2 标记为通过。

### 10.5 RV 首次越过历史故障区间

同一 RV 续跑覆盖层已经推进到 `100/446`。这次运行先后越过了旧日志中三个可重复的
失败边界：

- `37-45/446` 附近不再出现 `thiserror` 输出目录消失或 `bytes` dep-info `ENOENT`；
- `47/446` 不再因原 64 MiB 内核堆耗尽而 panic；
- `libc` 指纹文件不再以 `EISDIR` 终止，所有被跟踪的同名目标都保持普通文件类型。

这一阶段已经实际执行 `syn`、`serde` 派生宏、`tokio`、`cmake`、`aws-lc-sys` 等构建
单元，不是仅解析 Cargo 元数据。QEMU 在重负载时可持续使用约 6.3 至 6.5 个宿主 CPU，
说明 8 核调度并行有效。本地 Docker VM 只有约 3.8 GiB RAM 和 1 GiB swap，运行到该
阶段后 QEMU RSS 约 3.2 GiB；这会显著拖慢本地双重模拟，也可能在后续形成宿主 OOM，
但当前没有 guest OOM、panic 或系统调用错误。

`100/446` 仍只是阶段性进度，不能据此宣称 `tg-xtask` 已完成，更不能宣称题 2.2 已
通过。RV 需继续到 `TG_XTASK_RESULT rc=0`，随后运行正式构建；LA 也需要在 RV 释放本机
内存后按相同参数逐架构验证。

## 11. 2026-08-08：原生 QEMU 干净样本与 `exec` 的 `EFAULT`

### 11.1 区分宿主资源终止与 guest 内核故障

本机物理内存为 8 GiB，而 Docker Desktop 虚拟机实际只允许容器使用约 3.8 GiB。
Docker 内的 RV 长测曾推进到约 `355/446`，随后 QEMU 在容器占用接近上限时被宿主
强制终止；串口此前没有 guest OOM、内核 panic 或 lwext4 断言。因此该轮只能说明
Docker 资源不足，不能作为 ScintillaOS 在 `355/446` 发生语义故障的证据。

为继续保留官方 guest 配置，后续改用 macOS 上的原生 Homebrew QEMU 11.0.0。guest
参数仍然是 `-smp 8 -m 8G`、官方 BuildStorm 数据盘、ScintillaOS 根盘和 VirtIO
虚拟网卡，没有降低核数、内存或 Cargo 并行度。该方式仅减少
`arm64 macOS -> amd64 Docker -> guest ISA` 的额外一层翻译；它适合定位功能错误，
但本地耗时不能直接当作官网 Linux 基线上的时间分。

一份经历 Docker OOM 后续跑的旧覆盖层曾出现指纹文件 `ENOENT`，随后又由 rustc 报告
损坏的 `.rmeta`。物理检查可以看到目标文件最终存在，且重新打开能够写入。这份覆盖层
已经经过外部强停和手工停止，不能用它判断正常卸载链路是否可靠，也不能通过删除单个
损坏产物后把续跑结果计作完整验证。该覆盖层只保留为异常停机恢复的调试证据。

### 11.2 新鲜覆盖层首次可信失败

随后从官方 RV 原始镜像重新创建空白 qcow2 覆盖层，加载不含诊断 feature 的正常
release 内核，从零执行 `cargo build -p tg-xtask`。该轮持续越过早期历史故障点，
没有出现 EISDIR、ENOENT、堆 OOM、panic 或 `.rmeta` 损坏。在 `94/446` 编译
`futures-util` 时，Cargo 第一次报告了可归因于干净运行的失败：

```text
could not execute process .../bin/rustc ... (never executed)
Caused by:
  Bad address (os error 14)
```

这说明失败发生在创建 rustc 子进程到完成 `execve` 之间，而不是 futures-util
源码编译阶段。Cargo 的其他并行任务仍在正常收尾，`aws-lc-sys` 继续产生磁盘写入，
因此不是全局调度停滞。该轮必须等待 Cargo、文件系统同步和 QEMU 正常关机后再复用
覆盖层，不能在仍有写入时强制停止。

### 11.3 低扰动定位方案

本地 `buildstorm-diagnostics` feature 增加了两层只在诊断内核出现的记录：

- 系统调用统一返回 `EFAULT` 时记录 TID、系统调用号和六个参数；
- `execve` 复制 path、argv 指针槽、argv 字符串、envp 指针槽和 envp 字符串失败时，
  记录精确阶段、用户地址、VMA 校验结果及首尾页表映射。

正常 release 不编译这些串口输出。诊断内核已通过 RV release 构建并单独保存，不会
覆盖当前长测使用的内核。同时准备了本地并发 exec 压力入口，以 8 路并发和长 argv
重复执行真实 ELF，逐个检查子进程退出码。压力入口不属于官方评测流程，只用于判断
问题位于通用 `clone/fork/execve` 地址空间语义还是 Cargo 特有时序。

官方镜像中的 GNU `timeout` 也经过实际 ELF 符号和控制流检查：它先尝试
`timer_create`，收到 `ENOSYS` 时明确回退到 `alarm`。ScintillaOS 已通过
`setitimer` 支持 alarm 路径，因此在出现相反的运行证据前，不应贸然新增一套与当前
故障无关的 POSIX timer 实现。

### 11.4 当前验收边界

本节记录时题 2.2 仍未通过。下一步是在当前失败轮正常退出后，用诊断内核和同一份
未被强停的覆盖层重试，先确定返回 `EFAULT` 的具体系统调用与用户指针阶段，再构造
最小通用复现并按 Linux 地址空间语义修复。修复后必须重新创建干净覆盖层完成
`TG_XTASK_RESULT rc=0`，随后执行正式
`cargo xtask arceos build -p arceos-helloworld --arch riscv64`，要求返回 0 且产物
不小于 500 KiB；RV 完成后还需以相同规则验证 LoongArch。当前仍没有 commit 或 push。

## 12. 2026-08-08：并发 `fork` 与内核用户写回之间的 COW 竞态

### 12.1 `Bad address` 的系统调用级证据

在上一节的干净样本正常退出后，使用仅增加低频错误日志的诊断内核续跑同一工作负载。
Cargo 再次报告某个 rustc 子进程 `never executed`，内核同时记录：

```text
[buildstorm-diag] EFAULT tid=25 nr=59 a0=0x103c1fe648 a1=0x80000 ...
```

RISC-V 使用 Linux generic syscall ABI，其中系统调用 59 是 `pipe2`；第二个参数
`0x80000` 是 `O_CLOEXEC`。失败地址位于调用线程的用户栈，VMA 本身有效。Rust 标准库
启动子进程时会先建立一个 close-on-exec 错误报告管道，使子进程能在 `execve` 失败时
把 errno 返回父进程。该管道还未建立成功时，用户态只能把结果报告为进程“从未执行”。
因此这里的直接故障不是 ELF 装载器拒绝 rustc，而是 `pipe2` 已经分配两个文件描述符
后，向用户栈写回 `pipefd[2]` 时错误返回了 `EFAULT`。

### 12.2 根因：校验与写回之间允许另一个线程执行 `fork`

ScintillaOS 的用户缓冲区写入分成两个阶段。`prepare_task_user_access` 先验证 VMA，必要时
处理缺页或 COW，使目标 PTE 可写；随后释放任务和 VM 锁。真正的
`MemorySet::copy_to_user` 之后重新取得锁，通过物理直接映射写入用户页。

单线程时这两个阶段之间不会改变自身页表。Cargo 进程则有多个 pthread，共享同一个
`CLONE_VM` 页表和 VMA 元数据。线程 A 完成预处理后，线程 B 可以在间隙中调用
`fork`。`MemorySet::from_existed_user` 按 Linux 的 COW 语义，在共享 VMA 锁内把父进程
当前所有私有可写页重新映射为只读，并建立子进程页表。线程 A 随后取得同一个锁时，
看到的 VMA 仍然合法，但栈页 PTE 已经重新变成只读；旧实现把这个内核内部的瞬时 COW
转换直接暴露成 `EFAULT`。

VMA 锁已经保证 `fork` 改页表和最终物理写回不会同时发生，缺少的不是更大的全局锁，
而是两阶段之间的重验证。强行让整个预处理阶段跨远程 TLB shootdown 和调度一直持锁，
会扩大锁作用域并破坏现有锁序，也没有必要。

### 12.3 通用修复语义

`copy_to_task_user` 现在在最终写回发现 PTE 已发生变化时，重新执行
`prepare_task_user_access`。如果地址仍属于可写 VMA，内核重新完成 COW 后再写回；如果
另一个线程确实执行了 `munmap`、修改了权限，或页分配失败，预处理仍会返回失败，系统
调用继续按 Linux 语义返回 `EFAULT` 或对应错误。每连续 8 次竞态会主动让出 CPU，避免
在极端连续 `fork` 下独占当前 hart。跨页写回可能在失败前完成前半部分，但重试写入的
是同一组字节，因此该操作保持幂等。

这个修改作用于所有内核向用户地址写回的系统调用，不包含 Cargo、rustc、路径或测试
名称判断。`pipe2` 只是第一个稳定暴露竞态的调用；`wait4`、`statx`、`getdents64`、
`read` 等同样可能在多线程 `fork` 时经过这条通用路径。诊断 feature 会打印前四次
`copy-to-user-retry`，正常 release 不包含这些输出。

### 12.4 当前验证状态

修复后已经完成以下静态和基础运行验证：

```text
cargo check -p os --target riscv64gc-unknown-none-elf
cargo check -p os --target loongarch64-unknown-none
EXEC_STRESS_RESULT attempts=800 failures=0
```

其中 shell 压力测试以 8 路并发和长 argv 启动 800 个真实 ELF，全部正常退出。但每个
shell 后台任务属于不同进程，不能完整复现“同一个共享地址空间中的多个线程同时
`pipe2/fork`”。因此本地又加入了独立诊断模式：guest 内用官方镜像自带的 C 编译器
构建 8 个 pthread 的压力程序，各线程同步开始并反复执行
`pipe2(O_CLOEXEC)`、`fork`、`execve(/usr/bin/true)` 和 `waitpid`，分别统计各阶段 errno。
该程序不进入官方提交镜像，也不改变评测命令。

本机当前只有 8 GiB 物理内存，一轮不含本修复的旧诊断内核仍在使用官方
`-smp 8 -m 8G` 参数续跑，并已推进到约 `260/446`。此进度只能证明旧内核本轮尚未再次
撞中概率性 `pipe2` 竞态，不能作为修复通过证据。为避免第二个 8 GiB QEMU 触发宿主
换页或外部终止，pthread 压力测试将在该轮自然结束后执行。

题 2.2 的验收边界没有降低：pthread 复现必须无失败，修复后的 RV 和 LA 仍需分别在
官方双盘、VirtIO 网络、`-smp 8 -m 8G` 配置下完成 `cargo build -p tg-xtask`；随后
正式 `cargo xtask arceos build -p arceos-helloworld --arch <arch>` 必须返回 0，且
产物不小于 500 KiB。满足这些条件前仍不能声明题 2.2 已通过。当前仍没有 commit 或
push。

## 13. 2026-08-08：`MADV_DONTNEED` 的真实物理页回收

### 13.1 原实现的常驻内存问题

Rust 编译器和 glibc 分配器在释放较大的私有匿名区域时会调用
`madvise(MADV_DONTNEED)`。Linux 保留 VMA，但丢弃范围内已驻留的私有匿名页；
后续访问再通过缺页获得全零内容。原实现只把页内容清零，却始终保留 PTE 和
`FrameTracker`。对 COW 共享页，它甚至会额外分配一个新零页。这保证了下次读取为零，
但没有实现 `DONTNEED` 的回收语义。在数百个 rustc 子进程和多轮链接工作负载下，
已解除用户态需求的物理页仍会长期占用 guest 内存。

### 13.2 修复后的页表、TLB 与 COW 顺序

新实现先在地址空间的 VM 锁内完成范围校验，只接受私有匿名 VMA。对其中的
已驻留页依次执行：

1. 从页表删除 PTE，并从 VMA 的驻留页元数据中移除 `FrameTracker`；
2. 暂时把移除的帧保留在本次操作结果中，不立即还给物理页分配器；
3. 释放 Task/VM 锁后，对当前使用同一页表的其他核执行远端 TLB shootdown；
4. 所有可能持有陈旧转换的核均已失效该范围后，才释放保留的帧引用。

这个顺序避免了物理页被重新分配后，其他核仍通过陈旧 TLB 读写旧页的竞态。如果
该页因 `fork` 与子进程 COW 共享，移除当前地址空间的 `FrameTracker` 只会降低
引用计数，子进程仍持有并可正常访问原页。当前进程下次触及该 VMA 时，普通匿名
缺页路径重新分配已清零的帧，与 Linux 对私有匿名 `MADV_DONTNEED` 的可观察语义一致。

### 13.3 当前验证边界

该修复与并发用户写回重试一起已通过 RV 和 LA 的 `cargo check`，并生成了不包含
BuildStorm 诊断输出的 RV release 内核。当前运行的旧内核长测已推进到约
`271/446`，但它不包含本节修复，因此不能用它评估内存回收效果。待该轮正常关机后，
将先以最新诊断内核完成 pthread 并发 `pipe2/fork/execve/waitpid` 压力验证，再以
正常 release 内核执行 RV 的官方完整构建。本节修复还没有得到完整题 2.2 运行结果，
不将静态检查或中间进度冒充为通过证据。

## 14. 2026-08-08：修正 RISC-V/LoongArch 的 `epoll_event` ABI

### 14.1 官方用户态与内核布局不一致

`tg-xtask` 不是一个只执行同步 Cargo 命令的简单包装器。它的 `main` 使用
`#[tokio::main]`，程序启动时先创建 Tokio 多线程运行时，然后才进入 axbuild 并启动
下一轮 Cargo/rustc。Tokio 在 Linux 上通过 `epoll` 和 `eventfd` 传递唤醒事件，
`epoll_event.data` 必须不变地把用户态的 64 位令牌返回。

原内核把 `struct epoll_event` 硬编码为 12 字节：`events` 占据偏移 0..4，
`data` 紧接在偏移 4..12。这是 x86_64 为兼容历史 ABI 使用的 packed 布局，
不是比赛两个架构的布局。直接从官方 BuildStorm RV 和 LA 磁盘镜像中读取
`/usr/include/<triplet>/sys/epoll.h` 与 `bits/epoll.h` 后可以确认，两者的
`__EPOLL_PACKED` 均为空；结构按 8 字节自然对齐，总长 16 字节，
`data` 位于偏移 8..16。Rust `libc` 也只对 x86_64 和 32 位 x86 GNU 标注
`repr(packed)`，RISC-V 和 LoongArch 同样使用 16 字节布局。

旧实现会在 `epoll_ctl` 中把用户结构的 4 字节对齐填充误当作 `data` 低位，
并丢弃真实令牌的高 32 位；`epoll_pwait` 又会以 12 字节步长写回事件数组。
即使文件的可读状态判断正确，Tokio 也可能收到错位令牌或从数组错误位置读数据，
表现为运行时启动后停滞，而不是 `cargo build -p tg-xtask` 编译阶段报错。

### 14.2 按 Linux UAPI 进行通用修复

内核现在对 RV 和 LA 统一使用 16 字节事件步长。`epoll_ctl` 复制入参时从
偏移 8 解码完整 `u64 data`；`epoll_pwait` 输出时写入 4 字节 `events`、
4 字节零填充和 8 字节 `data`。用户输出范围校验和多事件数组的步长计算
也同步改为 16 字节。这项修复仅依赖目标架构的 Linux ABI，不包含 Tokio、
Cargo 或评测命令判断。

修改已通过两个内核目标的 `cargo check`。下一轮 guest 压力程序会直接创建
`eventfd` 和 `epoll` 实例，写入 eventfd 后调用 `epoll_wait`，校验官方 glibc 报告的
`sizeof(struct epoll_event) == 16`、`EPOLLIN` 和一个 64 位固定令牌全部原样返回。
随后还将运行 `target/debug/tg-xtask arceos config ls`，覆盖 Tokio 运行时的真实
初始化与退出。这些动态结果尚未产生，因此本节目前只记录已确认的 ABI 错误、
修复和静态检查，不将其写为题 2.2 已通过。

### 14.3 避免 `EPOLLET` 在两次采样之间丢失边沿

只修正 `epoll_event` 结构布局仍不足以保证 Tokio 唤醒可靠。原实现通过
`last_ready` 记录上一次 `epoll_wait` 采样的就绪位。对边沿触发的
eventfd，如果其计数器在两次采样之间经历“非零→零→非零”，两次
采样都会看到可读。旧代码因此认为没有新边沿，虽然 Linux 已经对第二次
从 0 到非 0 的转换排队了新事件。这个时序可以发生在一个 Tokio worker
排空唤醒 fd、另一 worker 立即再次唤醒的窗口内，结果是运行时有可执行
任务却永久停在 `epoll_wait`。

`File` 现在提供可选的 `readiness_generation`。eventfd 在设备锁内只对
可读或可写状态真正发生转换的操作递增原子代次：例如计数器从 0 增加、
从 1 降为 0，或从上限恢复可写。epoll 关注项同时记录上次的就绪位与
就绪代次。对 `EPOLLET`，只要当前仍就绪，且就绪位从 0 变为 1 或代次
发生变化，就返回新边沿。如果当前已不就绪，内核只更新基线而不伪造事件；
普通 level-triggered 关注不使用代次条件。因此这不是将边沿触发简化成
会忙轮询的电平触发。

同时修正了 eventfd 的 I/O 长度规则。Linux 只在用户缓冲区小于 8 字节时
返回 `EINVAL`；对更大缓冲区仍只传输并返回 8 字节。新回归使用
`EPOLLIN | EPOLLET` 注册同一 eventfd，先验证延迟写入的第一次唤醒，
读空后在下一次 `epoll_wait` 前立即写入，并要求第二个边沿、64 位 data
和计数值都正确；还用 16 字节缓冲区验证返回值必须是 8。修复已通过
RV 和 LA 的 `cargo check`，但这组动态回归尚未用新内核执行，不作为
题 2.2 通过证据。

## 15. 2026-08-08：补齐多线程进程组的子进程等待语义

### 15.1 问题不是单个 `waitpid` 返回码，而是子进程归属范围

BuildStorm 的 `tg-xtask` 由 `#[tokio::main]` 启动多线程运行时。异步任务可以在
一个 worker 线程上启动 Cargo/rustc 子进程，之后在另一个 worker 线程上轮询
并回收它。Linux 默认允许调用线程等待同一线程组中任意线程创建的子进程；
只有内部选项 `__WNOTHREAD` 才把范围限制为当前 TID。

原实现的 `wait4/waitpid` 和 `waitid` 只遍历当前
`TaskControlBlockInner.children`。如果子进程由线程 A 创建，线程 B 执行等待，
B 会把真实存在的子进程当成“无子进程”。即使 B 已经进入 waitpid 阻塞队列，
子进程退出路径也只唤醒直接父 TID，不会唤醒同进程的 B。两个缺口
分别可以表现为过早返回 `ECHILD` 或永久睡眠，而且它们都只在真实 pthread
调度下出现，单线程 shell 回归无法覆盖。

### 15.2 按 Linux 进程组语义扫描、回收和唤醒

新实现首先用全局任务快照收集当前 TID 及同 TGID 成员，再依次扫描每个
成员的 `children`。默认 `waitpid` 和 `waitid` 都使用线程组范围；
`__WNOTHREAD` 仍只查当前 TID，`waitid(WNOWAIT)` 只观察僵尸状态而不从实际
拥有者的子进程向量中移除它。选中并移除僵尸的操作在拥有者的
TaskLock 内完成，避免两个 worker 同时回收同一子进程。

阻塞前的二次检查使用事先收集好的拥有者快照：当前线程先注册进
WaitPid 队列，再在禁止本地中断的窗口内重查“子进程已可回收”和可中断信号。
这样既不在 IRQ-off 区域内分配快照，也不会丢失“首次检查后、真正睡眠前”
发生的退出事件。子进程正常退出、强制退出和延后资源清理完成三条路径现在
都会唤醒直接父任务及其同 TGID 成员。`SIGCHLD` 仍依 Linux 的父子关系
投递给直接父任务，只把等待队列唤醒扩展到线程组，没有改变信号归属。

该修复不匹配 Tokio、Cargo、命令行或测试名称，对任意 pthread 程序的
`fork/clone + wait4/waitid` 都使用同一套规则。为保持现有用户库兼容性，
`waitpid` 在完全没有匹配子进程时仍保留项目历史返回值 `-1`；`waitid`
继续返回 Linux `ECHILD` 对应的 `-10`。

### 15.3 针对竞态而非评测输出的动态回归

本地诊断脚本已增加一个独立 C pthread 压力测试：线程 A 创建子进程，子进程
阻塞在管道上；线程 B 先调用 `waitpid(child, ..., WNOHANG)`，此时必须返回 0，
而不是把由 A 创建的子进程误报为 `ECHILD`。B 随后调用阻塞
`waitpid(child, ...)`并应真正睡眠；A 确认 B
已进入等待阶段后释放子进程。回归同时验证 B 能跨线程找到子进程、
退出事件能唤醒 B，以及回收的退出状态是 37。它与已有的
pthread `pipe2/fork/execve/waitpid`、`MADV_DONTNEED` COW/回收和
`epoll/eventfd` ABI 回归组成同一轮 guest 压力测试。

目前脚本通过 `bash -n`，新内核源码已通过 RV 和 LA 的 `cargo check`。
但当前仍在运行的旧 RV 长测内核不包含本节修复，因此不能用它的中间进度
证明线程组等待已通过。待旧长测自然结束并正常卸载磁盘后，将用包含本修复的
新诊断内核要求 `CROSS_THREAD_WAIT_RESULT ... failures=0`，再进入正式
`tg-xtask` 运行与 RV/LA 2.2 完整构建。在这些动态结果产生前，本节只记录
已发现的 Linux 语义缺口和修复，不声称题 2.2 已通过。

## 16. 2026-08-08：官方磁盘重建与验证证据隔离

本轮旧 RV 长测运行期间，QEMU 已打开的
`/private/tmp/scintillaos-final-rv-pub.img` 目录项被外部清理。运行中的
QEMU 仍持有原 inode 的只读文件描述符，因此 guest 仍能从 backing file 读数据，
也能继续把修改写入 qcow2。但 QEMU 退出后该 inode 会被释放，原 qcow2 将变成
缺少 backing file 的不完整差分盘。因此本轮可以用于观察“旧内核能否继续推进”
以及发现新停滞位置，不将它的缓存盘或中间进度当作最终干净验证。

本地仍保留比赛下载的 `sdcard-rv.img.xz` 和 `sdcard-la.img.xz`，两份原始包
均已通过 `xz -t`。新增的本地脚本 `tmp/prepare_buildstorm_image.sh` 默认拒绝
覆盖已存在的基础盘，并按以下顺序重建：

```text
xz 完整性检查
解压到同目录临时文件
扩展原始文件到 14 GiB
e2fsck 强制检查 ext4
resize2fs 扩展文件系统
原子改名到新基础盘路径
```

当前长测正在使用旧 inode，因此只写入并检查了脚本，没有在运行期间重建或
替换任何磁盘。待 QEMU 正常关机后，先重建 RV 基础盘，为新内核创建新 qcow2，
完成压力回归和正式构建；随后以同样流程重建并验证 LA。最终 2.2 结论只采信
这些重建后的独立双盘、`-smp 8 -m 8G` 运行结果。

## 17. 2026-08-09：文件系统语义、锁粒度与读写热路径优化

### 17.1 停止无诊断长构建并建立短周期验收链路

完整 BuildStorm 一次需要数小时，继续只观察最终是否超时无法区分语义错误、锁竞争和
纯性能问题。本轮停止了正在进行的盲目长构建，先固定三组可在数分钟内完成、但仍使用
官方 glibc 数据盘和 8 核 QEMU 的回归：文件系统快速压力、VM/锁压力和两个真实
`rustc` 并行编译。所有优化都先经过静态检查和这三组回归，再决定是否保留。

文件系统快速压力会并行创建和替换 960 个目标文件，执行 16 轮大文件写入、重命名和
截断，覆盖稀疏写、稀疏扩展、跨空洞 `pwrite`，并增加同一 inode 的并发语义检查。
8 个独立 `O_APPEND` 文件描述符共同写入 1024 条定长记录，验证 append 定位与写入的
原子性；8 个线程再对同一文件执行 1024 次不重叠 `pwrite`，验证共享文件大小和数据
不会被其他打开实例的陈旧 EOF 覆盖。这些程序只存在于本地忽略的 `tmp` 脚本，不进入
提交镜像，也不修改官方判分流程。

### 17.2 稀疏文件与多打开实例的 Linux 语义

原来的稀疏写会在用户偏移超过 EOF 时，从旧 EOF 到目标偏移逐字节写零。大空洞因此被
错误地物理分配，并在持有 lwext4 卷锁期间产生与空洞大小线性相关的 I/O。现在普通写、
`pwrite` 和路径回退写都使用 `ext4_ftruncate` 只扩展逻辑大小；未映射数据块读取为零，
真正写到空洞末端时才分配目标块。这与 Linux 稀疏文件语义一致，不依赖测试路径或文件名。

lwext4 的非 extent 映射路径还补上了“创建映射后物理块号仍为 0”时的真实块分配；对
新分配块的首尾非整块写，先将临时块清零再覆盖用户数据，避免把磁盘旧内容暴露到空洞
边界。回归验证了 8 MiB、16 MiB 和 20 MiB 三个空洞边界的大小、零读与实际数据。

不同 `open` 产生的 `ext4_file` 原先各自缓存 `fsize`。一个描述符扩展或截断文件后，
另一个描述符可能继续按旧 EOF 读取，`O_APPEND` 也可能从陈旧位置开始写。VFS 现在按
`(superblock, inode)` 维护共享运行时文件大小，并用同一 inode 的可睡眠 mutation 锁
串行化写、append 和 truncate。成功提交到 lwext4 后才发布新大小，读和写在操作前刷新
本地句柄，因此既保持 Linux 的可见性，又不会在失败写入时提前暴露错误大小。

### 17.3 竞争感知写事务与可回收 readahead 缓冲

固定 32 KiB 写分片能够避免单个大写长期占用卷锁，但无竞争的大文件写也会被拆成大量
独立 lwext4 事务。`SleepLock`/`FsLock` 现在可只读查询是否存在等待者。写路径在有等待者
时继续使用 32 KiB 分片并在片间释放卷锁；没有等待者时最多合并为 256 KiB 事务，之后
仍释放并重新竞争锁。这个策略只根据真实锁竞争选择批量大小，不改变写入顺序、错误返回、
`fsync` 或写回语义。

首次小读会进行 64 KiB readahead。原实现每次都新建并销毁 `Vec`，复杂构建对成千文件
重复这一分配过程。新实现使用 16 个固定槽位保存已释放缓冲，获取和归还均发生在 lwext4
卷锁之外；槽位不足时仍可临时分配，内存不足时仍退化为原有直接读。缓冲归还前清空长度，
再次使用时完整覆盖，文件页缓存发布仍受 generation 校验保护，因此不会在文件之间泄露
数据或发布失效读结果。

本轮还尝试过对第二次访问的 4 KiB 对齐块进行 ghost-admission。该方案所有语义回归均
通过，但双 `rustc` 从 83.09 秒退化到 88.39 秒，说明当前负载中一次性文件数据多于重复
元数据，额外装入和复制造成负收益，因此相关代码已完整撤销，没有留在当前版本。

### 17.4 当前测量与验证边界

当前无诊断 RV release 内核得到以下结果：

```text
FS_QUICK_INODE_PASS files=960
FS_QUICK_TRUNCATE_PASS rounds=16
FS_QUICK_SPARSE_PASS
FS_QUICK_SHARED_INODE_PASS
BUILDSTORM_FS_QUICK_PASS
FORK_COW_PASS
VM_LOCK_STRESS_PASS threads=8 rounds=48 elapsed_ms=6456
RUSTC_PARALLEL_RESULT jobs=2 succeeded=2 elapsed_s=80.52 bytes=9620312
```

同条件下，本轮优化前双 `rustc` 样本为 82.69 秒，加入竞争感知写批处理后为 83.09 秒，
再加入 readahead 缓冲复用后为 80.52 秒。最后一项相对直接前序样本约快 3.1%，相对初始
样本约快 2.6%；这是单轮 QEMU A/B 数据，只作为优化方向证据，不冒充稳定统计结论。

包含 `lockdep` 的 RV 内核也完整通过上述文件系统回归，没有报告缓冲池、VFS 元数据锁、
inode mutation 锁、页缓存锁和 lwext4 卷锁之间的顺序反转。RV 的
`cargo check -p os --target riscv64gc-unknown-none-elf` 与 release 构建均通过。当前主机
缺少 `loongarch64-linux-gnu-gcc`，LA 静态检查停在 lwext4 C 交叉编译器查找阶段，尚未
完成本轮 LA 动态验证；这不是源码编译错误，但仍属于必须补齐的验证边界。

这些结果证明短回归和两个真实 `rustc` 已通过，不能证明官方完整
`cargo xtask arceos build` 已完成。题 2.2 的最终结论仍要求 RV、LA 分别使用官方双盘、
`-smp 8 -m 8G` 完整返回 0，并检查最终产物大小不小于 500 KiB。

## 18. 2026-08-10：写回页缓存与可扩展 VFS 缓存

### 18.1 优化目标与验收方法

前一轮已经证明继续微调写分片或调度参数只能获得个位数百分比的变化。本轮转而补齐
Linux 文件系统性能中的两个上层机制：第一，普通文件写入先进入按 inode 共享的脏数据
缓存，只在 `fsync`、`sync`、截断、内存压力或关机等真实同步边界回写；第二，路径解析
使用可失效的正、负 dentry 缓存以及 inode/句柄模板，避免每个 `openat` 都重新进入
lwext4 做全路径遍历。实现不识别 BuildStorm 文件名、进程名或测试输出，所有快路都由
inode、目录代际、内存压力和 Linux 系统调用语义决定。

为避免三小时完整构建掩盖根因，本轮使用四类短测逐项验收。`FS_QUICK` 在 8 核上并发
创建、写入和重命名 960 个文件，并覆盖大文件截断、稀疏文件、同 inode 的 append 与
`pwrite`；冷目录测试创建 2500 个文件，卸载重挂载后执行 `readdir` 与逐项
`openat(dirfd, name)`；命名空间测试由 8 个线程执行 640 组创建、写、重命名、读取和
删除，并在 `fsync`、卸载和重挂载后复验 1 MiB 文件；部分脏读测试先持久化 512 个
16 KiB 文件，再各改写 16 字节并完整读取。测试程序均位于被 Git 忽略的 `tmp` 目录，
没有进入评测镜像或修改官方脚本。

### 18.2 按 inode 共享的延迟写回

原来的 `write`/`pwrite` 会直接进入 lwext4，多个编译线程因此在每个小写上竞争整卷锁。
现在 VFS 以 `(superblock, inode)` 为键保存共享运行时大小、文件句柄模板、mutation 锁
和有序脏区间。连续区间在内存中合并，后写入的重叠区间覆盖先写入区间；每 inode 最多
缓存 8 MiB，所有 inode 的总量不超过当前内核堆的四分之一。当前 2 GiB 测试配置的内核
堆为 131076096 字节，因此总阈值为 32769024 字节。达到单 inode 或全局压力阈值时按
最早变脏顺序回写，而不是无界占用内核堆。

`close` 不再隐式等价于 `fsync`。`fsync`、`fdatasync`、`sync`、`truncate`、需要直接
访问磁盘旧内容的操作和关机仍显式回写；最终 unlink 且没有其他链接的 inode 可以直接
丢弃脏数据，因为此时已经不存在 Linux 可观察对象。普通读如果完全落在脏区间，直接从
缓存返回；若只部分命中，旧实现会先刷完整 inode，本轮改为读取磁盘中的干净区间、对
物理 EOF 与逻辑 EOF 之间的洞补零，再按写入顺序叠加脏区间。合并期间始终持有同一
inode mutation 锁，因而不会与另一个 CPU 的写回、截断或新写入组成不一致快照。

部分脏读的同脚本 A/B 结果如下：

```text
指标                         修改前      修改后      变化
512 文件完整读取耗时          5130 ms     946 ms      -81.56%
pending writeback 批次        1057        519         -50.90%
lwext4 FsLock 入口            4502        4011        -10.91%
脏缓存合并读取                0           553         新增语义快路
```

该测试逐字节校验 512 个文件，修改后全部通过。更综合的 `FS_QUICK` 也全部通过，耗时从
同机前一轮的 39.17 秒降到 34.70 秒，约减少 11.41%；卷锁入口从 9376 降到 9003，
回写批次从 1345 降到 1319，并记录到 95 次部分脏合并读。由于宿主是 macOS，QEMU
块设备 DMA 延迟存在明显抖动，耗时只作为同负载单轮 A/B 证据；回写次数和锁入口是更
稳定的实现计数。

### 18.3 正负 dentry、inode 与句柄模板缓存

目录子项由线性 `Vec` 改为 `BTreeMap`，大构建目录的查找和插入由线性复杂度降为
对数复杂度；ENOENT 结果保存在有上限的负 dentry 集合中。每个目录维护偶数稳定、奇数
更新中的 namespace epoch。创建、删除和重命名先把 epoch 变为奇数，完成 ext4 与缓存
发布后再进入下一偶数代；并发 lookup 只有在前后代际相同的情况下才能发布正、负缓存，
因此不会把变更期间看到的旧 ENOENT 长期缓存。

新文件创建现在一次 lwext4 事务同时返回 inode 信息和可复用文件句柄，不再执行“先查
不存在、创建、再查新 inode、再重新打开”的多次路径遍历。普通 `O_CREAT`、`O_EXCL`、
mkdir 与 rename 均先使用稳定代际内的缓存；重命名成功后直接按源 inode 发布新的叶
dentry。按 inode 共享的句柄只作为不可变模板，每个 open-file description 仍复制独立
的 offset、flags 和游标，并且真正访问 lwext4 时仍持有卷锁。

在同一 `FS_QUICK` 负载上逐步加入快路时，原始 inode 查询从 5915 依次降为 4289、
3299、2323、1027 和 383，累计减少 93.52%；lwext4 入口从 14645 依次降为 13188、
12114、11324、11209、9716 和 9037，累计减少 38.29%。各阶段分别对应原子创建复用
句柄、`O_EXCL`/mkdir 缓存、取消 rename 冗余预查询、打开句柄模板、重命名叶缓存与
一般 `O_CREAT` 快路。最终综合回归因加入部分脏读后为 9003 次，仍低于上述阶段结果。

### 18.4 冷目录与 dirfd 代际复验

仅缓存 `readdir` 返回的名字和 inode 仍不够。原实现每次相对目录 fd 执行 `openat`
前，都会再次按目录完整路径查询该目录是否存在。冷目录测试因此对同一个目录重复执行
2500 次 raw inode 查询。现在每个 child dentry 记录父目录被成功验证时的 namespace
epoch；父目录代际稳定且未变化时直接确认 dirfd，有变更时才物理复验一次，成功后发布
新代际。权限检查、子项 lookup 和最终 open 没有被跳过。

同一冷目录脚本的结果如下：

```text
指标                         修改前      修改后      变化
2500 文件扫描耗时             15362 ms    6095 ms     -60.32%
raw inode 查询                2875        375         -86.96%
lwext4 FsLock 入口            12031       9531        -20.78%
readdir 发布的 dentry         2500        2500        内容一致
dirfd epoch 快路               0           2500        新增
```

为证明该机制不是无条件信任旧路径，又在打开目标目录后对其父目录创建一个无关兄弟文件。
加强版结果为 inode 查询 376、卷锁入口 9532、epoch 快路 2499、物理复验 1：一次父目录
变化只触发一次真实复验，随后恢复快路。2500 个文件均成功打开且空文件读取返回 0。

### 18.5 重命名、硬链接、并发和持久化边界

延迟写回使“路径名”和“仍存活的 inode”不再等价。最终 unlink 的打开文件会先重命名到
隐藏孤儿名，最后一个 fd 关闭后再删除；rename 覆盖一个仍打开的目标时，也保留旧 inode
直到旧 fd 关闭。若旧 inode 还存在硬链接，关闭前必须写回通过旧 fd 产生的数据，避免
别名看到旧内容；写回失败时保留隐藏链接，防止 inode 被复用后继承错误缓存状态。

RV `-smp 8` 的定向测试验证了：打开 fd 在 rename 后继续可用；被覆盖目标的硬链接能
看到旧 fd 在 rename 后写入的数据；新路径只看到替换文件；`RENAME_NOREPLACE` 返回
`EEXIST`；空目录替换成功；文件覆盖目录返回 `EISDIR`。输出为
`RENAME_SEMANTICS_PASS`。

8 线程命名空间测试输出 `NAMESPACE_STRESS_PASS operations=640`，随后卸载和重挂载得到
`NAMESPACE_PERSIST_PASS`。最新一轮耗时 5631 ms，8 个 CPU 都实际调度过用户线程，
最终 `dirty_bytes=0`，没有 panic、死锁或数据不一致。前一轮同测试为 8680 ms，但宿主
抖动较大，因此不把 35.13% 的单轮差值作为稳定加速结论；该测试的主要证据是并发语义和
跨挂载持久化均通过。

### 18.6 当前边界

本轮通过了带 `lock-stats` 的 RV release 构建、`cargo check`、`FS_QUICK`、部分脏读、
冷目录、rename/硬链接语义以及命名空间持久化测试。当前主机仍缺少
`loongarch64-linux-gnu-gcc`，因此没有重新生成 LA 的 lwext4 静态库，也没有执行本轮
LA 动态测试；本轮实际修改均位于 Rust VFS、系统调用和调度代码，没有改动 lwext4 C ABI，
但这不能替代 LA 实机验证。

当前块设备层仍只有每卷 4096 个 4 KiB 槽位。`FS_QUICK` 最终统计中，测试盘发生
14097 次脏槽驱逐和 19896 个阈值回写块，说明下一阶段最可能的大头是可回收、按内存压力
伸缩的块/页缓存，而不是继续扩大固定数组或微调一个写分片常量。在没有回收机制和
双架构内存压力验证前，本轮没有贸然扩大固定缓存，也没有宣称官方 2.2 已经完成。

## 19. 2026-08-11：全局 CLOCK 块缓存与高低水位合并写回

### 19.1 问题定位与修改前基线

上一节确认块层仍是明显瓶颈后，本轮没有直接扩大 4096 个槽位。原实现按
`block % 1024` 将每个磁盘块固定映射到一个四路集合，只能在该集合的四个槽中查找和
淘汰。这样即使其他集合仍有空闲或干净槽，发生冲突的集合也会同步写回并淘汰脏块。
缓存脏块达到 75% 后，原实现还会在每次 miss 的前台路径循环写回 8 块；这些小批量
写回发生在 lwext4 卷锁内部，所有等待同一卷的编译线程都会被阻塞。

在修改块缓存之前，使用 `/opt/homebrew/bin/qemu-system-riscv64`、同一份官方测试盘
qcow2 快照、`-smp 8 -m 2G` 和同一个带 `lock-stats` 的 RV release 内核运行
`tmp/run_buildstorm_fs_quick.sh rv`，得到如下基线：

```text
FS_QUICK_INODE_PASS files=960
FS_QUICK_TRUNCATE_PASS rounds=16
FS_QUICK_SPARSE_PASS
FS_QUICK_SHARED_INODE_PASS
BUILDSTORM_FS_QUICK_PASS
FS_QUICK_ELAPSED_S 71.61
block-cache: hits=22924 misses=40264 dirty_evictions=14095
block-cache: threshold_flush_blocks=19928 explicit_flush_blocks=6143
block-device: writes=16926 bytes=151945216 avg=8977 write_dma_us=12818809
Fs lock: acquisitions=8997 wait_total_us=138109226
```

命中和 miss 次数可以作为后续 A/B 的工作量校验；`14095` 次脏淘汰与不到 9 KiB 的
设备平均写入直接证明固定四路映射和前台小批写回没有形成有效的空间合并。

### 19.2 预分配哈希索引与全局 CLOCK 淘汰

`BlockDeviceStream` 现在在挂载时为 4096 个缓存槽预分配 8192 个哈希桶，桶内通过缓存
槽自身的 `hash_next` 串联。负载因子不超过 0.5，块号查找保持均摊 O(1)，但不再把某个
磁盘块限制在四个固定槽中。替换槽位前先从原哈希链移除，装入新块后再插入对应链；整个
过程只修改预分配数组，没有在 lwext4 持卷锁的设备回调内申请或释放堆内存。哈希桶本身
在 64 位平台占 64 KiB，槽位只增加链指针和引用状态，内存不会随 I/O 数量增长。

缓存填满后使用全局 CLOCK 扫描。每次命中设置引用位；替换时先给近期访问页第二次机会，
并优先选择未引用的干净槽。所有槽共享同一替换域，因此只要全缓存还存在干净页，就不会
因为局部哈希冲突写回脏页。找不到普通候选时仍保留脏页同步写回兜底，确保内存压力下能够
前进，而不是假定测试工作集永远小于缓存。

### 19.3 高低水位写回与物理连续合并

脏块达到缓存的高水位 75% 时，不再让后续每个 miss 各写 8 块，而是一次选择最早变脏的
块回写到 50% 低水位。待写槽先按年龄选取，再按物理块号排序；相邻块最多合并成现有
128 KiB scratch 缓冲对应的一次 VirtIO 请求。高低水位之间的滞回避免连续 miss 反复
触发扫描和微小写回，也为全局 CLOCK 持续提供干净候选。该机制对所有 ext4 负载生效，
没有识别 BuildStorm 文件名、目录或进程。

`fsync`、`sync`、卸载与关机仍调用完整 drain，并继续执行底层设备 `flush`。高低水位
只改变脏块在内存压力下的内部调度，不推迟显式持久化边界，也不修改写入顺序和错误返回。
当前实现仍在卷锁内同步完成一次水位写回，并不等同于 Linux 独立 flusher 线程；它解决的
是冲突淘汰和请求碎片化，后续若引入异步块设备接口还可以继续把写回移出前台事务。

### 19.4 同条件 A/B 结果

改动后使用完全相同的 QEMU、镜像、内存、CPU 数和诊断特性重跑，工作量计数保持为
`hits=22980, misses=40249`，与基线的差异均小于 0.25%。结果如下：

```text
FS_QUICK_INODE_PASS files=960
FS_QUICK_TRUNCATE_PASS rounds=16
FS_QUICK_SPARSE_PASS
FS_QUICK_SHARED_INODE_PASS
BUILDSTORM_FS_QUICK_PASS
FS_QUICK_ELAPSED_S 44.30
block-cache: dirty_evictions=0 threshold_flush_blocks=34816
block-cache: explicit_flush_blocks=5280 replacement_scans=73072
block-cache: clean_replacements=36153 writeback_runs=1403
block-device: writes=2475 bytes=153579520 avg=62052 write_dma_us=1757420
Fs lock: acquisitions=9014 wait_total_us=115849358
```

| 指标 | 修改前 | 修改后 | 变化 |
| --- | ---: | ---: | ---: |
| `FS_QUICK` guest 耗时 | 71.61 s | 44.30 s | -38.14% |
| 脏块冲突淘汰 | 14095 | 0 | -100% |
| VirtIO 写请求 | 16926 | 2475 | -85.38% |
| 平均单次写入 | 8977 B | 62052 B | 6.91 倍 |
| VirtIO 写 DMA 累计时间 | 12818809 us | 1757420 us | -86.29% |
| 文件系统锁累计等待 | 138109226 us | 115849358 us | -16.12% |
| 设备写入总字节 | 151945216 B | 153579520 B | +1.08% |

阈值写回块数从 `19928` 增至 `34816`，说明降到低水位后部分块在后续事务中又被修改，
但设备总写入字节只增加 1.08%。与此同时写请求减少 85.38%、平均请求扩大到 6.91 倍，
最终耗时下降 38.14%。因此本轮收益来自消除冲突淘汰和形成大块连续 I/O，而不是省略
持久化、减少测试工作或伪造时间。

### 19.5 并发、一致性与验证边界

改后 RV 内核继续通过以下动态回归：

```text
NAMESPACE_STRESS_PASS operations=640 elapsed_ms=5547
NAMESPACE_PERSIST_PASS
RENAME_SEMANTICS_PASS
PARTIAL_DIRTY_PASS files=512 elapsed_ms=758
```

命名空间压力测试由 8 个 pthread 并行完成 640 组创建、写入、重命名、读取和删除；随后
对 1 MiB 文件执行 `fsync`，卸载并重新挂载测试盘，逐页逐字节校验成功。rename 回归覆盖
替换已有目标、替换后旧 fd 继续访问原 inode、硬链接别名、目录替换及
`RENAME_NOREPLACE`。部分脏读回归覆盖 512 个文件的“小范围覆盖写后整文件读取”，确保
上一轮 inode 脏区间覆盖与本轮块缓存叠加后仍保持一致。

`cargo check -p os --target riscv64gc-unknown-none-elf --features=lock-stats`、RV release
构建和 `git diff --check` 均通过。当前主机仍缺少 `loongarch64-linux-gnu-gcc`，因此本轮
没有新 LA 动态内核可运行；同时这些短测不能代替官方题 2.2 的 RV/LA 完整构建。当前
结论严格限定为：块层根因已由计数器证实，RV 短负载获得显著改善，并已通过并发和
持久化回归。

## 20. 2026-08-11：文件页缓存顺序预读与反向实验

### 20.1 从 4 KiB 同步缺页改为 64 KiB 固定窗口

前一版文件页缓存只能在调用方请求的 4 KiB 页面缺失后读取该页。Rust 动态链接器、
`rustc` 和链接器会连续扫描 ELF、元数据及归档文件，这使一次顺序读取退化为大量 4 KiB
同步 VirtIO 请求，而且每个请求都发生在 lwext4 卷锁临界区内。修改后，普通文件读在
页缓存 miss 时以 64 KiB 对齐窗口读取并填充最多 16 个 4 KiB 页面；同一窗口中的后续读
直接命中页缓存。并发读取同一窗口仍通过 in-flight load 状态合并，失效操作会先改变
generation，再唤醒等待者，因而不会在 truncate、写入或 unlink 后发布旧数据。

块设备缓存也增加保守的顺序检测：只有连续两次对齐读取后才预取后续 64 KiB，并在遇到
已有缓存块、设备末尾或槽位不足时缩短/取消预取。随机元数据读取不会在第一次访问时扩张，
预取块仍参加正常 CLOCK 淘汰，不具有不可回收特权。该设计对应 Linux 的按文件预读与
请求合并思想，不依据进程名、文件名或评测目录。

同一 RV、8 核、同镜像 `FS_QUICK` 的前后结果如下：

| 指标 | 修改前 | 64 KiB 固定预读 | 变化 |
| --- | ---: | ---: | ---: |
| guest 总耗时 | 56.47 s | 39.07 s | -30.81% |
| VirtIO 读请求 | 15115 | 3465 | -77.08% |
| VirtIO 读 DMA 累计时间 | 14.61 s | 5.32 s | -63.59% |

测试仍逐项输出 `FS_QUICK_INODE_PASS`、`FS_QUICK_TRUNCATE_PASS`、
`FS_QUICK_SPARSE_PASS`、`FS_QUICK_SHARED_INODE_PASS` 和
`BUILDSTORM_FS_QUICK_PASS`。读请求下降幅度与 DMA 时间、guest 时间方向一致，因此保留
固定 64 KiB 方案。

### 20.2 未保留的自适应窗口和扩大固定块缓存

曾尝试把连续预读窗口从 64 KiB 自适应放大到 128 KiB。该方案会在短 ELF、离散元数据
和交错扫描中带入更多未使用页面，且需要更大的临时缓冲；短测没有形成可重复的总耗时
下降，所以恢复为固定 64 KiB。这里保留“64 KiB -> 128 KiB -> 64 KiB”的实验结论，
不把没有稳定 A/B 的候选写成性能收益。

还单独把每卷块缓存从 4096 个槽扩大到 8192 个槽。在该次对照负载中，4096 槽基线的
实际脏页峰值只有 368，容量并不是工作集瓶颈；扩大后 guest 耗时从 28.28 秒增至
32.34 秒，读 DMA 从 4.09 秒增至 4.40 秒。额外容量增加了初始化、扫描和缓存管理成本，
却没有减少必要 I/O，因此恢复为 4096 槽。最终代码按“消除冲突映射、改善淘汰和合并”，
而不是“无依据扩大数组”的方向优化。

## 21. 2026-08-11：按父目录 inode 的原子命名空间操作

### 21.1 根因与接口边界

Rust VFS 已经完成父目录和叶名解析，但旧 lwext4 接口只接受完整路径。一次 create、remove
或 rename 会因此在持有整卷锁后再次从挂载根遍历相同目录；rename 还会分别打开源、目标
和隐藏替换路径。并发编译大量生成 `.rmeta`、对象文件和临时文件时，这些重复遍历既消耗
CPU，也把所有等待者阻塞在包含多次目录查询的长临界区中。

本轮在 lwext4 增加三个通用 inode 定向接口：

```text
ext4_create_at_with_attrs(parent_inode, leaf, ...)
ext4_remove_at(parent_inode, expected_child_inode, leaf, ...)
ext4_rename_at2(source_parent_inode, expected_source_inode, source_leaf,
                target_parent_inode, target_leaf, hidden_leaf, flags, ...)
```

接口并不信任 Rust 缓存。它们取得挂载锁并开始 journal transaction 后重新读取父 inode，
验证父项仍是有链接的目录，再查询叶目录项并核对实际 inode 与 VFS 传入的 expected inode。
这相当于 Linux VFS 在锁定父目录后重新验证 dentry，避免缓存命中与真实目录变更之间的
TOCTOU。C 层只接受单一叶名，拒绝空名、`.`、`..`、斜杠和超长名字。

### 21.2 原子 create 与 remove

create 在同一事务内完成 inode 分配、uid/gid/mode 初始化、父目录链接和文件句柄返回，
Rust 层直接用返回的 inode 信息发布 dentry 和句柄模板，不再执行创建后的完整路径
`stat + open`。同一 `FS_QUICK` 的逐步 A/B 中，create 调用点累计持锁时间从
4255966 us 降到 2956693 us（-30.53%），该调用点等待时间从 38822844 us 降到
28513691 us（-26.56%）；全文件系统锁等待从 91856109 us 降到 64447359 us
（-29.84%）。

remove 在事务内重新确认叶项仍指向目标 inode，并严格区分文件和目录。目录只有空目录
才能删除，类型不符分别返回 `EISDIR`/`ENOTDIR`；最终普通文件链接删除前执行必要截断和
inode 回收。Rust 层不再把所有 lwext4 错误压成 `EPERM`，而是把 `ENOENT`、`ENOTEMPTY`、
`EISDIR`、`ENOTDIR` 等原始 errno 返回给用户态。短测中 remove 的总时间没有得到可归因的
稳定下降，因此本项保留理由是减少重复路径解析并补齐 Linux 错误语义，不宣称单独的
wall-clock 加速。

### 21.3 原子 rename 与替换后打开文件语义

`ext4_rename_at2` 在一个挂载锁和一个 journal transaction 内重新验证源、目标及隐藏名。
目标存在时先检查 `RENAME_NOREPLACE`、文件/目录类型和空目录条件，再把旧目标链接到隐藏
名、移除旧目标项、链接源到新目标、移除旧源项。被覆盖 inode 及类型由 C 层原子返回，
Rust VFS 据此维持打开 fd 和硬链接可见性；目录环检测仍在 VFS 层沿目标祖先执行。任一
中间步骤失败都会 abort transaction，不会把只完成一半的命名空间发布到 dentry 缓存。

使用完全相同的镜像、8 GiB 内存、8 个 hart 和 `lock-stats` 内核做紧邻 A/B：

| 指标 | 完整路径 rename | inode 定向 rename | 变化 |
| --- | ---: | ---: | ---: |
| rename 持锁累计时间 | 5069162 us | 2903217 us | -42.73% |
| rename 等锁累计时间 | 47034366 us | 38275519 us | -18.62% |
| 全文件系统锁等待 | 125453196 us | 108184493 us | -13.76% |
| `FS_QUICK` guest 耗时 | 37.67 s | 37.62 s | -0.13% |

短测 wall-clock 基本不变，说明当前总时间还受同步块 I/O 和宿主抖动限制；但工作量相同的
rename 临界区缩短 42.73%，全卷累计等待减少 13.76%，属于能够在更高并发完整构建中扩展
的结构性收益，因此保留。不能把这组锁时间外推成官方完整构建加速比。

### 21.4 并发稳定性和错误暴露

早期候选曾有一轮在 640 次并发操作完成后 `rmdir` 失败，当时 Rust 层把底层错误统一映射
为 `EPERM`，无法判断真实原因。加入原始 errno 传播和目录诊断后，相同候选连续三轮输出：

```text
NAMESPACE_STRESS_PASS operations=640 elapsed_ms=8103
NAMESPACE_PERSIST_PASS
NAMESPACE_STRESS_PASS operations=640 elapsed_ms=7599
NAMESPACE_PERSIST_PASS
NAMESPACE_STRESS_PASS operations=640 elapsed_ms=8222
NAMESPACE_PERSIST_PASS
```

这三轮均由 8 个 pthread 并发创建、写入、rename、读取和 unlink，随后写入 1 MiB 文件、
`fsync`、卸载、重挂载并逐字节验证。原始失败尚未再次复现，所以不能把 errno 传播本身
描述成对竞态的确定修复；连续复测只能说明当前候选在该覆盖范围内稳定。rename 定向回归
另行输出 `RENAME_SEMANTICS_PASS`，覆盖打开目标替换、旧 fd、硬链接别名、
`RENAME_NOREPLACE`、目录替换和 `EISDIR`。

### 21.5 未保留的 128 KiB VirtIO bounce

块缓存最多合并 128 KiB，但 VirtIO 驱动使用 64 KiB bounce buffer，所以曾尝试把驱动
bounce 扩大到 128 KiB，使合并写以一个请求提交。完全相同的紧邻 `FS_QUICK` A/B 如下：

| 指标 | 64 KiB | 128 KiB | 变化 |
| --- | ---: | ---: | ---: |
| guest 总耗时 | 34.77 s | 38.03 s | +9.38% |
| VirtIO 写请求 | 2458 | 1323 | -46.18% |
| 写 DMA 累计时间 | 4948266 us | 3924847 us | -20.68% |
| bounce 写拷贝时间 | 297847 us | 819112 us | +175.01% |
| 文件系统锁等待 | 106249882 us | 104550040 us | -1.60% |

请求数和 DMA 时间虽下降，但更大的同步 bounce 拷贝和请求长尾抵消收益，总耗时明确退化，
因此恢复 64 KiB。后续若要继续降低请求数，应实现多队列或异步提交后再评估，而不是在
现有单同步队列上继续扩大缓冲。

### 21.6 当前验证边界

本轮 RV 候选通过带 `lock-stats` 的 release 构建、`cargo check`、完整 `FS_QUICK`、
rename 语义以及三轮连续命名空间持久化压力测试。当前主机仍缺少 LA C 交叉编译器，新增
lwext4 C ABI 尚未生成并动态验证 LA 静态库；官方 RV/LA 题 2.2 完整构建也尚未完成。
因此当前结论是“RV 短测的路径解析与锁等待得到明确改善”，不是“双架构 BuildStorm 已
拿分”。

## 22. 2026-08-11：文件型 exec 的匿名栈按需分配

### 22.1 根因与实现

旧实现为每次文件型 `execve` 立即分配、清零并建立完整 8 MiB 用户栈，即每个新进程固定
处理 2048 个物理页。真实构建会频繁启动 shell、build script、链接器和编译器；这些进程
启动时通常只需要容纳数页 `argv`、`envp` 和 `auxv`，未使用的绝大多数栈页仍会反复经过
帧分配器和页表锁。并发四个进程时，仅初始栈就可能瞬时占用 8192 页。

修改后，文件型 ELF 装载只建立 8 MiB 私有匿名栈的 VMA 元数据，不预先分配全部物理页。
在提交新地址空间前，内核按实际参数字符串、环境字符串、指针数组、16 字节随机数、平台
字符串、对齐填充和 18 项 auxv 精确计算初始栈需求，只预缺页并映射覆盖该范围的顶部页面。
进程随后向更低地址使用栈时，继续通过已有匿名写缺页路径逐页增长。栈顶、8 MiB 上限、
读写权限和每进程私有性均未改变；参数超过栈上限时装载失败，不会越界写入。

内嵌初始 `initproc` 的装载路径保持原状，本轮只修改来自文件系统的普通 `execve`，以缩小
回归范围。Debian glibc 环境中的 `cc --version`、`gcc --version` 和 `rustc --version`
均已在候选内核上正常返回，覆盖动态链接器、初始栈解析和 Rust 工具链线程启动。

### 22.2 同条件四路 rustc 对照

使用同一 RV 官方 BuildStorm 镜像、同一根盘、8 GiB、8 hart、同一份 libc `build.rs`，
并行启动四个完全相同的 `rustc`，每个进程超时均为 360 秒。前后结果如下：

| 指标 | 完整 8 MiB 预分配 | 按需栈 | 变化 |
| --- | ---: | ---: | ---: |
| guest 并发段耗时 | 389.36 s | 377.78 s | -2.97% |
| 帧分配器锁调用 | 167752 | 95366 | -43.15% |
| 页表锁调用 | 296928 | 220489 | -25.74% |
| VirtIO 读取总量 | 150908928 B | 152879104 B | +1.31% |

两轮在当前 Mac 上都触发 360 秒单进程超时，所以不能描述为“本地四路编译已经完成”；但
内存管理工作量下降明确，且读取数据量没有被减少，2.97% wall-clock 收益不是跳过输入所得。
宿主观测显示 Docker 中运行的是经 Rosetta 转译的 x86_64 QEMU，8 个 vCPU 线程合计长期
只有约 1.1 至 2 个宿主核心的吞吐。各 `rustc` 的约 2 万次 10 ms 抢占累计 CPU 时间与该
宿主并行度吻合，因此本地四路超时不能直接外推到官方原生 x86、8 物理核评测机。

这项优化保留的主要依据是帧分配和页表操作分别减少 43.15% 与 25.74%，它会降低完整构建
中频繁进程启动造成的内存峰值及锁竞争。下一步将继续把文件型 ELF 的完整 `PT_LOAD`
预读改为带 BSS 边界保护的按需文件映射，避免启动大型 `cargo`/链接器时读取从未执行或
访问的代码页。

## 23. 2026-08-11：ELF `PT_LOAD` 文件页按需装入

### 23.1 根因与 Linux 语义

旧的文件型 `execve` 会在切换地址空间前读取、分配并映射 ELF 主程序和动态解释器的每个
`PT_LOAD` 页面。`cargo` 本体约 43 MiB，而一次 `--version` 实际只访问其中一部分代码和
只读数据；旧实现仍读取整个文件映射段，并为全部页面获取物理帧和页表项。真实构建频繁
启动 `cargo`、`rustc`、链接器和 build script，这部分冷页开销会随进程数重复出现。

现在装载器先按 ELF program header 建立文件后备 VMA，完整文件页在第一次读、写或执行
缺页时才装入。这与 Linux 的 demand paging 方向一致，不依据可执行文件名或评测命令。
只读/可执行页继续进入全局文件页缓存，不同进程可以共享相同物理页；私有可写段在缺页时
分配独立页面，fork 后仍沿用现有 COW 路径，因此不会把一个进程的数据段写入另一个进程。
VMA 持有文件对象的 `Arc`，即使 `execve` 返回并关闭原 fd，后续缺页仍有稳定的 inode 和
文件生命周期。

### 23.2 非页对齐段与 BSS 边界

ELF 段并不保证文件数据尾部与虚拟内存尾部同时页对齐。为保持 Linux ELF 装载语义，本轮
没有简单地把整个向上取整区间直接映射到文件：

1. 起始地址非页对齐时，首个页面立即分配，只把文件字节复制到正确的页内偏移，其余字节
   保持为零。
2. 中间完整的 4 KiB 文件页建立按需文件映射。
3. 文件数据最后不足一页时，该尾页立即分配并复制有效字节，页内剩余部分清零，避免把
   文件中属于下一段或其他内容的字节暴露为 BSS。
4. `p_memsz` 大于 `p_filesz` 的后续整页建立匿名按需映射，第一次访问得到零页。
5. 所有长度、偏移和向上取整都使用 checked arithmetic；`p_filesz > p_memsz`、地址回绕
   或区域重叠时装载失败，而不是建立不完整地址空间。

动态解释器使用同一条装载路径。原先内嵌在内核镜像中的 `initproc` 仍走字节切片装载路径，
本轮没有改变其启动边界。

### 23.3 `cargo --version` 三轮交替 A/B

为避免把 Rosetta/QEMU 的单轮抖动误判为收益，使用同一 RV 镜像、8 GiB、8 hart 和同一
`lock-stats` 构建，按“基线、候选、基线、候选”交替运行；再计入此前各自的一轮，共三轮。
测试只执行真实 `cargo --version`，随后立即关机。结果如下：

```text
完整 ELF 基线：9.18 s, 8.78 s, 9.07 s
按需 ELF 候选：6.57 s, 6.36 s, 6.02 s
```

| 指标 | 完整 ELF 基线 | 按需 ELF | 变化 |
| --- | ---: | ---: | ---: |
| guest 耗时中位数 | 9.07 s | 6.36 s | -29.88% |
| 块设备读取字节 | 46477312 B | 23736320 B | -48.93% |
| 帧分配器锁调用 | 约 34833 | 约 14322 | -58.88% |
| 页表锁调用 | 约 27483 | 约 15433 | -43.84% |

三轮候选均输出真实 Cargo 版本并以状态 0 结束。读取减少来自未访问 ELF 页不再预读；测试
执行的命令、二进制和验证条件均未改变。

### 23.4 完整题 2.1 A/B

随后使用完全相同的官方 RV BuildStorm 镜像、根盘、`-smp 8 -m 8G` 和最小工程流程运行
严格 A/B。流程包含 `rustc --version`、`cargo --version`、`cargo new`、真实
`cargo build -vv`、链接以及执行生成的 Hello World，不是只验证进程能启动。

| 指标 | 完整 ELF 基线 | 按需 ELF | 变化 |
| --- | ---: | ---: | ---: |
| `cargo build` 区间 | 111.24 s | 102.16 s | -8.16% |
| 启动至完成标记的 guest uptime | 164.57 s | 142.32 s | -13.52% |

两轮都输出 `BUILDSTORM_TOOLCHAIN ok` 和 `BUILDSTORM_MINIBUILD ok`，候选生成的程序实际
输出 `Hello, world!`。启动阶段收益大于编译阶段收益，符合“减少大 ELF 冷启动读取和映射”
的预期；编译阶段仍有多个工具进程启动，所以也得到 8.16% 的整体改善。

### 23.5 回归与当前边界

候选继续通过 RV `FS_QUICK` 全部文件语义检查，输出
`BUILDSTORM_FS_QUICK_PASS`，本轮耗时 36.12 秒。8 线程命名空间压力测试完成 640 组
并发创建、写入、rename、读取和 unlink，输出
`NAMESPACE_STRESS_PASS operations=640 elapsed_ms=8467`；随后卸载、重挂载并逐字节验证
1 MiB 文件，输出 `NAMESPACE_PERSIST_PASS`。这些结果覆盖了动态装载、私有可写数据、
pthread、文件系统并发和进程退出后的持久化。

本地 Docker 中的 x86_64 QEMU 仍经 Rosetta 运行，wall-clock 不能直接外推到官方原生
x86 评测机；本机也仍缺少 LoongArch C 交叉编译器，所以本节尚无 LA 动态结果。按需 ELF
显著降低了工具启动成本，但完整 2.2 仍同时受 rustc CPU、全卷 lwext4 串行区和同步
VirtIO I/O 限制，不能据此宣称双架构完整构建已经达到官方时限。

## 24. 2026-08-11：未保留的文件缺页聚簇映射

按需 ELF 保留后，又实验了类似 Linux `fault-around` 的聚簇映射：一次文件缺页完成 64 KiB
预读后，把同一只读 VMA、同一窗口中已经存在于页缓存的相邻页一并装入当前进程页表。
候选不会额外读取文件，也不处理私有可写段；映射前会二次核对 VMA 范围、权限、文件对象、
文件偏移和 resident-frame 集合身份，以避免与并发 `munmap` 或 `mprotect` 混淆。

同一 `cargo --version` 微基准中，候选耗时 6.20 秒，处于未聚簇按需 ELF 三轮
6.02 至 6.57 秒的正常波动内；读取仍为 23736320 B，帧分配仍约 14.3k，没有减少实际
I/O 或物理页工作量。候选提前映射了 6207 个相邻页，页表锁调用却从约 15433 增至
25189（+63.21%），说明预读窗口内有大量页面最终没有被该进程访问。由于 wall-clock
没有可重复收益且页表工作量明显上升，该实验已完整回退；最终保留逐个实际缺页映射，
不把预读命中等同于访问预测。

## 25. 2026-08-11：未保留的私有可写文件页初始共享

在保留按需 ELF 后，本轮实验了更接近 Linux 私有文件映射的初始页共享：私有可写
`PT_LOAD` 页面第一次缺页时也进入全局文件页缓存；只要缓存或其他进程仍持有该物理页，
页表就去掉写权限，第一次写缺页再沿已有 COW 路径复制。可写 `MAP_SHARED` 映射不进入这条
路径，避免改变进程间共享写语义。该实现不识别 `rustc`、`cargo` 或任何评测文件名。

先以同一 RV 镜像、8 hart 和相邻运行的 `cargo --version` 比较：

| 指标 | 只缓存只读页 | 同时缓存私有可写页 | 变化 |
| --- | ---: | ---: | ---: |
| guest 耗时 | 6.38 s | 6.71 s | +5.17% |
| 块设备读取字节 | 23736320 B | 23801856 B | +0.28% |
| 帧分配器锁调用 | 14305 | 13480 | -5.77% |
| 页表锁调用 | 15413 | 18290 | +18.67% |

物理页分配虽然减少，但多数可写数据页很快发生 COW，新增写缺页和页表修改抵消了共享收益。
为确认并发进程是否会改变结论，又使用同一份 libc `build.rs`、8 GiB 和 8 hart 同时启动
四个 `rustc`，每个进程使用相同的 120 秒上限：

| 指标 | 只缓存只读页 | 同时缓存私有可写页 | 变化 |
| --- | ---: | ---: | ---: |
| guest 并发段耗时 | 125.86 s | 127.71 s | +1.47% |
| 120 秒内完成数 | 0/4 | 0/4 | 无改善 |
| 块设备读取字节 | 91860992 B | 91074560 B | -0.86% |
| 文件页缓存驻留页 | 22229 | 22021 | -0.94% |

第二组实验只减少不足 1% 的读取和驻留页，没有增加完成数，耗时反而略增。当前 Mac 上的
x86_64 QEMU 受 Rosetta CPU 吞吐限制，所以不能把 1.47% 单次波动解释成精确退化；但两组
测试都没有显示可重复收益，且短测明确增加了 18.67% 的页表操作。该实验已完整回退，最终
继续只共享只读/可执行文件页，私有可写文件页在首次访问时直接成为进程私有页。

## 26. 2026-08-11：无本地竞争时跳过周期性任务切换

### 26.1 根因与实现

旧的时钟中断路径每经过一个用户时间片就无条件挂起当前任务，即使当前 CPU 的本地就绪
队列为空，也会完整保存用户浮点/向量状态、切回内核页表、获取调度器和任务锁、把当前任务
重新入队，然后立即把同一个任务取出并恢复。编译器这类长时间运行且经常独占一个 CPU 的
任务因此每 10 ms 支付一次没有产生公平性收益的上下文切换成本。

本轮参考 Linux `need_resched` 的方向，在时间片到期时只在确实需要重新调度时切换：当前
任务的 CPU 亲和性已经排除本 CPU 时必须切换；本地就绪队列存在竞争任务时必须切换；读取
调度器状态的非阻塞锁获取失败时也保守地切换。只有亲和性仍合法、调度器状态读取成功且
本地没有竞争者时，才让当前任务继续运行。主动 `yield`、阻塞、退出、网络和定时器唤醒
路径均未改变，10 ms 时间片长度也未修改，因此这不是针对 BuildStorm 调参或跳过工作。

### 26.2 微基准与四路编译压力对照

使用同一 RV BuildStorm 镜像、8 GiB、8 hart 和相邻构建的带 `lock-stats` 内核进行 A/B。
单次 `cargo --version` 的 guest 耗时从 6.38 秒变为 6.41 秒，属于本机波动范围；但该进程
的周期性抢占次数从 216 次降至 16 次，说明无效切换确实被消除。

随后并行启动四个相同的 `rustc`，每个进程上限为 120 秒：

| 指标 | 无条件周期切换 | 按需周期切换 | 变化 |
| --- | ---: | ---: | ---: |
| guest 并发段耗时 | 125.86 s | 126.11 s | +0.20% |
| 120 秒内完成数 | 0/4 | 0/4 | 无变化 |
| 调度器锁调用 | 235213 | 169668 | -27.87% |
| 调度器锁累计等待 | 8140404 us | 7447959 us | -8.51% |
| VirtIO 读取总量 | 91860992 B | 92057600 B | +0.21% |
| 跳过的无竞争周期切换 | 0 | 32761 | - |

当前 Mac 上的 x86_64 QEMU 经 Rosetta 运行，四个进程都因宿主 CPU 吞吐在 120 秒触发上限，
所以 0.20% 的 wall-clock 差异不能解释为性能退化或提升。可确认的收益是调度器锁调用减少
27.87%，且输入读取量和执行完成条件没有被削减。

### 26.3 公平性、SMP 与文件系统回归

为了避免“独占运行”误伤同核公平性，额外把两个不主动 `yield` 的计算循环同时固定到
CPU 0。两个进程均以状态 0 完成，总耗时 27.20 秒，分别发生 62 次和 1123 次定时抢占，
证明本地就绪队列有竞争者时仍会进行时间片轮转。8 核 SMP 回归中，`cpuinfotest` 观测到
`0xff` 在线掩码，`clonesmptest` 的父进程位于 CPU 0、子进程在 CPU 1，`smpstress`
也观测到全部 8 个 CPU，三项均通过且无 panic、死锁或饥饿。

同一候选继续通过完整 RV `FS_QUICK`，输出 `BUILDSTORM_FS_QUICK_PASS`；本轮耗时 34.81 秒，
相对按需 ELF 候选此前的单轮 36.12 秒缩短 3.63%。这是单轮结果，只用于确认没有明显退化，
不把它作为稳定的文件系统加速结论。

### 26.4 完整题 2.1 复验与保留结论

使用官方 RV BuildStorm 根盘执行 `rustc --version`、`cargo --version`、`cargo new`、真实
`cargo build`、链接并运行 Hello World。候选输出 `BUILDSTORM_TOOLCHAIN ok` 和
`BUILDSTORM_MINIBUILD ok`；编译区间从 guest uptime 38.37 秒到 133.39 秒，共 95.02 秒。
此前按需 ELF 候选的最佳独立运行是 94.99 秒，两者仅差 0.03 秒，不能宣称 2.1 wall-clock
得到可重复提升。完整流程中共跳过 21920 次无竞争周期切换，并正常关机。

这项修改最终保留，因为它在不改变时间片、公平性和工作量的前提下显著减少调度路径工作，
且通过了同核竞争、跨核迁移、8 核压力、文件系统和完整 2.1 回归。它解决的是长运行任务的
无效切换，不足以替代对 2.2 文件系统并发写入和 lwext4 全卷串行区的后续优化。当前主机
仍缺少 LoongArch C 交叉编译器，因此本节尚无 LA 动态回归结果。

## 27. 2026-08-11：未保留的目录删除查询结果复用

### 27.1 实验目的与语义边界

目录删除旧路径会先查询叶目录项以完成类型和权限检查，进入 lwext4 事务后又查询一次同一
目录项。候选尝试把第一次查询得到的 inode 和类型传入删除事务，并在事务锁内重新核对父
目录和叶项仍然匹配 expected inode。重新核对不能省略，否则并发 rename/unlink 会使缓存
结果变成 TOCTOU；候选没有依据文件名或测试内容跳过检查。

候选通过文件删除、空目录删除、非空目录 `ENOTEMPTY`、文件/目录类型不匹配以及 8 线程
并发命名空间语义检查，局部 unlink 调用点的重复查询确实减少。但相同镜像、8 hart 和同一
`FS_QUICK` 负载的紧邻两轮结果如下：

| 轮次 | 原路径 | 查询结果复用 | 变化 |
| --- | ---: | ---: | ---: |
| 第一轮 | 34.86 s | 38.05 s | +9.15% |
| 第二轮 | 36.84 s | 39.04 s | +5.97% |
| 两轮均值 | 35.85 s | 38.55 s | +7.52% |

局部工作减少没有转化为整体收益，候选两轮均稳定退化。推测原因是该路径相对同步 journal、
块写回和目录缓存失效占比很小，新增的结果传递和事务内核对不能抵消其他长尾。由于没有
可重复 wall-clock 收益，本实验已完整回退；最终代码继续在事务内独立查询并验证目录项。

## 28. 2026-08-11：每核小对象缓存与动态内核堆分片

### 28.1 隐藏的全局内核堆瓶颈

此前锁统计覆盖调度器、VFS、页表和帧分配器，却没有覆盖 Rust 全局 allocator。内核所有
`Arc`、`Vec`、VMA、任务和 dentry 元数据最终都进入同一个
`MutexNoIrq<buddy_system_allocator::Heap>`。在同一 RV 官方镜像、8 GiB、8 hart、四个
并行 `rustc`、每进程 120 秒的基线中，内核执行 87913 次分配和 80464 次释放，全局堆锁
获取 168379 次，其中 18366 次发生竞争；累计等待 13134521 us，最长一次 89079 us。
四个编译进程均在宿主 Rosetta 吞吐限制下触发 120 秒上限，并发段为 126.95 秒。

本轮首先在 `lock-stats` 特性下加入 allocator 调用、锁获取、竞争、累计/最长等待、缓存命中
和回收统计。统计代码不改变非诊断构建的分配语义，也不使用系统时间伪造 guest uptime。

### 28.2 每核规格化小对象缓存

第一阶段为 8 B 至 4 KiB 的 10 个二次幂 size class 建立每 CPU 空闲链表。释放对象的首个
机器字保存下一节点地址，每类每核最多保留 64 个对象；命中只获取本核关中断锁，不进入
全局 buddy。缓存满时对象按原规格化 layout 返回 buddy。分配与释放在调度器建立前后都使用
同一个 `max(size, align).next_power_of_two()` layout，避免启动阶段分配、运行阶段释放时以
不同规格回收。对象可以由另一个 CPU 释放并进入释放者缓存，所有权不会依赖任务亲和性。

相同四路 `rustc` 对照如下：

| 指标 | 单一全局 buddy | 每核小对象缓存 | 变化 |
| --- | ---: | ---: | ---: |
| guest 并发段 | 126.95 s | 125.54 s | -1.11% |
| buddy 锁获取 | 168379 | 33448 | -80.14% |
| buddy 锁竞争 | 18366 | 8672 | -52.79% |
| buddy 累计等待 | 13134521 us | 5078594 us | -61.33% |
| buddy 最长等待 | 89079 us | 47770 us | -46.37% |

缓存命中 66652 次、未命中 19592 次、缓存回收 68288 次、满缓存回退 10867 次。输入读取量
仍为约 92.8 MiB，四个进程仍执行真实 rustc，没有靠减少工作获得结果。

### 28.3 未保留的批量补充与批量回收

随后实验类似 slab magazine 的批处理。`8` 个对象补充、`32` 个对象回收时，buddy 锁获取
降到 6162 次、竞争降到 394 次，但一次锁内连续执行多个 buddy 拆分/合并，累计等待反而升
到 7328440 us，并发段为 126.35 秒。缩短为 `2` 个补充、`8` 个回收后，锁获取 14104 次、
竞争 1185 次，累计等待仍为 7487191 us，并发段进一步变为 127.19 秒。

两种批次都没有减少编译输入或改变超时条件，却都不如逐对象缓存。原因是当前 buddy 的
dealloc 会遍历并合并空闲链，批量循环显著延长关中断临界区；锁次数下降不能代表锁成本
下降。这两个候选均已回退，最终不保留批量循环，也不通过继续调批次掩盖架构问题。

### 28.4 按地址归还的动态堆分片

最终方案参考现有物理帧分配器和 Linux per-CPU allocator 的分层方向。固定 64 MiB
`HEAP_SPACE` 继续承担启动阶段分配和全局兜底；启动时从物理内存保留的动态增长区按平台
CPU 数切为独立 buddy shard。运行期优先从当前 CPU 对应 shard 分配，局部 shard 耗尽时才
按顺序尝试其他 shard，全部耗尽后回退启动堆。

每段动态堆都登记稳定的直映虚拟地址区间。释放时根据指针地址找到原始 shard，而不是按
当前 CPU 归还，所以任务迁移、跨核析构和其他 CPU 释放都不会导致一个 buddy 的统计在另
一个 buddy 下溢，也不会让两个 allocator 同时管理重叠内存。每个分片只合并自己地址区间
中的 buddy；小对象缓存溢出后同样按地址回到原分片。分片范围在次级 hart 进入调度器前
完成发布，运行期只做 acquire 读取。

### 28.5 文件系统与虚拟内存压力 A/B

先使用同一 2 GiB 镜像和 `FS_QUICK` 比较每核缓存与“每核缓存 + 动态堆分片”。测试包含
8 线程并发创建、写入、rename、读取、unlink，960 个产物 inode，截断、稀疏文件和同 inode
并发写语义：

| 指标 | 每核缓存 | 再加动态堆分片 | 变化 |
| --- | ---: | ---: | ---: |
| `FS_QUICK` guest 耗时 | 35.45 s | 32.39 s | -8.63% |
| allocator 调用 | 447911 | 449107 | +0.27% |
| buddy 锁获取 | 165461 | 165566 | +0.06% |
| buddy 锁竞争 | 670 | 1 | -99.85% |
| buddy 累计等待 | 198122 us | 956 us | -99.52% |

两组完成的工作量相同，候选输出 `FS_QUICK_INODE_PASS files=960`、
`FS_QUICK_TRUNCATE_PASS`、`FS_QUICK_SPARSE_PASS`、`FS_QUICK_SHARED_INODE_PASS` 和
`BUILDSTORM_FS_QUICK_PASS`。收益来自原本同一把锁上的并行元数据分配，不是删减文件操作。

再运行 8 线程 48 轮 `mmap/mprotect/madvise/munmap`、fork COW，以及 4 轮每轮 8 个子进程
各自建立 32 MiB 地址空间后并行退出的压力测试：

| 指标 | 每核缓存 | 再加动态堆分片 | 变化 |
| --- | ---: | ---: | ---: |
| 普通 VM 压力段 | 25.53 s | 26.04 s | +1.97% |
| 1 GiB 并行退出段 | 30.98 s | 8.18 s | -73.60% |
| buddy 锁竞争 | 41056 | 1060 | -97.42% |
| buddy 累计等待 | 145893205 us | 161926 us | -99.89% |
| buddy 最长等待 | 250351 us | 19798 us | -92.09% |

候选输出 `FORK_COW_PASS`、`PARALLEL_EXIT_PASS` 和 `VM_LOCK_STRESS_PASS`。普通 VMA 段的
1.97% 单轮差异属于本机波动范围，不宣称该段加速；并行退出缩短 73.60%，与锁等待下降
99.89% 同时出现，说明分片确实消除了释放高峰的全局串行点。

### 28.6 编译负载与完整 2.1

四路 `rustc` 的分片候选连续两轮并发段为 124.43 秒和 124.38 秒，相对每核缓存的
125.54 秒缩短约 0.9%，相对单一 buddy 的 126.95 秒缩短约 2.0%。第二轮中 buddy 锁获取
32961 次，与每核缓存的 33448 次基本相同；竞争从 8672 次降到 101 次（-98.84%），累计
等待从 5078594 us 降到 63170 us（-98.76%），最长等待从 47770 us 降到 15112 us
（-68.37%）。动态 shard 完成 21063 次分配，启动堆只回退 2 次。锁调用数没有被删减而
等待显著下降，说明结果来自锁域分离。三组都因当前 Mac 上经 Rosetta 运行的 QEMU 吞吐在
120 秒触发上限，因此这组 wall-clock 只作为压力边界，不外推为官方原生评测机加速比。

完整 RV 题 2.1 中，`rustc --version`、`cargo --version`、`cargo new`、真实
`cargo build -vv`、链接和 Hello World 执行全部通过，输出 `BUILDSTORM_TOOLCHAIN ok` 与
`BUILDSTORM_MINIBUILD ok`。本轮编译区间为 guest uptime 37.89 秒到 135.75 秒，即
97.86 秒；此前同类候选为 95.02 秒，本轮慢 2.99%，不能宣称 2.1 wall-clock 提升。最终
保留分片的依据是并发 FS 和退出压力中的结构性收益，以及完整功能回归通过。

当前主机缺少 LoongArch C 交叉编译器，尚不能为本轮包含 lwext4 本地 C 改动的完整工作树
生成并动态验证 LA 内核。因此本节结论仍限定为 RV；在 LA 工具链补齐前，不宣称双架构
2.1 或 2.2 已由本地验证通过。

### 28.7 本轮逐项验收矩阵

为保证每个小优化都有独立证据，而不是只报告最终整机数字，本轮候选按同一 RV 官方镜像、
8 hart 和真实用户态负载逐项记录如下。负数表示耗时或等待下降；“回退”表示代码未进入当前
工作树。

| 小改动 | 验证负载 | 修改前 | 修改后 | 变化 | 结论 |
| --- | --- | ---: | ---: | ---: | --- |
| 每核 8 B--4 KiB 小对象缓存 | 四路并行 `rustc` | 126.95 s | 125.54 s | -1.11% | 保留；buddy 锁获取同时下降 80.14% |
| 一次补充 8 个、满 32 个后批量回收 | 四路并行 `rustc` | 125.54 s | 126.35 s | +0.65% | 回退；批量 buddy 操作延长关中断临界区 |
| 一次补充 2 个、满 8 个后批量回收 | 四路并行 `rustc` | 125.54 s | 127.19 s | +1.31% | 回退；累计等待仍上升到 7487191 us |
| 动态堆按 CPU 分片 | `FS_QUICK` | 35.45 s | 32.39 s | -8.63% | 保留；所有文件语义标记通过 |
| 动态堆按 CPU 分片 | 1 GiB 并行退出压力 | 30.98 s | 8.18 s | -73.60% | 保留；buddy 累计等待下降 99.89% |
| 动态堆按 CPU 分片 | 四路并行 `rustc` | 125.54 s | 124.38 s | -0.92% | 保留；锁竞争下降 98.84%，不把单轮 wall-clock 外推到评测机 |
| 动态堆按 CPU 分片 | 完整 RV 题 2.1 | 95.02 s | 97.86 s | +2.99% | 功能通过但无耗时收益声明；保留依据为并发压力下的结构性改进 |

当前保留方案没有改变 BuildStorm 的构建工作量、Cargo 并发数、QEMU 核数、guest 时钟或评分
标记。每项优化都先通过对应语义测试，再依据紧邻 A/B 决定保留或回退；未获得稳定收益的
批量缓存候选没有留在源码中。

## 29. 2026-08-11：完整题 2.2 首轮故障与大内存元数据分区

### 29.1 信号唤醒与 Ready--Running 交接竞态

在保留每核小对象缓存和动态堆分片后，使用官方 RV 数据盘、根盘、VirtIO 网卡以及
`-smp 8 -m 8G` 从新建 qcow2 覆盖层执行完整链路。工具链和 minibuild 均通过，但
`cargo build -p tg-xtask` 刚进入 `0/446` 时触发
`pid 146 was enqueued twice` 断言。

信号发送路径原先在 TaskLock 内读取目标为 `Ready`，释放 TaskLock 后再进入
`add_priority_task`。在这两个动作之间，另一个 CPU 可以从就绪队列取走目标并把它变为
`Running`。旧 `add_priority_task` 只检查 `enqueued` 标志；目标被取走后该标志已经清零，
于是发送者把仍在其他 CPU 运行的上下文再次发布到就绪队列。运行者随后正常结束时间片并
重新入队时，重复所有权断言就会触发。

修复后，`add_priority_task` 按既定 `SchedulerLock -> TaskLock` 顺序同时持有两把锁，在
调度器所有权稳定时重新检查目标状态。目标仍为 `Ready` 时只提升已经存在的队列项；若它
已经 `Running`，则不创建第二个队列引用，只向实际运行 CPU 请求重调度，使其尽快观察
pending signal。重复入队断言没有删除或降级，仍负责捕获任何新的所有权错误。

包含该修复的第二轮完整运行通过工具链和 minibuild，并从 `0/446` 推进到 `41/446`，原
竞态没有复现。这一结果证明已越过原确定故障点，但不能单独量化性能提升，也不等同于题
2.2 完整通过。

### 29.2 256 MiB 内核堆的确定性耗尽

第二轮在 `41/446` 编译 `futures-io` 等 crate 时由 lwext4 首先报告 `malloc failed`，
随后全局分配器打印：

```text
[heap-debug] user=218258372 actual=268367608 total=268435456
```

`actual` 与 256 MiB 总容量只差 67848 字节，多个 CPU 随后连 192 B、388 B 和 1024 B
分配都无法满足。这是 guest 内核元数据分区的确定性 OOM，不是 Cargo 无进度、测试超时或
Docker 宿主强制结束。原策略在 8 GiB guest 中仍只使用 64 MiB 静态堆和最多 192 MiB
动态堆；VFS 延迟写区间、dentry/inode 元数据、任务对象、页表元数据及 lwext4 的 C 分配
共同使用这个固定分区。Linux 的 slab/cache 可以在整机可用内存范围内按压力扩展，而原
实现即使还有数 GiB 空闲用户帧也不能把它们用于内核元数据。

### 29.3 分层容量策略与写回反向约束

新策略保持小内存机器的行为不变：可用内存低于 2 GiB 时仍只取 `1/32` 作为动态堆，
因此初赛 256 MiB 配置不会因本轮修改额外失去用户帧。可用内存不少于 2 GiB 时改为取
`1/8`，动态增长最高 960 MiB；加上固定 64 MiB 启动堆，官方 8 GiB 配置的内核堆总量为
1 GiB，仍给用户页和文件页缓存保留约 7 GiB。动态区继续按平台 CPU 数分为独立 buddy，
跨核释放仍根据指针地址回到原分片，没有引入共享无锁状态。

延迟写缓存原上限为内核堆的四分之一；如果只扩大堆，它会从 64 MiB 自动膨胀到 256 MiB，
反而消耗新增元数据余量。现在小堆仍使用四分之一且至少 16 MiB，大堆额外硬性封顶
64 MiB。这样 256 MiB 旧配置的有效上限保持 64 MiB，1 GiB 新配置也仍为 64 MiB；相对
“只扩堆不加约束”的 256 MiB 候选减少 192 MiB，即降低 75%。单 inode 的 8 MiB
回写阈值、`fsync`/`sync`/close/truncate 的同步语义和真实写入工作量均未改变。

### 29.4 本轮逐项验证状态

| 小改动 | 验证负载 | 修改前 | 修改后 | 变化 | 当前结论 |
| --- | --- | ---: | ---: | ---: | --- |
| 信号优先唤醒重新核对调度状态 | 完整 RV `tg-xtask` 预编译 | `0/446` 重复入队 panic | 越过原点并到达 `41/446` | 确定故障消失 | 保留；完整长测仍需继续 |
| 大内存动态堆分层 | RV，`-m 8G` 启动 | 256 MiB 总堆 | 1 GiB 总堆 | 容量 +768 MiB（+300%） | 保留候选；需以完整 2.2 验收 |
| 延迟写全局硬上限 | 1 GiB 堆的理论上限 | 256 MiB | 64 MiB | -192 MiB（-75%） | 保留；防止扩堆转化为写缓存膨胀 |

修复已通过 RV `cargo check`，官方 Docker 工具链也已成功生成 RV 与 LA release 内核。
最新 RV 内核启动时识别 8 个 hart、双 VirtIO 块设备和 VirtIO 网卡，并打印
`bootstrap=67108864 growth=1006632960 total=1073741824`；工具链及真实 minibuild 已
再次通过。完整 `tg-xtask` 和正式 `cargo xtask arceos build` 在本节记录时仍在运行，
最终标记、产物大小和耗时必须以该轮实际日志为准，尚不写成通过。

### 29.5 Docker 资源边界与当前完整样本

扩大 guest 内核堆后，Docker 样本从全新覆盖层通过工具链和 minibuild，越过旧 OOM 点并
推进到 `119/446`，期间无 guest panic、malloc failure 或调度断言。此时 Docker Desktop
虚拟机的 3918 MiB RAM 已使用约 3860 MiB，1023 MiB swap 已使用约 1011 MiB，QEMU RSS
约 3.3 GiB，五分钟只能完成约一个构建单元。该轮由开发者主动停止，原因是宿主分页抖动，
不是 guest 错误；因此它只能证明确定故障点已越过，不能作为完整通过或性能数据。

随后关闭已停止容器的 Docker Desktop，使用本机 QEMU 11.0.0 从另一份全新覆盖层运行完全
相同的 `-smp 8 -m 8G`、双盘、VirtIO 网卡和 Cargo 工作量。检查宿主进程时还发现六个前一
日遗留的 `scintillaos-rustc-micro-rv.*` QEMU，各自持续占用约 60%--134% CPU；精确核对
命令行后只终止这六个旧实例，当前完整样本未中断。清理前当前 QEMU 约使用 121% CPU，
清理后立即升到约 377%，说明前半程 wall-clock 被无关实验严重污染。

尽管存在宿主抢占，当前样本仍真实完成全部 446 个 `tg-xtask` 构建单元，并输出
`Finished dev profile ... in 262m 06s`。该数字不能用于宣称内核性能，也不能与 Linux
基线比较；它只证明预构建功能链路返回成功。脚本随后已进入计时的
`cargo xtask arceos build -p arceos-helloworld --arch riscv64`，正在从源码构建
`core`、`std`、`compiler_builtins` 和 ArceOS 组件。正式构建尚未输出最终返回码和产物大小，
LA 也尚未运行本轮全量，因此截至本次发布仍不能宣称双架构题 2.2 完整通过。
