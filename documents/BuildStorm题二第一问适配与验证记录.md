# BuildStorm 题二第一问（Q2.1）适配与验证记录

## 1. 文档目的、范围和当前结论

本文记录 ScintillaOS 为 2026 年全国大学生操作系统大赛线上决赛 BuildStorm
第二题第一问（Q2.1）所做的排障、内核语义补充和本地验证。Q2.1 不是只要求
`rustc --version` 能打印版本号；它要求在 glibc 用户态中真实运行 Rust 工具链，并
完成：

1. `rustc --version` 与 `cargo --version`；
2. `cargo new --vcs none /tmp/minibuild`；
3. 在新工程内执行 `cargo build`；
4. 运行构建产物并验证输出为 `Hello, world!`。

截至 2026-07-30，RISC-V64 正常 release 内核已经在官方 Docker 镜像
`zhouzhouyi/os-contest:20260510` 中，以 `-machine virt -cpu max -smp 4` 完整通过
上述真实命令链。最终串口日志包含：

```text
BUILDSTORM_TOOLCHAIN ok
BUILDSTORM_MINIBUILD ok
#### OS COMP TEST GROUP END buildstorm ####
```

最终证据保存在 `tmp/buildstorm-q21-rv-officialcpu.log`。官方 judge 对这两个标记
分别计 8 分和 12 分，因此当前已验证 Q2.1 的脚本部分为 20/20 分。本文不把这个
结论扩展为 Q2.2 的完整 ArceOS 编译成功，也不把 RISC-V 的结果扩展为 LoongArch
的结果；这两项仍需要独立验证。

本文以当前 `main` 工作树为对象。四核调度、VFS、futex、内存管理等底层改动中有
一部分来自此前的 SMP 适配工作，详细背景见
`Documents/四核SMP适配与死锁修复全记录.md`。本文只展开说明与 BuildStorm Q2.1
调用链直接相关的改动及其验证证据，避免把无关变更误说成 Q2.1 的必要条件。

## 2. 官方测例和判定口径

官方脚本位于
[`scripts/buildstorm_testcode.sh`](https://github.com/oscomp/testsuits-for-oskernel/blob/final-2026/scripts/buildstorm_testcode.sh)，
评分逻辑位于
[`judge/judge_buildstorm-glibc.py`](https://github.com/oscomp/testsuits-for-oskernel/blob/final-2026/judge/judge_buildstorm-glibc.py)。
Q2.1 的脚本约定如下：

| 项目 | 真实执行内容 | 成功标记 | 分值 |
| --- | --- | --- | --- |
| 工具链加载 | `rustc --version && cargo --version` | `BUILDSTORM_TOOLCHAIN ok` | 8 |
| 最小构建 | `cargo new`、`cargo build`、执行产物 | `BUILDSTORM_MINIBUILD ok` | 12 |

judge 使用正则搜索上述标记，而不是依赖 shell 提示符的精确格式。因此串口中可能
出现 `# ` 提示符或多核日志穿插，但只要真实命令成功且标记存在，结果即可被识别。
本轮没有修改官方脚本、官方 rootfs、工具链二进制、`/proc/uptime` 或系统时钟，也
没有用预生成的 `minibuild` 二进制替代实际构建。

## 3. 验证环境与复现方式

| 项目 | 实际配置 |
| --- | --- |
| 内核分支 | `main` |
| 目标架构 | RISC-V64，`riscv64gc-unknown-none-elf` |
| 容器 | `zhouzhouyi/os-contest:20260510` |
| QEMU | 容器内 QEMU 10.0.2 |
| CPU 参数 | `-machine virt -cpu max -smp 4` |
| 内存 | 本轮 Q2.1 验证使用 `-m 1G` |
| 测试盘 | `/private/tmp/scintillaos-final-rv-pub.img` 的 qcow2 overlay |
| 根文件系统盘 | 当前工程构建出的 `disk-rv.img` 的 qcow2 overlay |
| 用户态 | 测试盘内 Debian glibc、Rust nightly-2026-05-28、离线 cargo 缓存 |

验证时通过 `busybox chroot /mnt /bin/sh` 进入官方 glibc rootfs，再输入与官方
Q2.1 完全等价的命令。之所以先进入真实 `/bin/sh`，而不是让内核自带
`user_shell` 解析一整段嵌套引号命令，是因为 `user_shell` 只提供简单交互式命令
解析，不能可靠保留 `sh -c` 中的复杂引号。这个输入方式只解决宿主串口注入问题，
不会改变 guest 中真正执行的命令。

本地临时驱动位于 `tmp/run_buildstorm_q21_rv.sh`，该文件被忽略，不属于提交产物。
它只在真实条件成立时打印官方标记：

```sh
if rustc --version && cargo --version; then
    echo "BUILDSTORM_TOOLCHAIN ok"
else
    echo "BUILDSTORM_TOOLCHAIN fail"
fi

if cargo new --vcs none /tmp/minibuild >/dev/null 2>&1 \
   && ( cd /tmp/minibuild && cargo build >/dev/null 2>&1 ) \
   && [ "$(/tmp/minibuild/target/debug/minibuild)" = "Hello, world!" ]; then
    echo "BUILDSTORM_MINIBUILD ok"
else
    echo "BUILDSTORM_MINIBUILD fail"
fi
```

最后一次有效运行显式使用 `QEMU_CPU=max`，没有保留早期调试时的 `v=true` 向量
扩展。项目默认竞赛配置同样使用 `-cpu max`，因此最终证据不依赖非官方的向量
扩展行为。

## 4. 初始故障链与定位过程

Rust/Cargo 的最小构建会同时覆盖比普通 shell 程序更复杂的内核路径：动态 ELF
加载、`clone` 创建线程、`CLONE_FILES` 共享描述符、pipe、Unix socketpair、
`execve`、`waitpid`、futex、`mmap`、`munmap`、`madvise`、`cc`、`collect2` 和
`ld`。因此“能启动 cargo”不等于“能够完成构建”。

最早的失败日志 `tmp/buildstorm-minibuild-rv-pipe64-diag.log` 记录了以下错误：

```text
the CLOEXEC pipe failed: Os { code: 9, ... Bad file descriptor }
error: worker panicked
CARGO_BUILD_RC=101
```

在同一段跟踪中，`socketpair` 创建后，子进程经 `execve` 启动 `rustc`。这说明错误
名虽然叫作 “CLOEXEC pipe”，但 Rust 标准库的进程控制通道实际使用了带
`SOCK_CLOEXEC` 的 Unix 域 socketpair。内核此前只让 Unix socket 对象走普通
`read`/`write` 的 `File` 接口，而 RISC-V Linux ABI 下 `recv` 对应
`recvfrom(207)`；`sys_recvfrom` 又只识别 TCP/UDP `SocketFile`，从而对 Unix
socket 返回 `-EBADF`。这正是 userspace 看到 errno 9 的原因。

之后的诊断运行还暴露了线程组退出问题：leader 在 `exit_group` 或 `execve` 路径
扫描同组线程时用 `try_inner_*` 读取 TCB。若某个 sibling 正在 `munmap` 等 syscall
中持有自己的 `TaskLock`，扫描会把“锁暂时竞争”错误地当作“该线程不在组内”，导致
leader 开始退出后仍有 sibling 执行。这种遗漏会表现为偶发卡住、futex 等待者不再
返回或进程回收次序异常。

排障不是通过屏蔽错误输出、缩短 cargo 命令或伪造 judge 标记完成的，而是按 Linux
进程、fd、socket 和线程语义补齐相应实现。下面按子系统说明。

## 5. 进程执行与 errno 语义

### 5.1 `execve` 的错误区分

修改位置：`os/src/syscall/process.rs` 的 `sys_exec`。

以前 `execve` 的多个失败场景混用 `-1`，用户态无法区分“路径不存在”和“文件不是
可执行映像”。Cargo/GCC 的 PATH 搜索会依次尝试多个候选程序，例如
`/root/.cargo/bin/cc`、`/usr/local/bin/cc`、`/usr/bin/cc`。若不存在的候选不返回
`ENOENT`，上层程序可能把它当作异常而不再继续搜索。

现在的语义是：

| 场景 | 返回值 | 含义 |
| --- | --- | --- |
| 可执行文件或 ELF interpreter 不存在 | `-ENOENT`，即 `-2` | 可以继续 PATH 搜索或报告文件不存在 |
| `#!` 行格式无效、ELF 格式不合法、映像加载失败 | `-ENOEXEC`，即 `-8` | 文件存在但不可执行 |
| ELF 或脚本解释器加载成功 | `0` | 切换到新映像 |

实现同时保留并使用既有的 `PT_INTERP` 解析、动态链接器加载、脚本解释器 fallback、
启动栈、auxv 和 TLS 初始化逻辑。这些不是为了某个 cargo 命令特判，而是 glibc
动态 ELF 能正常进入 `_start` 和动态链接器所需的基础 ABI。

### 5.2 `execve` 前的线程收敛

Linux 在多线程进程执行 `execve` 时，必须先终止并回收其他线程，再替换共享地址
空间。当前实现通过 `terminate_other_threads_for_exec()` 标记同组 sibling，唤醒其
阻塞状态并请求运行 hart 重新调度；执行 `execve` 的线程循环确认已没有同 TGID
的非 Zombie peer 后，才加载新映像。远端线程不会由调用方直接改写 trap frame，
而是由其自身 trap/信号路径完成退出，避免跨核破坏运行上下文。

## 6. `CLONE_FILES`、fd 生命周期与 close-on-exec

### 6.1 问题

Rust 的线程和子进程创建路径会组合 `CLONE_VM`、`CLONE_THREAD`、`CLONE_FILES`、
`CLONE_CHILD_CLEARTID` 等 flag。Linux 的 `CLONE_FILES` 语义是同一 `files_struct`
中的描述符安装、关闭、`FD_CLOEXEC` 变更应对所有共享者立即可见。早期每个 TCB
各自保存 `fd_table`，即使某次安装同步到部分线程，也会在并发 clone、close、dup 或
exec 时出现不一致。

### 6.2 结构与锁顺序

修改位置：`os/src/task/mod.rs`、`os/src/task/task.rs`、
`os/src/task/manager.rs`、`os/src/syscall/fs.rs`、`os/src/net/syscall.rs`。

当前实现没有把原有的 `Vec<Option<Arc<dyn File>>>` 大规模重构为一个全新的
`Arc<FilesStruct>`，而是在保持 TCB 布局兼容的前提下建立显式共享组：

```text
TaskControlBlockInner.files_id
        |
        v
FILE_TABLE_LOCK: FileTableState
        |
        +-- FileTableGroup { files_id, members: Vec<Weak<TaskControlBlock>> }
```

具体规则如下：

1. 每个 TCB 的 `files_id` 标识其 Linux `files_struct` 语义组；非共享 clone 使用
   新 id，`CLONE_FILES` child 继承 parent 的 id。
2. `add_task()` 在将任务发布到 PID registry 前调用 `register_files_task()`。这样
   共享描述符组不需要在持有 `FilesLock` 时反向查询 `PID2TCB`，避免
   `FilesLock -> PidRegistryLock` 的锁顺序逆转。
3. `clone_shared_vm()` 在 child 发布前复制 parent 的 fd 槽位和 close-on-exec 位，
   对每个文件调用 `dup_ref()`，然后将 child 注册到同一个 `files_id` 组。
4. `file_group_tasks_locked()` 只从该组的 `Weak<TCB>` 升级 Arc、过滤 Zombie 或
   已离组成员，并返回稳定快照。PID registry 锁在这一步之前已经释放。
5. `install_fd_shared()`、`close_fd_shared()`、`duplicate_fd_shared()`、
   `set_fd_cloexec_shared()` 统一在 `FILE_TABLE_LOCK` 下对同组表执行同一项变更。
   `open`、`pipe2`、`socket`、`accept`、`eventfd2`、`epoll_create1`、
   `socketpair`、`dup*`、`fcntl` 和 `close_range` 都复用这些入口。
6. `close_fd_shared()` 在锁内只从 fd 表中移出文件，把待关闭的 `Arc<File>` 收集到
   局部 `Vec`；释放 `FILE_TABLE_LOCK` 后才调用 `file.close_ref()`。这是必要的，
   因为 `close_ref()` 可能唤醒 pipe/socket 等待者并进入调度器，不能在文件表锁中
   回调到低等级锁。

这种设计保证可见性和引用计数的一致性，同时保持既有 fd table API 的改动范围较小。
它也修复了诊断期曾出现的 `Files.descriptor_tables -> PidRegistry` lockdep 反向获取。

### 6.3 `FD_CLOEXEC`

`exec_file_with_interp()` 通过 `take_cloexec_fds()` 取出当前映像需关闭的文件；文件
引用同样在释放 `TaskLock` 后调用 `close_ref()`。这使 `SOCK_CLOEXEC` socketpair 和
普通 `O_CLOEXEC` 文件描述符在 exec 成功时按 Linux 规则关闭，而不在非 exec 的
线程中意外失效。

## 7. pipe 语义与安全的用户缓冲区访问

修改位置：`os/src/fs/pipe.rs`。

### 7.1 容量

环形 pipe buffer 从 8192 字节改为 `16 * PAGE_SIZE`，即 RISC-V 4 KiB 页下的
65536 字节。Linux 默认 pipe 容量也是 16 页。这里调整的是容量，不是把大写入
错误地声明为原子；POSIX `PIPE_BUF` 的原子性边界并未因此改变。

较小的 8 KiB 容量会放大“父进程等待 child，child 因诊断/错误输出写满 pipe 而
等待父进程读取”的闭环风险。64 KiB 不能替代正确的 read/write 阻塞语义，但能够
与 Linux 的默认行为对齐并降低正常工具链路径的无谓阻塞。

### 7.2 不在设备锁中触碰用户页

pipe 读写现在先通过预分配的内核 scratch buffer 从/向用户空间复制，再在
`PipeRingBuffer` 锁内仅操作内核内存。原因是用户页可能触发 COW 或按需缺页；若在
`DeviceLock` 持有期间直接访问用户 slice，trap handler 可能需要 `CpuLocal`、
`TaskLock`、`VmLock`，从而造成锁递归或逆序。该改动同时保留：

| 情况 | 行为 |
| --- | --- |
| 读端全部关闭后写 | 发送 `SIGPIPE`，返回 `-EPIPE` |
| 非阻塞且无空间/无数据 | 返回 `-EAGAIN` |
| 阻塞读写 | 使用统一阻塞状态机登记和唤醒 |
| 信号中断 | 返回 `-EINTR` |

## 8. Unix 域 socketpair 与 `recvfrom`/`sendmsg` 路径

### 8.1 AF_UNIX socketpair 对象

新增文件：`os/src/fs/unix_socket.rs`；系统调用入口：
`os/src/syscall/fs.rs::sys_socketpair`。

Q2.1 所需的是 `AF_UNIX` 的一对已连接端点，而不是路径名 bind/listen/connect。
实现支持：

| 能力 | 当前语义 |
| --- | --- |
| domain/type | `AF_UNIX`，`SOCK_STREAM` 与 `SOCK_SEQPACKET` |
| flags | `SOCK_NONBLOCK`、`SOCK_CLOEXEC` |
| 数据组织 | 两个方向各有一个记录队列，使用同一 `DeviceLock` 保护 |
| 容量 | 每方向最多 64 条记录、256 KiB 排队数据，单记录上限 64 KiB |
| stream | 可部分读取，维护 record offset |
| seqpacket | 每次接收消费一个记录；不足的用户缓冲区会截断该记录 |
| recv flags | `MSG_PEEK`、`MSG_DONTWAIT` |
| send flags | `MSG_DONTWAIT`、`MSG_NOSIGNAL` |
| 对端关闭 | recv 返回 EOF；send 返回 `-EPIPE`，未设置 `MSG_NOSIGNAL` 时发送 `SIGPIPE` |

队列满时任务按 pipe 读写等待键进入统一阻塞状态机；收到数据、消费数据、关闭端点时
都会唤醒相应读写者以及 poll 等待者。因此该实现不是“固定返回成功”的评测特判，
而是有容量、阻塞、EOF、信号和引用计数语义的内核对象。

当前不支持 pathname Unix socket、`SCM_RIGHTS`、credentials ancillary data、
`SOCK_DGRAM` 等更宽泛 Linux 功能；这些限制在第 13 节列为后续工作，不影响本次
真实调用的 socketpair 控制通道。

### 8.2 Linux socket syscall 适配

修改位置：`os/src/net/syscall.rs` 与 `os/src/syscall/mod.rs`。

原来的 `sock_lookup()` 只接受网络层的 TCP/UDP `SocketFile`。增加
`with_unix_socket()` 后，先从当前 fd 表取得 `Arc<File>`，再安全 downcast 为
`UnixSocketFile`；若成功就使用 Unix socket 语义，否则继续原有 TCP/UDP 路径。
因此不会把 AF_UNIX 混入 smoltcp 的 socket handle 管理。

下列 RISC-V Linux ABI 入口均增加了 AF_UNIX 分支：

| 系统调用 | ABI 号 | Unix socket 行为 |
| --- | --- | --- |
| `socketpair` | 199 | 创建 stream/seqpacket 双端点并写入两个 `u32` fd |
| `sendto` | 206 | 无目的地址时发送数据；带非空目的地址返回 `-EISCONN` |
| `recvfrom` | 207 | 接收数据；无地址语义时写回 `addrlen = 0` |
| `sendmsg` | 211 | 聚合 iovec 后发送 |
| `recvmsg` | 212 | 接收后拷回 iovec，清空 name/flag 输出字段 |

这修复了 Rust 在接收 child exec 状态时通过 `recvfrom` 获得 `EBADF` 的根因。普通
`read`/`write` 仍可工作，但仅实现它们不足以兼容使用 socket API 的 libc/Rust
路径。

## 9. 线程组退出、`execve` 与共享地址空间

### 9.1 稳定的线程组成员识别

修改位置：`os/src/task/mod.rs` 的 `collect_thread_group_tasks()`，以及
`os/src/syscall/process.rs` 的 `thread_group_has_peer()`。

旧实现采用类似下面的 best-effort 逻辑：

```text
task_snapshot()
  -> other.try_inner_borrow()
  -> 锁竞争时返回 None
  -> 当作“不属于当前线程组”
```

这是错误的并发语义。线程恰好持有 `TaskLock` 不改变其 TGID，也不意味着它已经
退出。新的收集算法为：

```text
task_snapshot()                 // 只复制 Arc，随后释放 PID registry 锁
  -> 逐个 other.inner_exclusive_access()
  -> 判断 tgid 相同且不是 Zombie
  -> 形成稳定的 sibling Arc 列表
```

调用位置没有持有 PID registry、scheduler 或当前 TCB 的锁，所以等待一个 sibling
短暂完成 syscall 是安全的。对于 `exit_group` 和 `execve` 而言，等待得到稳定成员
关系也是正确的语义：不能为了避免一次 spin 等待而遗漏正在 `munmap` 的线程。

### 9.2 退出协议

`force_exit_task_group()` 对 stable snapshot 中每个 sibling 写入：

```text
killed = true
exit_code = group exit code
terminating_for_group = true
SIGKILL pending
```

随后调用 `wake_task()` 并对实际运行 hart 请求重新调度。sibling 在本地 trap/signals
路径调用 `exit_current_and_run_next()`，由 `begin_exit()` 原子地移出调度队列并发布
Zombie，再由 idle reaper 在安全的内核页表上下文回收资源。这样不会由 leader 的
CPU 强行回收一个可能仍在别的 CPU 运行的 TCB。

诊断成功日志中可以看到 leader 和被杀 sibling 均经过 `exit-enter -> zombie ->
registry-removed -> deferred -> reap`，之后父进程 `waitpid` 正常收集 child。该证据
比单纯“程序不再卡住”更能说明线程组生命周期已闭合。

### 9.3 共享 VM 的边界

`MemorySet` 对 `CLONE_VM` 共享 page table、VMA metadata、地址空间活跃 CPU mask 和
`brk` 状态。mmap、munmap、mprotect、COW 权限变化会在释放 Task/Vm lock 后触发远端
TLB 刷新。`execve` 只有在同组 sibling 全部退出后才替换自己的 `MemorySet`；这保证
没有线程继续使用将被替换的页表。

## 10. `madvise(MADV_DONTNEED)` 与页大小正确性

修改位置：`os/src/syscall/process.rs::sys_madvise` 与
`os/src/mm/memory_set.rs::madvise_private_anonymous_dontneed`。

Rust 运行时会对大量匿名内存执行 `MADV_DONTNEED`。实现先检查：

1. `start` 必须页对齐；
2. 范围不得溢出，并且必须已映射到用户地址空间；
3. 每一页必须属于 private anonymous area；文件映射和 `MAP_SHARED` 返回 `-EINVAL`；
4. 若 frame 因 COW 仍被共享，先分配一个私有页并更新 PTE；
5. 对实际覆盖页清零并清除 dirty 标记。

关键修复是使用：

```rust
frame.paddr.slice_mut_with_len::<u8>(PAGE_SIZE).fill(0);
```

而不是依赖泛型元素类型推导的整段切片。前者明确写入恰好一个 4 KiB 页，避免类型
推导把页长度按更宽元素解释而覆盖相邻物理页。该实现目前实现的是“后续读取到零值”
的可见语义，并未把 private anonymous page 立即归还 buddy allocator；这比假装完整
reclaim 更准确，也避免把尚有引用的 frame 过早释放。

## 11. 其他兼容对象：eventfd、epoll 与 syscall 分派

当前工作树还新增了 `os/src/fs/eventfd.rs` 和 `os/src/fs/epoll.rs`，并在
`os/src/syscall/fs.rs`、`os/src/syscall/mod.rs` 接入：

| 对象 | 已实现的核心语义 | 与 Q2.1 的关系 |
| --- | --- | --- |
| `eventfd2(19)` | 64 位计数器、semaphore 模式、非阻塞 `EAGAIN`、阻塞唤醒、poll 状态 | 未在最终 Q2.1 日志中直接观察到，但可覆盖更复杂工具链事件循环 |
| `epoll_create1/ctl/pwait(20/21/22)` | interest 列表、`ADD/DEL/MOD`、基本 level/edge/oneshot 就绪计算、fd 引用保持 | 未作为 Q2.1 成功的前提宣称；为后续复杂构建做兼容准备 |
| socket API 分派 | 199、206、207、211、212 号 syscall 进入正确实现 | Q2.1 Unix 控制通道直接依赖 |

eventfd/epoll 的加入没有把未支持的调用伪装为成功。参数错误、资源耗尽和非阻塞条件
仍返回标准 errno；当前实现范围也不等同于完整 Linux epoll/Unix socket 功能集。

## 12. 构建、运行与最终验证

### 12.1 静态构建检查

在项目根目录执行：

```sh
cargo check -p os --target riscv64gc-unknown-none-elf
make kernel-rv SMP_DIAGNOSTICS=0
git diff --check
```

结果：三项均成功。编译输出仍有仓库既有的 unused import、未来兼容性和 vendor
warning；没有因为本轮改动新增的编译错误或补丁空白错误。

### 12.2 诊断构建的中间证据

使用 `make kernel-rv SMP_DIAGNOSTICS=1` 后运行最小 BuildStorm 链路，可以看到：

```text
CARGO_NEW_RC=0
CARGO_BUILD_RC=0
HELLO_RC=0
Hello, world!
```

同一日志还显示 cargo 启动 `rustc`、`/usr/bin/cc`、`collect2` 和 `/usr/bin/ld`，并
展示多线程组退出与 reaper 完整闭环。诊断 feature 仅增加 lockdep 和日志，不改变
用户 ABI；最终评分验证使用的是关闭 diagnostics 的正常 release 内核。

### 12.3 最终官方格式复测

最终运行使用的容器命令等价如下：

```sh
docker run --rm -w /work \
  -v /Users/hermit/code/OS/oskernel2026test:/work \
  -v /private/tmp:/private/tmp \
  --entrypoint /bin/bash zhouzhouyi/os-contest:20260510 \
  -lc 'QEMU_MEM=1G QEMU_SMP=4 QEMU_CPU=max \
       SHUTDOWN_DELAY=420 bash tmp/run_buildstorm_q21_rv.sh'
```

最终日志 `tmp/buildstorm-q21-rv-officialcpu.log` 的关键片段为：

```text
[smp] online mask 0xf
rustc 1.98.0-nightly (... 2026-05-27)
cargo 1.98.0-nightly (... 2026-05-26)
BUILDSTORM_TOOLCHAIN ok
BUILDSTORM_MINIBUILD ok
#### OS COMP TEST GROUP END buildstorm ####
```

`BUILDSTORM_MINIBUILD ok` 只有在新建工程、真实 `cargo build` 和实际执行
`/tmp/minibuild/target/debug/minibuild` 输出匹配时才会打印。完成标记出现后停止临时
Docker 容器，只是为了避免驱动脚本的固定 `sleep` 继续占用 QEMU；不会影响已经结束的
guest 构建和串口结果。

## 13. 相关源文件清单

| 文件 | 与 Q2.1 的作用 |
| --- | --- |
| `os/src/syscall/process.rs` | `execve` errno、shared-VM exec 收敛、clone/futex/madvise 等进程语义 |
| `os/src/task/mod.rs` | 文件表共享组、稳定线程组枚举、组退出与 exec sibling 终止 |
| `os/src/task/task.rs` | `files_id`、clone 继承、共享 VM TCB 初始化、CLOEXEC 回收 |
| `os/src/task/manager.rs` | 任务发布前注册 file-table group，调度与退出状态机 |
| `os/src/syscall/fs.rs` | 统一 fd 安装/关闭/复制、pipe2、socketpair、eventfd、epoll |
| `os/src/fs/pipe.rs` | 64 KiB ring、阻塞唤醒、用户缓冲区 scratch copy、SIGPIPE/EOF |
| `os/src/fs/unix_socket.rs` | AF_UNIX stream/seqpacket endpoint、队列、recv/send flag 语义 |
| `os/src/net/syscall.rs` | Unix socket 的 sendto/recvfrom/sendmsg/recvmsg 分支，保留 TCP/UDP 原路径 |
| `os/src/syscall/mod.rs` | RISC-V syscall 19-22、199、206、207、211、212 的分派 |
| `os/src/mm/memory_set.rs` | shared VM VMA/page-table 元数据、COW、MADV_DONTNEED 每页处理 |
| `os/src/fs/eventfd.rs` | eventfd 计数器对象 |
| `os/src/fs/epoll.rs` | epoll interest 与就绪事件对象 |
| `os/src/fs/mod.rs` | 将 Unix socket、eventfd、epoll 暴露给 syscall 层 |

## 14. 已验证边界与后续工作

当前结果应按以下边界理解：

| 项目 | 状态 |
| --- | --- |
| RISC-V Q2.1 工具链加载 | 已在官方 Docker、四核、正常 release 内核验证 |
| RISC-V Q2.1 `cargo new/build/run` | 已在同一环境验证 |
| Q2.1 官方 score marker | 已输出并可被 judge 正则识别 |
| LoongArch Q2.1 | 尚未在本轮同样验证，不应据此声称通过 |
| Q2.2 `cargo xtask arceos build` | 尚未运行完成，不应声称构建成功或获得时间分 |
| Q2.2 评分参数 | 尚未使用官方要求的多核大内存配置做完整基准测试 |
| `rseq(293)` 与 `riscv_hwprobe(258)` | 当前返回 `ENOSYS`，Q2.1 工具链能够 fallback；复杂构建前应评估是否需要补充 |
| AF_UNIX pathname/ancillary data | 未实现；当前 socketpair 控制通道不需要 |

下一阶段应先在足够磁盘和内存资源下运行官方 Q2.2 脚本的 untimed `tg-xtask` 预构建，
记录第一个真实缺失的 syscall 或文件系统语义，再逐项按 Linux 语义修复。只有完整
`cargo xtask arceos build` 返回 0、产物大小符合要求、guest `/proc/uptime` 真实计时
正常，才可以讨论 40 分构建门槛和 120 分性能部分。

## 15. 当前工作树逐文件变更审计

本节补充回答“实际改了哪些文件”。执行本文所述验证后，当前工作树的已跟踪源码差异为
19 个文件、2895 行新增和 942 行删除；此外还有三个新增但尚未纳入 Git 的源码模块。
这个数字是当前 `git diff --numstat` 的审计结果，不把被忽略的临时运行脚本、qcow2
overlay、内核镜像或测试日志统计为内核源码改动。

这些代码不是一个可独立 cherry-pick 的单一 Q2.1 小补丁：其中一部分来自此前为四核
SMP、文件系统和 Rust 工具链排障所做的连续工作。本表按当前内容逐项归类，说明每个文件
承担的职责以及与 Q2.1 的关系。

| 文件 | 具体变更 | 与本次验证的关系 |
| --- | --- | --- |
| `os/src/drivers/block/virtio_blk.rs` | 在 `smp-diagnostics` feature 下加入受监视物理页在 VirtIO DMA 前后是否变化的检查，并可打印 rustc 可执行文件相关扇区的首字。正常 release 构建不启用这段日志。 | 仅用于定位曾出现的多核内存破坏，不是 Q2.1 成功的运行时依赖。 |
| `os/src/fs/inode.rs` | 复用 VFS 的动态长度 symlink 读取函数；对 `ext4_fread` 的 out-count 增加“不能大于请求长度”的边界校验；诊断构建记录可能发生的受监视页改写。 | 动态链接器、`cc` 与 cargo 会大量读取 ELF、脚本和符号链接；边界校验防止 C ABI 返回异常长度时越界推进 Rust slice。 |
| `os/src/fs/mod.rs` | 导出 Unix socket、eventfd、epoll 模块及其类型，使 syscall 层能够创建相应内核对象。 | Unix socket 是已验证的直接依赖；eventfd/epoll 是后续兼容能力。 |
| `os/src/fs/pipe.rs` | pipe ring 从 8 KiB 改为 16 页；读写先经 scratch buffer 复制用户数据，避免持有 `DeviceLock` 时触发 COW/缺页；保留 EOF、`EPIPE`/`SIGPIPE`、`EAGAIN`、`EINTR` 和阻塞唤醒。 | cargo/rustc 的标准输入输出与子进程管道直接依赖。 |
| `os/src/fs/vfs.rs` | 将 ext4 内部目录项类型转换为 Linux `DT_*` 值；统一 `ext4_fread` 长度检查；实现按 inode 实际大小读取 symlink；路径解析改为组件队列并限制最多 40 层 symlink；解析创建路径的父目录时跟随中间 symlink。 | 动态 loader、PATH、`/usr/bin` 和 Debian 文件树依赖可靠 symlink 与目录遍历。 |
| `os/src/main.rs` | 增强 `smp-diagnostics` 下的用户缺页、非法指令和受监视页破坏打印，记录 PC 指令字、寄存器参数、映射信息与潜在别名。 | 仅诊断用途，release 行为不通过日志改变。 |
| `os/src/mm/frame_allocator.rs` | 把启动时对整段可用 RAM 的一次性清零改为“帧分配时清零”；补齐 frame 引用计数的诊断跟踪。 | 这是大内存 QEMU 启动速度的通用优化；分配时仍清零，未牺牲进程隔离。 |
| `os/src/mm/memory_set.rs` | 增加 `MremapResult`，实现保留 frame 内容的缩小/原地扩展/可移动重映射；为匿名和文件 `mmap(NULL, ...)` 提供原子地址选择与 VMA 发布；增加 ELF 读取长度校验、共享地址空间活跃 CPU 跟踪、远端 TLB 刷新配合，以及 `MADV_DONTNEED` 的 private-anonymous 零化语义。 | `mmap`、COW、动态 ELF 映射和 Rust allocator 路径直接相关；`mremap` 主要为更复杂构建准备。 |
| `os/src/mm/mod.rs` | 在诊断 feature 下重新导出帧监视函数。 | 仅诊断用途。 |
| `os/src/net/mod.rs` | 把网络 socket 创建后的 fd 安装改为共享文件表入口，避免 `CLONE_FILES` 子线程看到不一致的 fd。 | 保证 socket 类对象与普通 fd 使用同一套线程共享语义。 |
| `os/src/net/syscall.rs` | 去除按 TGID best-effort 复制 fd 的旧实现，统一使用 `install_fd_shared()`；增加 AF_UNIX 的 `sendto`、`recvfrom`、`sendmsg`、`recvmsg` 分支，保留 TCP/UDP 原路径。 | Rust 的 CLOEXEC 控制 socket 通过 `recvfrom` 直接依赖该修复。 |
| `os/src/sync/lock.rs` | 增加 `FilesLock` 锁类，并把它安排在 `TaskLock` 之前，更新 lockdep 的全局锁顺序。 | 防止共享 fd 表更新、任务登记和文件关闭之间出现锁顺序反转。 |
| `os/src/sync/mod.rs` | 重新导出 `FilesLock` 及其 guard。 | 为任务和 syscall 代码使用统一锁域提供接口。 |
| `os/src/syscall/fs.rs` | 实现共享 fd 的 install/close/dup/dup3/CLOEXEC 修改；关闭文件时先释放 `FilesLock` 再回调 `close_ref()`；接入 `eventfd2`、`epoll_create1/ctl/pwait`、`socketpair`；补齐 `/proc/self/exe` 和 `/proc/<pid>/exe` 的 readlink/open/stat；将 open、pipe、socket、accept、fcntl、close-range 等入口改到统一共享表逻辑。 | Q2.1 的 socketpair、exec 后 CLOEXEC、cargo 子进程 fd 生命周期直接依赖；`/proc/*/exe` 和 epoll 为更广工具链兼容准备。 |
| `os/src/syscall/mod.rs` | 增加/修正 RISC-V ABI 分派：`sigaltstack(132)`、`mremap(216)`、`eventfd2(19)`、`epoll_ctl(21)`、`epoll_pwait(22)`、`socketpair(199)`，并接入 AF_UNIX 收发路径。`rseq` 和 `riscv_hwprobe` 仍明确返回 `ENOSYS`。 | 避免工具链系统调用误派到错误号或旧的 `ENOSYS` stub；未支持接口不伪装成功。 |
| `os/src/syscall/process.rs` | `execve` 区分 `ENOENT` 与 `ENOEXEC`、记录可执行路径、支持脚本解释器和 `PT_INTERP` 的失败传播；多线程 `execve` 前收敛 sibling；`mmap` 检查长度/偏移溢出并同步 shared VM；`mremap` 对未实现的 `FIXED`/`DONTUNMAP` 明确返回 `EOPNOTSUPP`；增加 `MADV_DONTNEED` 入口和必要信号、futex、waitpid 诊断。 | `execve`、动态链接和 cargo 的 PATH 搜索直接依赖；共享 VM、mremap、futex 修复主要提高多线程构建稳定性。 |
| `os/src/task/manager.rs` | 在任务进入 PID registry 前注册 file-table group。 | 固定 `FilesLock -> PID registry` 的反向查询风险。 |
| `os/src/task/mod.rs` | 定义 `FileTableState/FileTableGroup` 与 `FILE_TABLE_LOCK`；稳定枚举同线程组任务；`exit_group` 和 `execve` 终止 sibling 时使用可靠快照、唤醒阻塞任务并请求目标 hart 重调度。 | cargo/rustc 是多线程程序，线程组退出、exec 与共享 fd 的正确性直接影响是否卡死。 |
| `os/src/task/task.rs` | TCB 增加 `files_id`、`executable_path`、`terminating_for_exec`、`terminating_for_group`；clone 根据 `CLONE_FILES` 保持或复制 fd 表；exec 时关闭 CLOEXEC 描述符并更换信号处置；提供共享 fd 初始化和可执行路径更新接口。 | 提供 Linux thread/process 基础状态，直接承载 Q2.1 路径。 |

新增源码模块如下：

| 新文件 | 实现范围 | 状态 |
| --- | --- | --- |
| `os/src/fs/unix_socket.rs` | AF_UNIX `SOCK_STREAM`/`SOCK_SEQPACKET` socketpair 的双向队列、阻塞/非阻塞、EOF、`SIGPIPE`、`MSG_PEEK`、`MSG_DONTWAIT`、`MSG_NOSIGNAL`、poll 与引用计数语义。 | Q2.1 已由 Rust CLOEXEC 控制通道实际使用并验证。 |
| `os/src/fs/eventfd.rs` | 64 位计数器、semaphore 模式、`EAGAIN`、阻塞唤醒、poll 和动态 `O_NONBLOCK`。 | 未作为 Q2.1 成功条件单独验证。 |
| `os/src/fs/epoll.rs` | fd interest 持有引用、`ADD/DEL/MOD`、level/edge/oneshot 的基础就绪计算与最大 watch 数限制。 | 未作为 Q2.1 成功条件单独验证。 |

`kernel-rv-libcbench-codeword` 是当前工作树中的未跟踪内核产物，不是源码，未列入上述
实现清单，也不应被提交。`tmp/run_buildstorm_q21_rv.sh` 和 `tmp/*.log` 是被 `.gitignore`
排除的本地复现证据，同样不属于提交内容。

## 16. 提交前的验证责任与当前状态

本文是当前本地工作树的工程记录，不代表这些修改已经 commit 或 push；截至本文写入时，
新增文档与上述源码差异均处于本地未提交状态。已完成的验证只包括：RISC-V64、四核、
官方 Docker 镜像中的 Q2.1 真实工具链加载和最小 cargo 构建。它证明这些变化的组合能
跑通 Q2.1，但不等价于每个新接口都有单元测试，也不替代初赛回归、LoongArch 回归和
Q2.2 全量 ArceOS 构建。

在准备提交前，至少应分别执行 RISC-V 和 LoongArch 的内核构建、初赛测例回归、Q2.1
官方脚本原样运行，并在资源足够的环境执行 Q2.2 的无计时预构建。若其中出现新错误，
应首先记录真实 syscall/文件系统/调度根因，再按 Linux 可观察语义修复，不能依靠脚本
改时限、过滤输出或伪造结果标记取得分数。
