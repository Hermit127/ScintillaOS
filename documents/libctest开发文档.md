# libctest 相关操作系统开发说明

本文记录为了运行并稳定通过官方 `libctest` 测试，我们对内核和少量用户态 runner 做过的主要开发。这里的目标不是绕过测试输出，而是补齐 libc 程序在 Linux ABI 下真实依赖的系统语义，让 musl/glibc 的 libc-test 用例可以正常启动、执行、退出并被评测脚本识别。

## 测试入口与评分格式

官方评测环境中的 `libctest` 位于测试盘目录下，当前 runner 会进入对应目录后输出：

```text
#### OS COMP TEST GROUP START libctest ####
...
#### OS COMP TEST GROUP END libctest ####
```

每个测试项通过 `runtest.exe -w entry-static.exe <case>` 启动。`contest_runner` 中覆盖的典型 case 包括：

- 启动与环境：`argv`、`env`、`tls_align`、`setjmp`
- 文件与 stdio：`fdopen`、`fscanf`、`fwscanf`、`ungetc`、`stat`、`utime`
- 时间与 locale：`clock_gettime`、`time`、`strftime`、`strptime`、`clocale_mbfuncs`
- 字符串与数值转换：`string_*`、`strtod*`、`strtof`、`strtol`、`strtold`、`snprintf`、`swprintf`
- 线程与同步：`pthread_cancel_points`、`pthread_cancel`、`pthread_cond`、`pthread_tsd`
- 搜索、排序、路径和网络辅助：`qsort`、`search_*`、`basename`、`dirname`、`inet_pton`、`socket`

虽然很多 case 的主体逻辑在 libc 用户态完成，但它们对内核 ABI 的要求很细：进程启动栈、文件描述符、信号、futex、时间、mmap、线程退出等任何一个细节不对，都可能表现为 `Timed out`、静默卡死或 libc 初始化失败。

## ELF 加载、exec 与进程启动 ABI

`argv`、`env`、`tls_align` 以及大部分 libc 初始化路径都依赖正确的 exec 语义。为此我们补齐了：

- 解析 ELF `PT_LOAD` 时按段虚拟地址正确拷贝数据，支持非页对齐 load segment。
- 解析 `PT_TLS`，为 libc TLS 初始化提供 `tls_base`、`tls_size` 和对齐信息。
- 解析 `PT_INTERP`，为后续运行动态链接 musl/glibc 程序打基础。
- 构造用户栈上的 `argc`、`argv`、`envp`、`auxv`，补充 `AT_PHDR`、`AT_PHENT`、`AT_PHNUM`、`AT_ENTRY`、`AT_PAGESZ`、`AT_EXECFN`、`AT_RANDOM` 等 libc 常用 aux vector。
- `exec` 后清理旧地址空间、重建 trap context、cwd/root/fd 表继承关系保持 Linux 兼容。
- 支持脚本执行 fallback：遇到 `#!` 脚本时通过 `/bin/sh` 或 busybox shell 重新构造参数启动。

这些改动解决的是 libc 进入 `_start` 和 `__libc_start_main` 前后的基础 ABI 问题。如果这里不完整，`libctest` 通常不是某个 case 失败，而是所有 case 都无法稳定启动。

## 内存管理、mmap 与用户缓冲区访问

`libctest` 里大量 stdio、字符串、locale、pthread 和 malloc 路径都会触发 `brk`、`mmap`、`munmap`、`mprotect` 和 lazy page fault。相关开发包括：

- 实现 `brk` 扩缩堆区，满足 libc malloc 小对象分配。
- 实现匿名 `mmap`、文件 `mmap`、`MAP_PRIVATE`、`MAP_SHARED`、`MAP_FIXED`、offset 对齐检查和权限检查。
- 实现 `munmap` 的部分区间拆分/回收，避免释放一个映射时破坏相邻区域。
- 实现 `mprotect` 的基础权限变更，满足 libc 对栈、TLS、只读段和测试页保护的需求。
- 支持 copy-on-write fork，降低 glibc 程序 fork 后的物理页压力。
- 在内核读写用户缓冲区前检查地址合法性，并在必要时处理 lazy page fault，避免退出路径、futex 路径访问未落页地址失败。
- 为共享地址空间线程补充 page table/MemorySet 元数据同步，避免一个线程 `mmap` 或缺页后另一个线程看不到映射。

这些语义直接影响 `memstream`、`fdopen`、`fscanf`、`pthread_*`、`tls_align` 以及部分字符串/浮点转换 case 的稳定性。

## 文件描述符、stdio 与 VFS 语义

`fdopen`、`fscanf`、`fwscanf`、`ungetc`、`stat`、`utime` 等 case 对 fd 行为非常敏感。我们补充或修复了：

- `openat`、`close`、`read`、`write`、`pread64`、`pwrite64`、`readv`、`writev`、`lseek` 的基础 Linux 语义。
- `fstat`、`newfstatat`、`statx`、`readlinkat`、`getdents64`，并正确填写 `Kstat` 中的文件类型、大小、权限、时间和设备字段。
- `dup`、`dup3`、`fcntl(F_DUPFD/F_GETFD/F_SETFD/F_GETFL/F_SETFL)`，支持 libc 对 fd flags 和 close-on-exec 的管理。
- 路径解析支持绝对/相对路径、`.`、`..`、cwd/root、挂载点和目录 dentry cache。
- `mkdirat`、`unlinkat`、`renameat2`、`linkat`、`symlinkat`、`faccessat` 等基础文件系统操作。
- `utimensat`/`utime` 路径，满足 `utime` case 对文件时间戳的修改和读取。
- 管道读写端引用计数、阻塞/唤醒和 EOF 行为，避免 stdio 或 shell 管道场景卡住。
- stdout/stderr 作为字符设备的 `stat` 与写入行为，避免 libc 对标准 fd 做检查时误判。

我们还修过一个和后续 benchmark 相关但也会影响输出稳定性的 stdout owner 问题：如果拥有 stdout 输出锁的进程已经退出，新的进程会主动清理 stale owner，避免整个测试链路卡在打印输出上。

## 时间、定时器和基础系统信息

`clock_gettime`、`time`、`strftime`、`strptime`、`pthread_cancel_points` 等 case 依赖时间 syscall 和睡眠语义。相关工作包括：

- `clock_gettime` 支持 libc 常用 clock id，并返回单调递增时间。
- `clock_getres` 返回合理精度。
- `gettimeofday`、`times`、`uname`、`getrusage`、`sysinfo` 等基础信息 syscall 提供兼容实现。
- `nanosleep` 支持阻塞当前任务、被信号打断时返回剩余时间。
- `setitimer`/`alarm` 提供基础计时器语义，满足部分 libc 和测试框架路径。

这些改动让时间相关 case 不再因为 syscall 缺失或时间不前进而卡死。

## 线程、clone、futex 与 pthread 语义

`pthread_cancel_points`、`pthread_cancel`、`pthread_cond`、`pthread_tsd` 是 `libctest` 中最容易从“普通 syscall 缺失”升级成“整条链路卡死”的部分。我们主要补齐了：

- `clone` 支持 `CLONE_VM`、`CLONE_THREAD`、`CLONE_SETTLS`、`CLONE_CHILD_SETTID`、`CLONE_CHILD_CLEARTID` 等 libc pthread 依赖的 flag。
- 线程组内共享地址空间、文件表、信号处理表或按 flag 拷贝对应资源。
- `set_tid_address` 记录 `clear_child_tid`，线程退出时先把用户态 tid 地址清 0，再唤醒对应 futex。
- `futex` 支持 `FUTEX_WAIT`、`FUTEX_WAKE`、private/shared key、超时等待和唤醒队列。
- shared futex 同时考虑虚拟地址和物理页 key，避免共享映射或线程共享页表下 key 对不上。
- `set_robust_list`、`get_robust_list` 支持 pthread robust mutex。
- 线程退出时扫描 robust list，把 owner died 状态写回用户态 futex，并唤醒等待者。
- 针对 musl robust list 附近的 killlock 状态做清理，避免退出后留下 `0x80000001` 这类锁状态导致等待端误判。
- `exit`、`exit_group`、`waitpid`、`waitid`、父子关系回收、僵尸任务保留退出码等语义修复。

这一组改动是 `libctest` 从“偶尔 Pass、偶尔 timeout”变成稳定通过的关键。特别是 robust futex 和 `clear_child_tid`，如果不按 Linux 语义处理，pthread 测试经常会在 join、cond wait 或 cancel cleanup 阶段永久等待。

## 信号与取消点

pthread cancel 在 libc 内部通常会走信号、取消点和阻塞 syscall 中断路径。为此我们做了：

- `rt_sigaction`、`rt_sigprocmask`、`rt_sigreturn` 的基础实现。
- `kill`、`tkill`、`tgkill`、`SIGCHLD` 唤醒父进程等待路径。
- 阻塞在 `nanosleep`、`waitpid`、pipe、futex 等位置的任务收到可中断信号后能够被唤醒。
- wait status 按 Linux 形式编码，区分正常退出和信号退出。
- 为 syscall 返回路径保存/恢复 trap context，避免 signal handler 返回后寄存器状态破坏。

这些语义帮助 `pthread_cancel` 和 `pthread_cancel_points` 正确从取消点退出，而不是把取消请求吞掉或让目标线程永远睡眠。

## socket 与网络相关 libc 接口

`inet_pton` 大多是用户态解析，但 `socket` case 会触发内核 socket syscall。我们为它补充了：

- `socket`、`bind`、`connect`、`listen`、`accept4`、`sendto`、`recvfrom`、`shutdown` 等基础接口。
- `getsockname`、`getpeername`、`setsockopt`、`getsockopt` 的兼容返回。
- `socketpair` 的基础实现或兼容路径，满足 libc 自测中的本地通信需求。
- socket fd 纳入统一 fd table，支持 `poll`/`ppoll`/`select` 的基本就绪判断。

对 `libctest` 来说，这部分主要保证 `socket` case 不因为 syscall 缺失直接失败；对后续 `netperf`、`iperf` 则是进一步扩展的基础。

## 随机数、locale、宽字符和数学类 case

`random`、`clocale_mbfuncs`、`mbc`、`iconv_open`、`wcsstr`、`wcstol`、`tgmath`、字符串和浮点转换类 case 主要逻辑在 libc 内部。它们对内核的核心要求是：

- 程序能正确加载并拥有完整用户栈、TLS 和 auxv。
- malloc/mmap/brk 工作稳定。
- 文件和 locale 数据访问不会因为路径、stat 或 read 语义错误失败。
- 浮点寄存器和普通寄存器在 trap/syscall/signal 返回时不被破坏。

因此这些 case 的修复往往不是给某个 libc 函数写特殊分支，而是修底层 ABI、内存和上下文保存。

## 已知经验

- `libctest` 的失败日志里大量 `Timed out` 不一定代表测试本身慢，更多时候是 futex、pipe、waitpid 或 signal 唤醒路径丢了。
- 只返回 `ENOSYS` 有时可以让 libc fallback，但对 pthread、futex、mmap、stat、clock 等核心路径不可行，必须实现真实语义。
- musl 和 glibc 的触发路径不完全一样：musl 更容易暴露 robust futex/killlock，glibc 更容易暴露 auxv、TLS、动态链接、设备 stat 和 COW 内存压力。
- `libctest` 通过并不等于 LTP 一定通过，但它能很好地覆盖 libc ABI 的“启动、文件、时间、线程、信号、内存”主路径，是后续跑 LTP 和 benchmark 的基础。

## 代码位置速查

- 测试入口：`user/src/bin/contest_runner.rs`
- syscall 分发：`os/src/syscall/mod.rs`
- 进程、线程、futex、信号、时间、mmap：`os/src/syscall/process.rs`
- 文件 syscall：`os/src/syscall/fs.rs`
- 任务生命周期与调度唤醒：`os/src/task/mod.rs`、`os/src/task/task.rs`
- 地址空间与 ELF 加载：`os/src/mm/memory_set.rs`
- VFS 与 ext4 inode：`os/src/fs/vfs.rs`、`os/src/fs/inode.rs`
- 管道与 stdio：`os/src/fs/pipe.rs`、`os/src/fs/stdio.rs`
