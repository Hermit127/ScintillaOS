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
