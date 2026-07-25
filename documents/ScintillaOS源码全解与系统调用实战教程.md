# ScintillaOS 源码全解与系统调用实战教程

> 目标：从“能运行这个内核”走到“能解释它、能定位问题、能在决赛现场独立补系统调用”。
>
> 本文以 `main` 分支提交 `c4d1dfe`（2026-07-22）为基准。代码继续变化时，以源码为准，不要机械相信本文中的行号或旧文档。

## 0. 先说明“读完全部代码”的边界

这份教程覆盖了会决定 ScintillaOS 行为的全部一方代码和深度集成代码：

- 根目录的 Cargo、Makefile、链接脚本和构建配置；
- `os/src/` 下全部内核模块；
- `user/src/` 下用户库、系统调用封装、shell、runner 和测试程序；
- `lwext4_rust/src/` 下 Rust 绑定、块设备适配和 C 运行时胶水；
- `basic/` 下的 C 用户态运行库、系统调用封装和 basic 测例；
- 本项目通过 `[patch.crates-io]` 替换的 `vendor/polyhal`、`polyhal-boot`、`polyhal-trap`；
- 与运行直接相关的测试脚本和已有专题文档。

`vendor/` 中其余 Cargo 注册表依赖以及 `lwext4_rust/lwext4/` 中的通用第三方 C 库代码规模很大。它们按“依赖接口和本项目实际调用链”讲解，不把第三方库内部每个算法当作本内核自写设计。例如，理解 `smoltcp` 在本项目中的 socket 角色是必要的，但不需要背诵整个 `smoltcp` 的 TCP 状态机源码。

本文不是 API 清单，而是一条可以反复实践的学习路线。建议每读一章，就在源码中亲自沿调用链走一遍。

### 阅读导航

- 第 2～7 章：总架构、仓库、构建、启动、陷阱和同步规则；
- 第 8～13 章：内存、任务、信号、文件系统、网络和用户态；
- 第 14～16 章：系统调用全景、五类实现模板和端到端调用链；
- 第 17 章：从零重写本系统的阶段顺序；
- 第 18～20 章：调试、技术债和旧文档差异；
- 第 21～23 章：四周训练、决赛检查表和掌握标准。

## 1. 学完后应该达到什么程度

你应该能够不看答案完成下面的事情：

1. 从 QEMU 启动参数一路解释到第一个用户进程运行。
2. 画出一次 `read(fd, buf, len)` 从用户态到 ext4 或 pipe，再回到用户态的完整路径。
3. 解释 `fork`、`clone(CLONE_VM)`、`execve` 和 `wait4` 对 TCB、页表、文件描述符、父子关系、信号的影响。
4. 解释匿名 `mmap`、文件映射、延迟分配、COW 和缺页异常如何协作。
5. 判断一个用户指针是否能读或写，避免内核因坏指针崩溃。
6. 在任何可能阻塞的路径中，正确释放 `UPSafeCell` 借用并避免丢失唤醒。
7. 根据 Linux ABI 实现一个文件、进程、内存、信号或网络系统调用。
8. 为新系统调用写最小 C 测例，并完成 RISC-V、LoongArch 和双盘回归。
9. 遇到 `-ENOSYS`、`-EFAULT`、死锁、用户程序卡住或语义不兼容时，知道先查哪里。

## 2. 一张图看懂整个系统

```text
用户程序 / BusyBox / musl / glibc
        │
        │ ecall / syscall / svc
        ▼
polyhal-trap：保存寄存器，归一化 TrapType
        │
        ▼
os/src/main.rs::kernel_interrupt
        │
        ├─ SysCall ──> syscall/mod.rs 分发
        │                  ├─ syscall/fs.rs
        │                  ├─ syscall/process.rs
        │                  └─ net/syscall.rs
        │
        ├─ PageFault ──> mm/memory_set.rs
        ├─ Timer ──────> 调度、睡眠、itimer、网络轮询
        └─ Signal ─────> task/mod.rs 返回用户态前递送
                           │
        ┌──────────────────┼──────────────────┐
        ▼                  ▼                  ▼
 task/ 进程与调度       mm/ 地址空间        fs/ File + VFS
        │                  │                  │
        │                  │                  ├─ OSInode ─> lwext4
        │                  │                  ├─ Pipe
        │                  │                  ├─ proc/dev 伪文件
        │                  │                  └─ SocketFile
        └──────────────────┴──────────────────┘
                           │
                           ▼
              PolyHAL / VirtIO 块设备与网卡 / 串口
```

这个系统最重要的三个统一抽象是：

- `TaskControlBlock`：一个可调度执行实体的全部状态；
- `MemorySet`：一个用户地址空间及其 `MapArea` 元数据；
- `File` trait：普通文件、目录、pipe、stdio、设备文件和 socket 的统一 fd 接口。

理解这三个对象的所有权、共享方式和生命周期，就掌握了内核的大半。

## 3. 仓库地图：每类文件负责什么

### 3.1 根目录

| 路径 | 作用 |
| --- | --- |
| `Cargo.toml` | workspace 包含 `os`、`user`，并把三个 PolyHAL crate 指向本地补丁版本 |
| `Makefile` | 架构选择、用户程序、basic、根文件系统、内核、QEMU 和比赛产物的总入口 |
| `os/build.rs` | 按目标架构生成内核链接脚本 |
| `os/linker.lds` | 安排内核 text/rodata/data/bss 和加载地址 |
| `cargo_config/` | 各目标的 Cargo 编译配置 |
| `tmp/` | BusyBox、runner、LTP、lmbench、iozone 等测试材料 |
| `documents/` | 历史设计、迁移和测试记录；历史文档可能落后于当前源码 |

### 3.2 内核 `os/src`

启动与公共设施：

| 文件 | 主要责任 |
| --- | --- |
| `main.rs` | 内核初始化、统一陷阱处理、页分配接口 |
| `config.rs` | 4 KiB 页、1 MiB 用户栈、16 KiB 内核栈、192 MiB 内核堆 |
| `console.rs` | 通过 PolyHAL 调试串口输出 |
| `logging.rs` | `log` 后端和 64 KiB 内核日志环形缓冲 |
| `lang_items.rs` | panic 和分配失败处理 |
| `sync/up.rs` | 基于 `RefCell` 的单核内部可变性 `UPSafeCell` |

内存管理：

| 文件 | 主要责任 |
| --- | --- |
| `mm/frame_allocator.rs` | 伙伴系统物理帧分配、`FrameTracker` RAII、共享帧引用计数 |
| `mm/heap_allocator.rs` | 内核全局堆 |
| `mm/memory_set.rs` | ELF 装载、地址空间、`MapArea`、mmap、COW、缺页、共享页表 |
| `mm/page_table.rs` | 用户裸指针到引用/切片/字符串的直接转换 |
| `mm/vpn_range.rs` | 虚拟页范围及迭代器 |
| `mm/mod.rs` | 内存模块导出 |

任务与信号：

| 文件 | 主要责任 |
| --- | --- |
| `task/task.rs` | TCB、内核栈、创建、fork、共享 VM、exec、任务入口 |
| `task/processor.rs` | 当前 CPU 状态、idle 上下文和上下文切换 |
| `task/manager.rs` | FIFO 就绪队列和 PID 到 TCB 的索引 |
| `task/pid.rs` | PID 分配器及 `PidHandle` RAII |
| `task/mod.rs` | 让出、阻塞、唤醒、退出、initproc、信号递送和定时器队列 |
| `task/action.rs` | 用户信号动作表 |
| `task/signal.rs` | 信号位定义；当前 `MAX_SIG` 是 34 |
| `task/fp.rs` | 四种架构的浮点寄存器保存和恢复 |

文件系统：

| 文件 | 主要责任 |
| --- | --- |
| `fs/mod.rs` | `File` trait、`Kstat` 和文件类型位 |
| `fs/vfs.rs` | 超级块、Dentry、路径解析、挂载表、根文件系统、伪 tmpfs 配额 |
| `fs/inode.rs` | `OSInode`、open/create/link/unlink/rename、权限、时间和打开文件生命周期 |
| `fs/block_device_stream.rs` | 512 字节块设备到 lwext4 流接口的适配 |
| `fs/pipe.rs` | 8192 字节环形 pipe、阻塞/非阻塞、EOF 和 SIGPIPE |
| `fs/proc.rs` | `/proc`、`/sys` 部分节点和 RTC 伪文件 |
| `fs/stdio.rs` | stdin/stdout、null/zero/urandom/block、临时文件和空 socket |

设备与网络：

| 文件 | 主要责任 |
| --- | --- |
| `drivers/block/device.rs` | `BlockDevice` trait |
| `drivers/block/virtio_blk.rs` | VirtIO MMIO/PCI 块设备探测和 I/O |
| `drivers/block/ram_blk.rs` | 内存块设备回退 |
| `drivers/block/mod.rs` | 测试盘、根盘选择 |
| `drivers/net/virtio_net.rs` | smoltcp `Device` 到 VirtIO 网卡的适配 |
| `net/mod.rs` | 网卡、接口、socket set、DHCP、轮询和网络等待唤醒 |
| `net/socket.rs` | 实现 `File` 的 `SocketFile` |
| `net/syscall.rs` | socket 系统调用、本地 TCP/UDP 快速路径、阻塞 I/O |

系统调用：

| 文件 | 主要责任 |
| --- | --- |
| `syscall/mod.rs` | 调用号、六参数分发、最近调用调试信息 |
| `syscall/fs.rs` | fd、路径、目录、挂载、stat、同步、向量 I/O 等 |
| `syscall/process.rs` | 进程、线程、内存、时间、信号、凭证、futex、poll、共享内存等 |

### 3.3 用户态 `user/src`

- `lib.rs`：`_start`、8 MiB 用户堆、高层系统调用包装、用户 ABI 结构。
- `syscall.rs`：RISC-V、LoongArch、AArch64、x86_64 的内联汇编和原始包装。
- `console.rs`、`lang_items.rs`：用户态输出、panic 和分配失败。
- `linker.ld`：用户 ELF 的段布局。
- `bin/initproc.rs`：`main` 分支固定启动 shell；不要在日常测试时改成自动 runner。
- `bin/user_shell.rs`：命令解析、重定向、pipe、fork、exec。
- `bin/basic_runner.rs`、`contest_runner*.rs`、`ltp_runner.rs`、`iperf_runner.rs`：不同测试套件入口。
- `bin/cat.rs`、`count_lines.rs`、`getchar.rs`：简单用户工具。
- `bin/brktest.rs`、`filetest_simple.rs`、`huge_write.rs`：内存和文件测试。
- `bin/forktest*.rs`、`forktree.rs`、`exit.rs`、`matrix.rs`：进程和调度测试。
- `bin/pipetest.rs`、`pipe_large_test.rs`、`run_pipe_test.rs`：pipe 测试。
- `bin/sig_*.rs`、`until_timeout.rs`：信号与超时测试。
- `bin/sleep*.rs`、`nanosleep.rs`、`test_times.rs`、`yield.rs`：时间与调度测试。
- `bin/stack_overflow.rs`、`store_fault.rs`、`infloop.rs`：异常和抢占测试。
- `bin/tcp_echo_test.rs`：socket 测试。
- `bin/test_uid.rs`：凭证测试。
- `bin/mount.rs`、`umount.rs`：挂载测试。
- `bin/hello_*`、`fantastic_text.rs`、`cmdline_args.rs`、`usertests*.rs`、`shutdown.rs`：入口、参数、综合回归和关机。
- `bin_backup/`：暂未参与默认构建的专项测试素材。

### 3.4 ext4 和 C basic

`lwext4_rust/src`：

| 文件 | 作用 |
| --- | --- |
| `lib.rs` | crate 对外接口 |
| `blockdev.rs` | `KernelDevOp`、C 回调、mount/journal/writeback 管理 |
| `file.rs` | 较高层的 ext4 文件包装 |
| `bindings.rs` | bindgen 生成的 lwext4 C ABI |
| `ulibc.rs` | 为 C lwext4 提供 malloc/calloc/realloc/free/printf 等 |

`basic/user` 是一套 freestanding C 用户环境。`lib/syscall.c` 和各架构调用号头完成系统调用，`src/oscomp/*.c` 每个文件是一个 basic ELF 测例，CMake/Makefile 负责交叉编译。它是新增系统调用后最适合照着写最小 ABI 测试的地方。

## 4. 构建、镜像和运行

### 4.1 四种架构和比赛主线

Makefile 接受：

```text
riscv64 | loongarch64 | x86_64 | aarch64
```

比赛主线是 RISC-V 和 LoongArch。RISC-V 内核目标为
`riscv64gc-unknown-none-elf`，LoongArch 为 `loongarch64-unknown-none`。

最常用命令：

```bash
# 快速检查内核 Rust 代码
cargo check -p os --target riscv64gc-unknown-none-elf

# 构建单架构内核
make kernel ARCH=riscv64
make kernel ARCH=loongarch64

# 用户程序 + basic + 根文件系统
make fs-img ARCH=riscv64

# 单盘本地开发
make run ARCH=riscv64

# 双盘模拟比赛
make run-dual ARCH=riscv64 TEST_DISK=fs-img.img

# 完整提交产物
make all
```

`make fs-img` 的关键步骤是：

1. 编译全部 `user/src/bin/*.rs`；
2. 编译或复用 basic C 测例；
3. 创建 rootfs 目录，复制用户 ELF、BusyBox、测试和 runner；
4. 生成 BusyBox、lmbench、iozone、LTP 等脚本；
5. 用 `mke2fs -d` 制作 512 MiB ext4；
6. 用 `e2fsck` 检查并复制为 `fs-img.img`。

`main` 分支的 `initproc` 保持 shell 入口，运行后由你亲自输入测试命令。只有明确制作评测分支时，才改变自动测试入口。

### 4.2 单盘与双盘

```text
单盘：
  bus 0 ─┐
         ├─ 同一个 fs.img
  bus 1 ─┘

双盘：
  bus 0 ── TEST_DISK：测试方磁盘
  bus 1 ── ROOT_DISK：本项目根文件系统
```

`BLOCK_DEVICE` 通常指 bus 0，`ROOT_BLOCK_DEVICE` 通常指 bus 1。设备不足时存在回退逻辑。根目录由根盘挂载，测试脚本可再把测试盘挂到 `/mnt`。

### 4.3 内核是怎样链接起来的

`os/build.rs` 根据目标架构替换 `os/linker.lds` 中的三个占位符：

| 架构 | 内核虚拟基址 | 加载基址 |
| --- | ---: | ---: |
| RISC-V | `0xffffffc080200000` | 同虚址 |
| LoongArch | `0x9000000000200000` | `0x00200000` |
| AArch64 | `0xffff000040080000` | 同虚址 |
| x86_64 | `0xffff800000200000` | 同虚址 |

链接脚本依次放置 `.text.entry`、text、rodata、got、data、启动页表和 bss。入口符号为 `_start`。LoongArch 有自己的入口汇编；其余架构通过 `polyhal_boot::define_entry!` 生成入口。

## 5. 从上电到第一个用户程序

### 5.1 PolyHAL 替内核做了什么

根 `Cargo.toml` 把 `polyhal`、`polyhal-boot`、`polyhal-trap` 指向本地 `vendor`。三者分工是：

- `polyhal-boot`：早期栈、启动页表、特权寄存器和各架构入口；
- `polyhal`：地址、页表、时间、中断控制、调试串口、内核上下文切换；
- `polyhal-trap`：各架构陷阱汇编、`TrapFrame` 和统一 `TrapType`。

以 RISC-V 为例，启动代码建立恒等映射和高半内核映射，启用 Sv39，设置 S 态所需状态，再进入 Rust。`polyhal` 的构造阶段初始化 CPU、平台、驱动服务等。你在内核里看到的是已经被归一化的地址、页表和陷阱接口。

不要把 `define_entry!(main)` 理解为普通函数调用。它生成了真正的机器入口，最后才调用 `os/src/main.rs` 中的 `main(hartid)`。

### 5.2 `main` 的初始化顺序

hart 0 的流程是：

```text
init_heap
  → 初始化日志
  → polyhal::common::init(PageAllocImpl)
  → 读取平台可用内存并注册到物理帧分配器
  → mount_root
  → net_init
  → list_apps
  → 保存启动内核页表
  → add_initproc
  → run_tasks
```

顺序有依赖：

- 使用 `Arc`、`Vec`、Dentry 前必须先有内核堆；
- 页表和设备 DMA 需要物理帧；
- 从 ext4 读取 initproc 前必须有块设备和根文件系统；
- 创建任务后才能进入调度器。

`add_initproc` 使用编译期 `include_bytes!` 嵌入用户 ELF。`TaskControlBlock::new` 解析 ELF、创建地址空间和内核栈，设置用户 PC/SP，建立 0/1/2 三个 stdio fd，然后加入 PID 表和就绪队列。

## 6. 陷阱、系统调用 ABI 和返回用户态

### 6.1 四架构 ABI

| 架构 | 调用指令 | 调用号 | 参数 | 返回值 |
| --- | --- | --- | --- | --- |
| RISC-V | `ecall` | `a7/x17` | `a0..a5` | `a0` |
| LoongArch | `syscall 0` | `r11` | `r4..r9` | `r4` |
| AArch64 | `svc #0` | `x8` | `x0..x5` | `x0` |
| x86_64 | `syscall` | `rax` | `rdi,rsi,rdx,r10,r8,r9` | `rax` |

用户侧实现在 `user/src/syscall.rs`。内核侧 `TrapFrameArgs` 把架构差异隐藏掉，因此 `syscall(id, [usize; 6])` 始终看到六个机器字参数。

### 6.2 一次系统调用的完整过程

以 RISC-V `getpid()` 为例：

```text
user_lib::getpid
  → user/src/syscall.rs::sys_getpid
  → syscall(172, [0; 6])
  → ecall
  → polyhal-trap 保存用户寄存器，产生 TrapType::SysCall
  → kernel_interrupt
  → ctx.syscall_ok()：用户 PC 越过 ecall
  → syscall/mod.rs::syscall(172, args)
  → process.rs::sys_getpid
  → 返回 tgid
  → 写入 TrapFrameArgs::RET
  → 返回前处理信号
  → 恢复用户寄存器
```

`kernel_interrupt` 还负责：

- 进入时保存用户浮点状态并记用户时间；
- 读、写、取指缺页时调用 `MemorySet::handle_page_fault`；
- 非法指令转成 `SIGILL`；
- timer 中断时轮询网络、唤醒睡眠、检查 itimer 并抢占；
- 返回用户态前递送信号，致命信号则退出线程组；
- 离开时记内核时间。

### 6.3 当前调用号规则

大多数调用号采用 Linux generic/RISC-V ABI。当前还有三个项目自定义号：

```text
fork     = 401
sleep    = 300
shutdown = 403
```

`umount2` 是 39，`faccessat` 是 48。旧文档中“shutdown=48”的记录已经过时。未知调用统一返回 `-38`，即 `-ENOSYS`。

`syscall/mod.rs` 中的 `LAST_SYSCALL_ID/ARG0/ARG1/ARG2/RET/PID` 用于 panic 或调试时定位最近调用。热路径刻意不做格式化日志，因为 lmbench 对系统调用额外输出非常敏感。

## 7. 同步模型：最需要形成肌肉记忆的规则

`UPSafeCell<T>` 是 `RefCell<T>` 的单处理器封装，并通过 `unsafe impl Sync` 允许全局使用。它不是 SMP 锁，也不是“持有后可以随便调度”的锁。

必须遵守：

1. 同一 `UPSafeCell` 不能嵌套可变借用。
2. 调用可能再次访问同一对象的函数前，先 `drop(inner)`。
3. 调用 `schedule`、pipe/socket 阻塞、`sleep`、`waitpid` 前，必须释放所有 `RefMut`。
4. 不要持有 TCB 借用进入 `with_fs`，也不要持有文件系统借用再获取可能反向依赖的 TCB。
5. 从 fd 表取对象时，先 `Arc::clone`，再释放 TCB，最后执行 `file.read/write`。
6. 全局队列和单个任务状态要规定一致的加锁顺序；需要双阶段操作时做“入队后再检查”以避免丢失唤醒。

`sys_read`/`sys_write` 展示了正确形状：

```rust
let inner = task.inner_exclusive_access();
// 校验 fd、权限、用户缓冲区
let file = file.clone();
drop(inner);
file.read_or_write(...); // 这里可能阻塞
```

如果 panic 中出现 `already borrowed`，第一反应不是增加重试，而是画出当前路径持有哪些 `RefMut`，找出重入点。

## 8. 内存管理

### 8.1 内核堆和物理帧

内核堆是静态的 192 MiB 数组，由 `buddy_system_allocator::LockedHeap` 管理。它服务 `Vec`、`Arc`、`BTreeMap` 等动态对象。

物理页由另一个伙伴分配器管理。`FrameTracker` 是 RAII 所有者：

```text
frame_alloc → FrameTracker
clone shared metadata → 引用计数 +1
FrameTracker::drop → 引用计数 -1
最后一个引用 drop → 物理页归还伙伴系统
```

区分两种“分配”：

- 内核堆分配的是内核虚拟地址上的小对象；
- frame allocator 分配的是 4 KiB 物理页，用于页表、用户页和 DMA。

### 8.2 `MemorySet` 和 `MapArea`

`MemorySet` 包含：

- `Arc<PageTableWrapper>`：硬件页表；
- `Vec<MapArea>`：软件语义元数据；
- `shared_page_table`：是否与线程共享页表。

`MapArea` 记录：

- 虚拟页范围；
- `R/W/X/U` 权限；
- 已经分配的 `VPN → FrameTracker`；
- 可选文件、文件偏移；
- `MAP_SHARED` 与脏页集合。

硬件 PTE 只回答“这页映射到哪里、能否读写执行”；`MapArea` 才回答“这段内存为什么存在、缺页时从哪里取内容、是否共享”。处理缺页、munmap、mprotect 时两者必须同步。

### 8.3 ELF 和 `exec`

ELF 装载器读取程序头，对每个 `PT_LOAD` 建立区域，支持：

- 普通可执行文件和 PIE；
- `PT_INTERP` 动态解释器；
- `PT_TLS`；
- 文件内容加零填充 BSS；
- 1 MiB 用户栈，栈顶位于 `0x8_0000_0000` 附近。

`TaskControlBlock::exec_loaded` 还会构造 Linux 风格初始栈：

```text
高地址
  argv/env 字符串
  platform 字符串
  16 字节随机数
  对齐
  auxv（AT_PHDR、AT_PHNUM、AT_PAGESZ、AT_ENTRY、AT_RANDOM...）
  envp[]
  argv[]
  argc
低地址
```

然后替换 `memory_set`，关闭 `CLOEXEC` fd，重置信号处理动作，设置 PC/SP/参数/TLS，并唤醒可能等待 `vfork` 的父任务。

### 8.4 `fork` 的 COW

正确理解当前 COW：

1. 子进程创建自己的页表和区域元数据。
2. 父子先共享相同物理帧。
3. 对原本私有可写的页，父子 PTE 都去掉写权限。
4. 任一方写入时产生 store page fault。
5. 若物理帧仍被多个地址空间引用，分配新帧并复制一页。
6. 若只剩一个引用，只需恢复写权限。

所以 fork 时不是立即复制所有页面。旧文档中“子进程立刻分配新帧”的描述不准确。

### 8.5 `CLONE_VM`

线程通过 `clone` 的 `CLONE_VM` 共享同一个 `Arc<PageTableWrapper>`。每个 TCB 仍保存自己的 `MapArea` 元数据，因此 mmap、munmap 和缺页造成的元数据变化还要同步到同线程组任务。`sync_thread_group_vm_frame` 就是为这个模型补同步。

这比“所有线程直接共享一个 `MemorySet`”更复杂。修改相关逻辑时，要同时问：

- PTE 是否已改变？
- 当前任务的 MapArea 是否已改变？
- 同 tgid 的其他任务元数据是否也应改变？
- 最后一个线程退出时，谁负责真正释放共享页表？

### 8.6 mmap、munmap、mprotect

`sys_mmap` 负责校验参数、选择地址并创建匿名或文件后备区域。许多页面延迟到首次访问才真正分配。文件映射缺页时从 `File::read_at` 填充；共享映射的脏页在 `msync`、`munmap` 等路径回写。

`munmap` 和 `mprotect` 不是简单删除一项：传入范围可能只覆盖一个 MapArea 中段，因此要把区域拆成左、中、右三段，并同步更新 PTE、物理帧、文件偏移和 dirty 集合。

`brk` 则维护 `heap_bottom` 和 `program_brk`，按程序堆边界增减映射。

### 8.7 用户指针模型

当前页表设计让内核能直接解引用当前用户虚拟地址，所以：

```rust
translated_ref(ptr)
translated_refmut(ptr)
translated_byte_buffer(ptr, len)
translated_str(ptr)
```

本质上只是把裸指针构造成 Rust 引用/切片，不会替你做安全检查。调用前必须完成：

1. 空指针策略；
2. `addr + len` 溢出检查；
3. 跨页范围检查；
4. 每一页属于带 `U` 的 MapArea；
5. 读入参数要求 `R`，写回结果要求 `W`；
6. 对齐和 `size_of::<T>()`；
7. 必要时触发延迟页/COW，使 PTE 真正可访问；
8. C 字符串必须有最大长度，不能无限找 `\0`。

`MemorySet::valid_user_buf` 校验 MapArea 权限。`process.rs` 中的
`prepare_task_user_access` 还会逐页处理缺页和 COW，是复杂读写指针更稳妥的模板。
路径应使用 `fs.rs::translated_path_bounded`，当前上限 `PATH_MAX=4096`、单分量
`NAME_MAX=255`。

注意：`mm/page_table.rs::translated_str` 自己没有边界，不能直接用于不可信新接口。

## 9. 进程、线程和调度

### 9.1 TCB 里有什么

`TaskControlBlock` 外层只有 PID 句柄和 `UPSafeCell<TaskControlBlockInner>`。Inner 可按六类记忆：

| 类别 | 代表字段 |
| --- | --- |
| 执行现场 | `trap_cx`、浮点状态、`task_cx`、内核栈、状态 |
| 地址空间 | `memory_set`、堆边界 |
| 进程关系 | parent、children、tgid、pgid、sid、vfork |
| 资源 | fd 表、CLOEXEC、cwd、root、rlimit |
| 身份 | uid/euid/suid、gid/egid/sgid、groups、umask、nice |
| 异步状态 | signal、action、mask、睡眠、itimer、robust list、tid 地址、阻塞原因 |

每次实现进程类系统调用，先明确它修改的是“线程字段”还是“线程组/进程字段”。

### 9.2 调度器

当前是单 CPU FIFO：

```text
TASK_MANAGER.ready_queue
  → fetch_task
  → 标记 Running
  → PROCESSOR.current = task
  → context_switch_pt(idle → task，同时切页表)
  → task_entry
  → run_user_task
```

用户任务通过 timer 抢占、主动 yield、阻塞或退出切回 idle。`schedule` 使用启动时保存的内核页表回到调度循环。

状态机：

```text
Ready ──调度──> Running
  ▲                │
  │                ├─ yield/timer ──> Ready
  │                ├─ 等待事件 ─────> Blocked
  │                └─ exit ─────────> Zombie
  └────事件/超时/信号唤醒──── Blocked
```

全局还维护睡眠、网络/pipe/poll、waitpid、futex 和延迟释放队列。

### 9.3 fork、clone、exec、exit、wait

| 操作 | 地址空间 | fd | 父子/线程组 | 关键返回 |
| --- | --- | --- | --- | --- |
| `fork` | COW，新页表 | `Arc` 复制并调用引用钩子 | 新进程、新 PID/tgid | 父得子 PID，子得 0 |
| `clone` | 按 flags，可共享 VM | 当前实现按 tgid 同步 fd 变化 | 可成为同线程组 | 支持 TLS、parent/child tid、vfork |
| `execve` | 全部替换 | 保留非 CLOEXEC | PID 不变 | 成功后从新入口运行 |
| `exit` | 当前执行实体退出 | 清理资源 | 变 Zombie，通知父 | 不返回 |
| `wait4` | 不变 | 不变 | 回收匹配的 Zombie 子任务 | PID 与编码状态 |

退出码采用 Linux wait status：正常退出是低 8 位退出值左移 8 位；信号退出返回信号编号格式。父进程不 wait 时，Zombie 仍需保留退出信息。孤儿任务会重新挂到 initproc。

`exit_group` 和致命信号会作用于同 tgid。线程的 `CLONE_CHILD_CLEARTID` 地址退出时清零并 futex wake；robust futex 链用于标记 owner died。

### 9.4 阻塞的正确协议

一个健壮的阻塞系统调用通常是：

```text
loop:
  1. 检查事件是否已满足，满足则返回
  2. 若 O_NONBLOCK，返回 -EAGAIN
  3. 把当前任务加入等待队列并标记 Blocked
  4. 再检查一次事件，关闭“检查与入队之间”的竞态窗口
  5. 释放 TCB、文件对象内部借用和全局队列借用
  6. take_current_task + schedule
  7. 被唤醒后检查信号、超时和事件，回到 loop
```

即使现在只有单核，中断也可能落在步骤之间，所以“丢失唤醒”仍然存在。

## 10. 信号、时间与 futex

### 10.1 信号递送

待处理信号和 mask 是位集合，动作表在 TCB 中。每次陷阱返回用户态前：

1. 找一个未被 mask 的 pending signal；
2. `SIGKILL`、STOP/CONT 和默认致命信号走内核动作；
3. `SIG_IGN` 丢弃；
4. 用户 handler 则备份 TrapFrame；
5. 在用户栈写 siginfo/ucontext；
6. 把用户 PC 改为 handler、参数改为信号信息；
7. RA 指向 `SIGNAL_RETURN_TRAMPOLINE`；
8. handler 返回到跳板时触发取指异常，内核恢复保存的上下文。

当前实现的 `MAX_SIG=34`，不是完整 Linux 的 64 个信号。修改 `SignalAction`、sigset 或 ucontext 时必须保持用户/内核 `#[repr(C)]` 布局一致。

### 10.2 时间

PolyHAL 提供单调时间。内核用它实现：

- user/kernel CPU 时间记账；
- `gettimeofday`、`clock_gettime/getres`；
- nanosleep 和 sleep queue；
- interval timer 到期注入 `SIGALRM`；
- poll、信号等待和网络操作超时。

确认单位是调时间系统调用最重要的步骤：源码中同时有 nsec、usec、msec 和 clock tick，转换错三位就会造成“看似卡死”。

### 10.3 futex

futex 快路径只比较用户空间的一个 `u32`，不满足才阻塞。私有 futex key 使用地址空间页表根加虚址；共享 futex 尽量使用物理地址。实现或修改时必须处理：

- 地址对齐和用户可访问性；
- `FUTEX_PRIVATE_FLAG`；
- 入队前后的值重检；
- 超时与信号打断；
- WAKE 数量；
- requeue；
- `CLONE_CHILD_CLEARTID`；
- robust list 的 `FUTEX_OWNER_DIED`。

## 11. 文件描述符、VFS 和 ext4

### 11.1 `File` trait 是 fd 世界的中心

fd 表类型是：

```rust
Vec<Option<Arc<dyn File + Send + Sync>>>
```

`File` 提供 readable/writable、read/write、定点 I/O、stat、目录、seek、flags、poll、
dup/close 引用钩子和 downcast。它让下面这些对象都能放入 fd 表：

```text
OSInode | Pipe | Stdin/Stdout | /dev/* | proc/sys 伪文件 | SocketFile
```

因此实现一个 fd 系统调用时，优先面向 `File` trait，不要先写死 ext4。确实需要普通文件语义时，再通过 `as_any().downcast_ref::<OSInode>()`。

### 11.2 Dentry、超级块和路径

`VfsSuperBlock` 持有一个 `UPSafeCell<FsInner::Ext4>`，所有 lwext4 操作在
`with_fs` 闭包中串行执行。

`Dentry` 是“某个名字在目录树中的节点”，包含：

- 用户可见 `path`；
- 对底层文件系统使用的 `fs_path`；
- inode、文件类型和父节点；
- 所属超级块；
- 子节点缓存。

路径解析要同时处理：

- 绝对路径从任务 `root` 开始；
- 相对路径从 cwd 或 dirfd 开始；
- `.`、`..`；
- chroot 边界；
- symlink，当前深度限制为 8；
- 到达挂载点时跳入另一个超级块；
- 从挂载根执行 `..` 时回到挂载点父层；
- 缓存命中、失效和创建/删除后的更新。

### 11.3 一次 open

```text
sys_openat
  → 有界复制用户路径
  → 解析 dirfd/cwd/root
  → 处理 /proc、/sys、/dev 特殊对象
  → 校验目录遍历和目标访问权限
  → open_file_at
  → VFS 解析父目录和名字
  → 可选 create/truncate/tmpfile/symlink follow
  → OSInode
  → alloc_fd
  → 写入 fd_table 和 CLOEXEC
```

新文件会使用当前任务的 euid/egid。`fstat` 再从 inode 读取 uid/gid 和其他元数据。

打开后 unlink 的文件不能立即让现有 fd 失效。当前 `OSInode` 用打开 inode 计数，并把仍被打开的被删文件临时重命名为 `.os-unlinked-*`，最后一个引用关闭后再真正删除。

### 11.4 块设备到 lwext4

```text
VirtIOBlock
  → BlockDevice（512 字节块）
  → BlockDeviceStream
  → lwext4_rust::KernelDevOp C 回调
  → Ext4BlockWrapper
  → lwext4 C API
  → VfsSuperBlock::with_fs
  → OSInode/VFS
```

`BlockDeviceStream` 对齐时批量读写，不对齐时做 read-modify-write。lwext4 wrapper 负责设备注册、挂载、日志恢复/启动和 writeback。`ulibc.rs` 给嵌入式 C 库提供内存分配等符号。

不要持有 `with_fs` 闭包跨调度，也不要在闭包中进入可能再次访问同一超级块的 VFS 路径。

### 11.5 pipe

pipe 两端共享 8192 字节环形缓冲：

- 空缓冲且写端仍存在：阻塞读或 `O_NONBLOCK → -EAGAIN`；
- 空缓冲且无写端：返回 0，表示 EOF；
- 满缓冲且读端仍存在：阻塞写或 `-EAGAIN`；
- 无读端：发送 `SIGPIPE` 并返回 `-EPIPE`；
- `dup_ref/close_ref` 维护读写端数量并唤醒对方。

这是学习“File trait + 阻塞队列 + fd 生命周期”的最佳小模块。

### 11.6 伪文件系统的真实边界

`/proc`、部分 `/sys` 和 `/dev` 节点由内核对象拦截生成，并不是完整挂载的 procfs/sysfs/devfs。所谓 tmpfs 主要是 ext4 存储加内核内存中的容量统计，也不是独立内存文件系统。实现测试语义时可以沿用，但不要误以为已经具备 Linux 完整虚拟文件系统。

## 12. 网络

RISC-V/AArch64 路径大致是：

```text
VirtIO 网卡
  → VirtIONetAdapter
  → smoltcp Interface + SocketSet
  → SocketFile
  → socket 系统调用
```

timer 中断调用 `net_poll` 推动协议栈，并唤醒网络阻塞任务。没有可用 DHCP 时使用回退地址。为了让本机测试稳定，内核还实现了本地 TCP/UDP 队列：

- TCP listener、连接和双向缓冲；
- UDP 报文队列；
- 临时端口；
- accept/connect/send/recv 的本地快速路径。

当前支持重点是 IPv4 TCP/UDP、bind/listen/accept/connect/send/recv 和常用 sockopt。LoongArch、x86_64 的网络路径存在禁用或桩实现，IPv6、原始套接字等也不完整。新增 socket 调用时，必须同时考虑：

1. fd 是否确实是 `SocketFile`；
2. 本地快速路径和 smoltcp 路径；
3. nonblock、超时和信号；
4. sockaddr 的长度、字节序和可写回长度；
5. 关闭时如何唤醒对端。

## 13. 用户态、shell 与测试程序

### 13.1 `_start`

用户 ELF 从 `user_lib::_start(argc, argv)` 开始：

1. 初始化静态 8 MiB 用户堆；
2. 从初始栈的 argv 指针数组构造 `Vec<&str>`；
3. 调用弱符号 `main`，实际由每个 bin 提供；
4. 把 main 返回值交给 `exit`。

用户库的包装返回原始 `isize`。自行编写 Rust 测试时要直接判断负错误码；musl/glibc 通常会把 Linux 负错误转换成 `-1` 并设置 `errno`。

### 13.2 initproc 和 shell

`initproc` fork 后让子进程 exec `user_shell`，父进程持续 wait 回收孤儿。shell 解析引号、空格、管道和重定向，通过 fork、dup、close、exec 组织命令。

当前 shell 的 `cd` 在 fork 后的子任务中执行，因此不会持久改变 shell 父进程 cwd。这是阅读 shell 时应能识别出的语义缺口。决赛测试主要依赖 BusyBox shell 时，不一定触发自制 shell 的这个限制。

### 13.3 测试层次

```text
用户库小测试
  → basic 单系统调用 C 测试
  → BusyBox 命令
  → libc-test / LTP 子集
  → lmbench / iozone / iperf / netperf
  → 双盘整套回归
```

越靠下越容易定位 ABI 问题，越靠上越能发现系统调用之间的组合语义。新增调用时永远先写最小测试，不要直接拿完整 LTP 猜原因。

## 14. 系统调用实现全景

### 14.1 分发层

`os/src/syscall/mod.rs` 做三件事：

1. 定义调用号；
2. 把六个 `usize` 参数转换成具体 Rust 类型；
3. `match` 到 `sys_*`，未知号返回 `-ENOSYS`。

分发层只应做薄转换，不应塞入业务逻辑。例如：

```rust
SYSCALL_OPENAT => sys_openat(
    args[0] as isize,
    args[1] as *const u8,
    args[2] as u32,
    args[3] as u32,
),
```

参数个数、符号、指针可变性和 32/64 位宽在这里第一次固定。写错一个 `as`，handler 本身再正确也没有用。

当前系统调用按模块分为：

| 类型 | 主要文件 | 代表调用 |
| --- | --- | --- |
| fd 与路径 | `syscall/fs.rs` | openat、read/write、close、dup、fcntl、stat、getdents |
| 目录和挂载 | `syscall/fs.rs` | mkdirat、unlinkat、linkat、renameat2、mount、umount2 |
| 进程线程 | `syscall/process.rs` | clone、fork、execve、exit、wait4、getpid/tid |
| 虚拟内存 | `syscall/process.rs` | brk、mmap、munmap、mprotect、msync、madvise |
| 信号时间 | `syscall/process.rs` | kill、rt_sigaction、nanosleep、clock_gettime、itimer |
| 凭证资源 | `syscall/process.rs` | uid/gid、groups、rlimit、priority、rusage |
| 同步与等待 | `syscall/process.rs` | futex、ppoll、pselect6、robust list |
| System V 共享内存 | `syscall/process.rs` | shmget、shmat、shmctl、shmdt |
| 网络 | `net/syscall.rs` | socket、bind、listen、accept、connect、send/recv |

有些接口只实现了测试需要的子集，有些是兼容桩。例如某些调度亲和、epoll、rseq、
RISC-V hwprobe 路径可能返回固定值或 `-ENOSYS`；`shmdt` 当前也是简化实现。判断“已分发”
不等于判断“语义完整”。

### 14.2 返回值和 errno

内核直接返回：

```text
成功：0、字节数、fd、PID、地址等非负值
失败：-errno
```

常用值：

| 错误 | 数值 | 典型场景 |
| --- | ---: | --- |
| EPERM | 1 | 无权进行全局或身份操作 |
| ENOENT | 2 | 路径不存在 |
| EBADF | 9 | fd 无效或方向错误 |
| ECHILD | 10 | 没有匹配子进程 |
| EAGAIN | 11 | 非阻塞操作暂不可完成 |
| ENOMEM | 12 | 页或堆分配失败 |
| EACCES | 13 | 文件权限不足 |
| EFAULT | 14 | 用户地址无效 |
| EBUSY | 16 | 对象正在使用 |
| EEXIST | 17 | 要创建的对象已存在 |
| ENOTDIR | 20 | 路径中要求目录的位置不是目录 |
| EISDIR | 21 | 把目录当普通文件读写 |
| EINVAL | 22 | 参数、flag 或组合不合法 |
| EMFILE | 24 | fd 达到限制 |
| EROFS | 30 | 只读挂载上写入 |
| EPIPE | 32 | pipe/socket 对端已关闭 |
| ENAMETOOLONG | 36 | 路径或分量过长 |
| ENOSYS | 38 | 未实现调用 |
| ENOTEMPTY | 39 | 删除非空目录 |
| ELOOP | 40 | symlink 循环或过深 |

不要为了“让测试继续”随意返回 0。成功返回 0 会让 libc 和应用相信副作用已经发生，
之后往往在更远的位置失败，比立即返回准确错误更难排查。

### 14.3 ABI 结构体

跨用户/内核边界的结构体必须：

- 使用 `#[repr(C)]`；
- 字段顺序、宽度、有无符号与 Linux 目标 ABI 一致；
- 明确 padding；
- 时间结构明确秒/纳秒及字段位宽；
- 用户和内核若各定义一份，逐字段核对；
- 不在 ABI 中直接放 Rust `bool`、`enum`、`Vec`、`String`、trait object；
- 写回前全部初始化，避免泄漏内核栈或堆中的未初始化字节。

`Kstat`、`SignalAction`、`TimeSpec`、`UtsName`、socket address、`PollFd` 都是现成参考。

## 15. 决赛新增系统调用：标准工作流

这一章是最需要练熟的部分。任何指定系统调用都按同一流程处理。

### 15.1 第一步：先写“语义合同”

在动代码前，写下：

```text
调用名和目标架构调用号：
六个寄存器参数各是什么类型：
哪些指针可为 NULL：
每个指针由内核读还是写，长度从哪里来：
结构体精确布局：
成功返回什么：
所有预期 errno：
是否修改 TCB / fd / VFS / 页表：
是否可能阻塞：
是否被信号打断：
fork/clone/exec 后状态如何：
32 位 flag 中未知位如何处理：
测试程序会观察哪些副作用：
```

调用号和结构体应以比赛目标架构的 Linux UAPI、libc 调用方式和实际测试程序为准。
不要只凭函数名猜语义，也不要从 x86_64 调用表抄到 RISC-V。

### 15.2 第二步：选择归属模块

| 系统调用特征 | 放置位置 | 先阅读的相似实现 |
| --- | --- | --- |
| 只读/修改 PID、凭证、时间 | `process.rs` | getppid、getcpu、times、priority |
| 用户输出结构 | `process.rs` 或 `fs.rs` | uname、sysinfo、fstat |
| fd 操作 | `fs.rs` | read、write、dup3、fcntl |
| 路径操作 | `fs.rs` + `inode/vfs` | mkdirat、unlinkat、fstatat |
| 地址空间 | `process.rs` + `memory_set.rs` | mmap、mprotect、mincore |
| 阻塞等待 | `process.rs`/`fs.rs` + `task` | futex、ppoll、pipe |
| socket | `net/syscall.rs` + `socket.rs` | accept、sendto、getsockopt |
| 架构状态 | PolyHAL/trap | 时钟、页表、hwprobe 类接口 |

### 15.3 第三步：接入调用号和分发

在 `os/src/syscall/mod.rs`：

```rust
const SYSCALL_XXX: usize = /* 目标 ABI 的真实调用号 */;

// syscall() 的 match 中
SYSCALL_XXX => sys_xxx(
    args[0],
    args[1] as *mut SomeAbiStruct,
    args[2] as u32,
),
```

检查清单：

- 是否与现有号冲突；
- signed 参数是否应先转 `isize/i32`；
- 输入指针用 `*const T`，输出指针用 `*mut T`；
- 大偏移在该 ABI 是否拆成两个寄存器；
- flag 应保留多少位；
- 是否需要导入 handler。

用户 Rust 测试若要调用，再在 `user/src/syscall.rs` 加同号原始包装，并在
`user/src/lib.rs` 加高层包装。BusyBox/musl/glibc 自带 syscall 汇编，不依赖
`user_lib`，所以只改用户库不能让比赛二进制识别内核实现。

### 15.4 模板 A：无指针、不会阻塞

现有 `getppid` 就是模板：

```rust
pub fn sys_getppid() -> isize {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    match &inner.parent {
        Some(parent) => parent
            .upgrade()
            .map(|p| p.getpid() as isize)
            .unwrap_or(1),
        None => 1,
    }
}
```

这种调用仍要想清楚：

- Linux 返回的是 pid 还是 tgid；
- parent 弱引用升级失败时的语义；
- init 的父 PID；
- 是否需要权限检查；
- 是否能在获取另一个 TCB 时造成借用顺序问题。

### 15.5 模板 B：向用户写一个结构体

下面是一个仅用于本地练习的私有 `osinfo` 示例。私有号只是练习；决赛必须换成题目规定的真实号和真实 ABI。

```rust
#[repr(C)]
struct OsInfo {
    page_size: usize,
    total_frames: usize,
    free_frames: usize,
}

pub fn sys_osinfo(out: *mut OsInfo) -> isize {
    if out.is_null() {
        return -14;
    }
    let task = current_task().unwrap();
    if !prepare_task_user_access(
        &task,
        out as usize,
        core::mem::size_of::<OsInfo>(),
        true,
    ) {
        return -14;
    }
    let token = {
        let inner = task.inner_exclusive_access();
        inner.memory_set.token()
    };
    *translated_refmut(token, out) = OsInfo {
        page_size: crate::config::PAGE_SIZE,
        total_frames: crate::mm::total_frames(),
        free_frames: crate::mm::available_frames(),
    };
    0
}
```

这里最重要的不是三个字段，而是顺序：

```text
NULL → 范围/权限/缺页准备 → 取 token → 释放 TCB → 完整写回 → 返回
```

如果 ABI 允许 `out=NULL` 表示查询长度，就不能一概返回 EFAULT；应按合同处理。
这个示例应放在能访问 `prepare_task_user_access` 的 `process.rs`。其他模块也需要同样能力时，
应把准备逻辑提取成统一的 `pub(crate)` 内存辅助函数，而不是复制出多个逐渐不一致的版本。
此外，`translated_refmut<T>` 要求地址满足 `T` 的对齐；若目标 ABI 明确允许非对齐结构体
指针，应在完整范围校验后使用字节复制或 `write_unaligned`，不能构造未对齐的 Rust 引用。

### 15.6 模板 C：fd I/O

fd 类接口按下面的骨架：

```rust
pub fn sys_xxx(fd: usize, user_buf: *mut u8, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    let task = current_task().unwrap();
    let token = current_user_token();
    let inner = task.inner_exclusive_access();

    let Some(file) = inner.fd_table.get(fd).and_then(|slot| slot.as_ref()) else {
        return -9;
    };
    if !inner
        .memory_set
        .valid_user_buf(user_buf as *const u8, len, true)
    {
        return -14;
    }
    let file = file.clone();
    drop(inner);

    let buf = translated_byte_buffer(token, user_buf, len);
    file.read(buf)
}
```

真实实现还要根据方向检查 readable/writable、目录、offset、append、O_NONBLOCK、
只读挂载和信号。最关键的规则是：执行可能阻塞的 `file.*` 前已经释放 TCB 借用。
`valid_user_buf` 只验证 MapArea 元数据；若新接口要求在调用文件对象前保证 lazy/COW 页面
已经落成 PTE，应复用或提取前一节的逐页 prepare 逻辑。

### 15.7 模板 D：路径操作

路径类调用优先放进 `fs.rs`，复用它的私有辅助函数：

```text
translated_path_bounded
  → path_has_too_long_component
  → resolve_base_dir_for_path / AT_FDCWD
  → check_traverse_to_leaf
  → vfs_resolve_path 或 vfs_resolve_parent_and_name
  → 权限与 flag 检查
  → inode/vfs 原子操作
  → Dentry 缓存更新
```

不要自己用 `translated_str` 复制路径。`*at` 接口至少要区分：

- 绝对路径忽略 dirfd；
- 相对路径 + `AT_FDCWD` 使用 cwd；
- 相对路径 + 普通 dirfd 要验证 fd 存在且是目录；
- 非法负 dirfd 返回 `EBADF`；
- `AT_EMPTY_PATH`、`AT_SYMLINK_NOFOLLOW` 等 flag；
- symlink 是否跟随；
- chroot 后的 root。

### 15.8 模板 E：会阻塞

阻塞调用不要从零发明调度协议。先选最相似路径：

- 等文件就绪：`ppoll/pselect6`；
- 等用户字：`futex`；
- 等 pipe：`pipe.rs`；
- 等网络：`net/syscall.rs`；
- 等子进程：`waitpid`；
- 等时间：`nanosleep`。

然后核对：

```text
[ ] 非阻塞模式是否立即 -EAGAIN
[ ] 入队和状态修改是否不可被观察为半完成
[ ] 入队后是否重查条件
[ ] schedule 前是否释放所有借用
[ ] 唤醒方是否清除 block_reason
[ ] 超时是否从全部相关队列移除
[ ] 信号是否返回 -EINTR 或重启
[ ] 对象关闭时是否唤醒等待者
[ ] 多次伪唤醒后是否仍用 loop 重检
```

### 15.9 第四步：写最小测试

对内核 ABI 最可靠的是 freestanding C：

```c
// 伪代码：调用号必须换成题目规定值
long ret = syscall3(NR_xxx, arg0, arg1, arg2);
if (ret < 0) {
    printf("FAIL: ret=%ld\n", ret);
    return 1;
}
printf("PASS\n");
return 0;
```

测试不能只有 happy path。至少覆盖：

1. 正常输入；
2. NULL；
3. 未映射地址；
4. 跨页缓冲区；
5. 长度 0；
6. 错误 fd/flag；
7. 边界大小；
8. fork/clone 后状态；
9. 非阻塞；
10. 信号或关闭打断阻塞。

若加入 official basic 风格测试，输出保持：

```text
========== START test_xxx ==========
...
========== END test_xxx ==========
```

在 `main` 上不要改 initproc 自动运行它。启动 shell 后手工输入测试命令。

### 15.10 第五步：按风险逐层回归

```bash
# 1. 语法和类型
cargo check -p os --target riscv64gc-unknown-none-elf

# 2. 内核
make kernel ARCH=riscv64

# 3. 用户程序与文件系统
make fs-img ARCH=riscv64

# 4. 单盘启动，shell 手工执行最小测试
make run ARCH=riscv64

# 5. 双盘
make run-dual ARCH=riscv64 TEST_DISK=fs-img.img

# 6. LoongArch
make kernel ARCH=loongarch64

# 7. 相关 BusyBox/libc/LTP 回归

# 8. 完整产物
make all
```

只改文档不需要重建整个镜像；改系统调用 ABI、用户库或测试 ELF 时必须重建 fs-img。

## 16. 三条必须会手画的端到端调用链

### 16.1 `read` 普通文件

```text
用户 read
 → trap
 → syscall/mod.rs
 → fs.rs::sys_read
 → 校验 fd、方向、用户输出缓冲区
 → clone Arc<dyn File> 并 drop TCB
 → OSInode::read
 → 锁 offset
 → VfsSuperBlock::with_fs
 → lwext4 file read
 → BlockDeviceStream
 → VirtIOBlock
 → 拷贝进用户 buf
 → 返回字节数
```

### 16.2 `fork` 后首次写

```text
sys_fork
 → TaskControlBlock::fork
 → MemorySet::from_existed_user
 → 父子共享 FrameTracker
 → 私有可写 PTE 都改只读
 → 子任务入 ready queue
 → 某一方 store
 → StorePageFault
 → handle_page_fault(Write)
 → 若共享计数 > 1：分配、复制、重映射 W
 → 若共享计数 = 1：原页恢复 W
 → 原指令重新执行
```

### 16.3 pipe 空读

```text
sys_read
 → Arc<dyn File> 是 Pipe
 → Pipe::read
 → 缓冲为空且仍有 writer
 → 当前任务标记 Blocked(PipeRead(id))
 → 加入等待队列，释放借用，schedule
 → writer 写入并 wake_pipe_read_tasks(id)
 → reader 变 Ready
 → 再次调度
 → loop 重检并复制数据
 → 返回字节数
```

能在不看代码的情况下画出这三条链，说明你已经把陷阱、任务、内存、fd 和驱动连接起来了。

## 17. 从零重写这个系统的正确顺序

“会写出来”不是把 3 万行一次默写，而是知道依赖顺序和每阶段最小闭环。

### 阶段 0：只启动和打印

实现入口、链接脚本、栈、bss 清零和串口。验收：QEMU 打印一句话后稳定停住。

### 阶段 1：内核堆与物理页

接入平台内存区域、伙伴分配、`FrameTracker` 和内核堆。验收：反复分配释放，帧数回到原值。

### 阶段 2：页表和高半内核

能创建、映射、取消映射和切换页表，保留内核全局映射。验收：新页表下仍能打印并读写测试页。

### 阶段 3：陷阱

保存/恢复通用寄存器，识别 syscall、timer、page fault。验收：用户程序能 ecall 并原路返回。

### 阶段 4：单用户程序

解析 ELF、映射段、建用户栈、设置 PC/SP，进入 U 态。先只实现 write 和 exit。验收：`hello_world` 输出并退出。

### 阶段 5：调度

实现 TCB、内核栈、上下文切换、FIFO 队列和 timer 抢占。验收：两个无限循环任务仍能交替输出。

### 阶段 6：进程

先完整复制 fork，再优化成 COW；实现 exec 和 wait。验收：父子返回值、退出码、孤儿回收正确。

### 阶段 7：统一 fd

定义 `File`，先接 stdio 和 pipe，再接普通文件。验收：重定向和 `cmd1 | cmd2`。

### 阶段 8：块设备、ext4 和 VFS

先块读写，再 ext4 wrapper，再 Dentry、路径、mount 和 OSInode。验收：跨目录创建、读写、rename、unlink-open-file。

### 阶段 9：高级虚拟内存

加入 lazy mmap、文件映射、COW、mprotect、munmap 拆分。验收：跨页、部分解除、fork 后写隔离。

### 阶段 10：信号、线程、futex

先 pending/default action，再用户 handler/sigreturn，然后 clone flags、tid 和 robust futex。验收：musl 多线程创建、等待、退出。

### 阶段 11：兼容性

按 basic → BusyBox → libc-test → LTP 的失败逐个补 ABI。每次只修一个可复现最小用例。

### 阶段 12：网络和性能

接 VirtIO 网卡、smoltcp、SocketFile、阻塞唤醒，再做本地路径和性能优化。验收：echo、iperf/netperf，并确认日志不污染热路径。

这条顺序体现一个原则：每次只增加一个新抽象，并立刻形成可运行闭环。

## 18. 调试手册

### 18.1 未知系统调用

症状：应用返回 `Function not implemented`，内核值是 `-38`。

排查：

1. 查 `LAST_SYSCALL_ID` 或临时只打印未知号；
2. 在 `syscall/mod.rs` 查常量和 match；
3. 核对目标架构调用号；
4. 确认不是调用已接入但 handler 主动返回 `-38`；
5. 查看调用前后的其他调用，确定真实缺口。

不要在所有 syscall 热路径打印，会严重拖慢甚至扰乱测试。

### 18.2 `-EFAULT`

按顺序问：

```text
指针是否允许 NULL？
长度是否发生乘法/加法溢出？
读写方向是否写反？
结构体 size/alignment 是否正确？
范围是否跨页？
MapArea 是否带 U/R/W？
lazy/COW 页面是否已 prepare？
字符串是否有界且带 NUL？
```

如果内核直接 page fault 或 panic，说明很可能在校验前用了 `translated_*`。

### 18.3 卡死或 RefCell 借用 panic

画“持锁图”：

```text
当前 TCB RefMut
  → File 内部 UPSafeCell
  → VfsSuperBlock::with_fs
  → 全局等待队列
  → 另一个 TCB
```

检查是否：

- 持有任意借用调用 `schedule`；
- 持 TCB 调 `file.read/write`；
- `with_fs` 内重新解析路径；
- 同时获取两个任务时顺序不一致；
- 入等待队列后没有任何唤醒路径；
- 对象 close 后没有唤醒等待者。

### 18.4 Rust 小测试过、BusyBox 或 libc 失败

重点不是算法，而是 ABI：

- 调用号是否为 RISC-V generic；
- flag 的未知位是否该拒绝或忽略；
- `#[repr(C)]`、padding、字段宽度；
- 可选 NULL；
- errno 是否准确；
- `O_CLOEXEC`、`O_NONBLOCK`、append；
- fork/exec 后继承；
- 信号打断和部分读写；
- 路径 `*at`、symlink 和 dirfd；
- Linux 要求返回字节数，是否却固定返回 0。

### 18.5 文件系统异常

按层分离：

```text
fd 是否有效
 → File 类型和 offset
 → Dentry/path/fs_path
 → mount 跳转
 → VfsSuperBlock
 → lwext4 返回
 → BlockDeviceStream 对齐
 → VirtIO 设备选择
```

创建、删除、rename 后若只有第二次访问出错，优先查 Dentry 缓存失效。双盘只有比赛失败，优先查 bus 0/bus 1 和挂载来源。

### 18.6 GDB 和反汇编

```bash
make disasm ARCH=riscv64
make gdbserver ARCH=riscv64
make gdbclient ARCH=riscv64
```

也可取消 Makefile 中 QEMU `-D qemu.log -d ...` 的注释查看机器级日志。遇到用户异常时同时记录：

- 当前 PID/tgid；
- trap type 和 bad address；
- SEPC；
- 最近 syscall；
- 对应 MapArea 和 PTE flags；
- 任务状态与 block reason。

## 19. 当前实现中必须知道的边界和技术债

这些不是否定当前系统，而是修改代码时的风险地图：

1. `UPSafeCell` 建立在单 CPU 假设上，不能直接扩成 SMP。
2. 用户指针是当前地址空间中的直接裸指针，所有调用者都必须自律校验；部分旧路径的校验强度并不完全一致。
3. `getdents64` 等少数路径应继续审计用户缓冲区校验，不能盲目复制其旧写法到新调用。
4. `CLONE_VM` 共享页表、各 TCB 保留元数据，需要显式同步，复杂度较高。
5. fd 表不是一个直接共享的 `Arc<FdTable>`；当前通过同 tgid 任务传播 fd 安装/关闭来接近线程共享语义。
6. proc/sys/dev 是按路径拦截的有限伪文件，不是完整 Linux 文件系统。
7. tmpfs 是 ext4 后备加配额记账，不是纯内存文件系统。
8. System V 共享内存、epoll、rseq、亲和性、hwprobe 等存在部分实现或兼容桩。
9. `sysinfo` 的部分内存值是兼容性近似，并非全部来自实时分配器统计。
10. 自制 shell 的 `cd` 不会持久改变父 shell cwd。
11. 网络重点覆盖 IPv4 TCP/UDP；各架构能力不完全一致。
12. 自定义 300/401/403 调用号不是 Linux 标准接口，不能和官方调用混淆。

决赛补调用时，先判断题目是否会碰到这些边界。若会，不要只在 syscall handler 表面返回一个值，而要把底层语义补齐。

## 20. 旧文档与当前源码的关键差异

阅读仓库历史文档时尤其注意：

| 主题 | 当前源码 |
| --- | --- |
| 用户栈 | `4096 * 256 = 1 MiB` |
| 内核栈 | `4096 * 4 = 16 KiB` |
| 内核堆 | `0xC000000 = 192 MiB` |
| pipe 环形缓冲 | 8192 字节 |
| 最大信号索引范围 | `MAX_SIG = 34` |
| fork COW | 父子先共享帧并只读，写缺页时才复制 |
| shutdown | 自定义 403 |
| faccessat | 48 |
| main initproc | shell，不自动跑 contest runner |
| 用户 ELF | 已有 `PT_INTERP`/动态解释器装载路径，不应再概括成“全部静态” |

`documents/VFS与Dentry实现原理.md` 仍适合深入学习路径和挂载；迁移记录适合了解历史原因。但发生冲突时，当前源码优先。

## 21. 建议的四周学习计划

### 第一周：能从启动讲到用户态

- 第 1 天：Makefile、build.rs、linker、PolyHAL boot；
- 第 2 天：`main.rs` 和 TrapFrame；
- 第 3 天：frame allocator、heap、页表；
- 第 4～5 天：MemorySet、ELF、用户栈；
- 第 6 天：手画 hello/write/exit；
- 第 7 天：不看文档复述完整启动链。

验收：自己添加一个只返回常量的私有 syscall 和用户测试，然后完整撤销练习改动。

### 第二周：进程和内存

- TCB、scheduler、上下文切换；
- fork/COW、clone、exec、wait/exit；
- mmap、munmap、mprotect、page fault；
- 信号和时间。

验收：用日志证明父子 COW 前后物理页变化；写一个跨页用户指针测试。

### 第三周：文件系统和阻塞

- File trait、fd 生命周期；
- Dentry、mount、symlink、chroot；
- OSInode、lwext4、块设备；
- pipe、poll、futex。

验收：实现一个本地练习 fd 查询调用；测试 bad fd、关闭、dup、fork、nonblock。

### 第四周：兼容性与决赛演练

- socket 和网络；
- basic、BusyBox、libc-test、LTP 的测试方式；
- 每天随机选一个未实现/简化 syscall，限时完成“合同—实现—最小测试—回归—复盘”；
- RISC-V 完成后至少做一次 LoongArch 编译；
- 模拟双盘和 `make all`。

验收：两小时内独立完成一个带用户结构体和错误路径的指定系统调用。

## 22. 决赛现场检查表

拿到新系统调用题目后按顺序打勾：

```text
ABI
[ ] 调用号来自正确架构
[ ] 六个参数的顺序、位宽、有无符号正确
[ ] 结构体 repr(C)、padding、时间单位正确
[ ] NULL 和长度 0 语义明确
[ ] 成功值和全部 errno 明确

内核
[ ] syscall/mod.rs 有唯一常量和 match
[ ] handler 放在正确模块
[ ] 加法、乘法、页对齐无溢出
[ ] 所有用户指针先校验再解引用
[ ] 字符串有 PATH_MAX/专用上限
[ ] fd clone 后释放 TCB
[ ] 阻塞前释放所有借用
[ ] 入队后重检，关闭丢唤醒窗口
[ ] nonblock、signal、timeout、close 已处理
[ ] fork/clone/exec/exit 继承与清理已处理
[ ] Dentry/PTE/线程组元数据需要同步时已同步

测试
[ ] 最小正常测试
[ ] NULL/坏地址/跨页
[ ] 错误 fd/flag/边界长度
[ ] fork 或多线程
[ ] 阻塞与 nonblock
[ ] cargo check
[ ] RISC-V kernel + fs-img
[ ] shell 手工测试
[ ] 相关 BusyBox/libc/LTP
[ ] 双盘
[ ] LoongArch 编译
[ ] make all

提交
[ ] main 的 initproc 仍是 shell
[ ] 没带入磁盘镜像等产物
[ ] 没改无关代码
[ ] 没有高频调试日志
[ ] 没有未经用户要求的 push
```

## 23. 最后的掌握标准

不要以“我看过每个文件”为掌握标准，而用下面五级判断：

1. **定位**：看到测试失败，能找到入口模块。
2. **追链**：能从用户调用追到最终状态或设备。
3. **解释**：能说明对象所有权、锁、页表和错误码为什么这样设计。
4. **修改**：能补语义而不破坏阻塞、继承和缓存。
5. **验证**：能写最小 ABI 测试，并按层次回归。

达到第五级，你就不再是在“给这个内核打补丁”，而是在真正维护一个操作系统。决赛新增系统调用也不再是猜代码，而是把一个明确的 Linux ABI 合同，接入这套已经理解的任务、内存、文件和调度模型。
