# LTP 测试集成与运行指南

## 未实现的 LTP 所需系统调用清单（按优先级排列）

> 内核目前注册了约 65 个系统调用，其余均返回 -38 (ENOSYS)。
> 下面列出 LTP `runtest/syscalls` (1799 测试/366个syscall目录) 中**当前未实现但 LTP 会调用**的系统调用。

### Tier 1 — 必须实现（LTP 核心测试依赖，约 40 个）

| # | syscall | 对应 LTP 目录 | 说明 |
|---|---------|--------------|------|
| 1 | `fchmod` / `fchmodat` | `chmod/`, `fchmod/`, `fchmodat/` | 修改文件权限（LTP: chmod01~07, fchmod01~06, fchmodat01~02） |
| 2 | `fchown` / `fchownat` / `lchown` | `chown/`, `fchown/`, `fchownat/`, `lchown/` | 修改文件所有者（LTP: chown01~05, fchown01~05 等） |
| 3 | `poll` | `poll/` | I/O 多路复用（LTP: poll01~02），ppoll 已实现但 poll 独立编号 |
| 4 | `select` (_newselect) | `select/` | I/O 多路复用（LTP: select01~04），pselect6 已实现但 select 独立编号 |
| 5 | `pread64` / `pwrite64` | `pread/`, `pwrite/` | 带偏移量的读写（LTP: pread01~03, pwrite01~04） |
| 6 | `preadv` / `pwritev` / `preadv2` / `pwritev2` | `preadv/`, `preadv2/`, `pwritev/`, `pwritev2/` | 带偏移量的向量读写（LTP: preadv01~02, pwritev01~02） |
| 7 | `msync` | `msync/` | mmap 内存同步到磁盘（LTP: msync01~04） |
| 8 | `fsync` / `fdatasync` | `fsync/`, `fdatasync/` | 文件同步到磁盘（LTP: fsync01~04, fdatasync01~03） |
| 9 | `sync` / `syncfs` | `sync/`, `syncfs/` | 全系统/单文件系统同步（LTP: sync01, syncfs01） |
| 10 | `fallocate` | `fallocate/` | 预分配文件空间（LTP: fallocate01~06） |
| 11 | `posix_fadvise` / `fadvise64` | `fadvise/` | 文件访问建议（LTP: fadvise01~04） |
| 12 | `mremap` | `mremap/` | 重映射虚拟内存区域（LTP: mremap01~05） |
| 13 | `mincore` | `mincore/` | 检查页是否在物理内存（LTP: mincore01~02） |
| 14 | `mlock` / `mlock2` / `mlockall` / `munlock` / `munlockall` | `mlock/`, `mlock2/`, `mlockall/`, `munlock/`, `munlockall/` | 锁定/解锁内存（LTP: mlock01~04, mlockall01~03 等） |
| 15 | `getrlimit` / `setrlimit` | `getrlimit/`, `setrlimit/` | 进程资源限制（LTP: getrlimit01~02, setrlimit01~03） |
| 16 | `sysinfo` | `sysinfo/` | 系统信息（LTP: sysinfo01~02），**当前 stub 返回 -38** |
| 17 | `sigaltstack` | `sigaltstack/` | 信号交替栈（LTP: sigaltstack01~02） |
| 18 | `sigsuspend` / `rt_sigsuspend` | `sigsuspend/`, `rt_sigsuspend/` | 等待信号（LTP: sigsuspend01, rt_sigsuspend01） |
| 19 | `sigpending` / `rt_sigpending` | `sigpending/` | 检查未决信号（LTP: sigpending01~02） |
| 20 | `alarm` | `alarm/` | SIGALRM 定时器（LTP: alarm02~07） |
| 21 | `pause` | `pause/` | 等待信号（LTP: pause01~03） |
| 22 | `clock_getres` | `clock_getres/` | 获取时钟分辨率（LTP: clock_getres01） |
| 23 | `getitimer` / `setitimer` | `getitimer/`, `setitimer/` | 间隔定时器（LTP: getitimer01~03, setitimer01~03） |
| 24 | `prctl` | `prctl/` | 进程控制（LTP: prctl01~04），**当前 stub 返回 0** |
| 25 | `getgroups` / `setgroups` | `getgroups/`, `setgroups/` | 获取/设置附加组（LTP: getgroups01~03, setgroups01~04） |
| 26 | `getresuid` / `getresgid` / `setresuid` / `setresgid` / `setreuid` / `setregid` | `getresuid/`, `getresgid/`, `setresuid/`, `setresgid/`, `setreuid/`, `setregid/` | 真实/有效/保留 UID/GID（LTP: 各 01~03） |
| 27 | `epoll_create1` / `epoll_ctl` / `epoll_wait` / `epoll_pwait` | `epoll_create1/`, `epoll/`, `epoll_ctl/`, `epoll_wait/`, `epoll_pwait/` | 事件轮询（LTP: epoll01~03 等），**当前 epoll_create1 stub 返回 -38** |
| 28 | `eventfd` / `eventfd2` | `eventfd/`, `eventfd2/` | 事件文件描述符（LTP: eventfd01~02），**当前 eventfd2 stub 返回 -38** |
| 29 | `flock` | `flock/` | 文件锁（LTP: flock01~06） |
| 30 | `creat` | `creat/` | 创建文件（LTP: creat01~05），旧接口，可用 openat 模拟 |
| 31 | `futimesat` | `futimesat/` | 设置文件时间（LTP: futimesat01），旧接口 |
| 32 | `fpathconf` / `pathconf` | `fpathconf/`, `pathconf/` | 获取文件/路径配置（LTP: fpathconf01, pathconf01~02） |
| 33 | `sched_setaffinity` / `sched_getaffinity` | `sched_setaffinity/`, `sched_getaffinity/` | CPU 亲和性（LTP: sched_setaffinity01），**当前 getaffinity stub 返回 -38** |
| 34 | `nice` / `setpriority` / `getpriority` | `nice/`, `setpriority/`, `getpriority/` | 进程优先级（LTP: nice01~05, setpriority01~02 等） |
| 35 | `mknod` / `mknodat` | `mknod/`, `mknodat/` | 创建设备节点（LTP: mknod01~09, mknodat01~03） |
| 36 | `socketpair` | `socketpair/` | 创建已连接套接字对（LTP: socketpair01~02） |
| 37 | `readahead` | `readahead/` | 预读文件（LTP: readahead01~02） |
| 38 | `copy_file_range` | `copy_file_range/` | 内核态文件拷贝（LTP: copy_file_range01~03） |
| 39 | `memfd_create` | `memfd_create/` | 匿名内存文件（LTP: memfd_create01~04） |
| 40 | `close_range` | `close_range/` | 批量关闭文件描述符（LTP: close_range01~02） |
| 41 | `stat` / `lstat` | `stat/`, `lstat/` | 旧版 stat/lstat（LTP: stat01~06, lstat01~03），需用 fstatat 模拟 |
| 42 | `getdents` | `getdents/` | 旧版读取目录项（LTP: getdents01~02），需用 getdents64 模拟 |

### Tier 2 — 应该实现（提升 LTP 通过率，约 25 个）

| # | syscall | 对应 LTP 目录 | 说明 |
|---|---------|--------------|------|
| 1 | `signalfd4` / `signalfd` | `signalfd/`, `signalfd4/` | 信号文件描述符（LTP: signalfd01, signalfd4_01） |
| 2 | `timerfd_create` / `timerfd_settime` / `timerfd_gettime` | `timerfd/` | 定时器文件描述符（LTP: timerfd01~02） |
| 3 | `timer_create` / `timer_delete` / `timer_getoverrun` / `timer_gettime` / `timer_settime` | `timer_create/`, `timer_delete/`, `timer_getoverrun/`, `timer_gettime/`, `timer_settime/` | POSIX 定时器（LTP: 各 01~03），`timer_create` 在 Makefile 中已过滤 |
| 4 | `clock_nanosleep` | `clock_nanosleep/` | 按时钟睡眠（LTP: clock_nanosleep01~03） |
| 5 | `clock_settime` / `settimeofday` | `clock_settime/`, `settimeofday/` | 设置系统时间（LTP: clock_settime01~03） |
| 6 | `adjtimex` | `adjtimex/` | 调整内核时钟（LTP: adjtimex01~03） |
| 7 | `rt_sigqueueinfo` / `rt_tgsigqueueinfo` | `rt_sigqueueinfo/`, `rt_tgsigqueueinfo/` | 携带数据的信号发送（LTP: rt_sigqueueinfo01~02），tgsigqueueinfo 已在 Makefile 过滤 |
| 8 | `sigtimedwait` / `sigwaitinfo` / `sigwait` | `sigtimedwait/`, `sigwaitinfo/`, `sigwait/` | 同步等待信号（LTP: sigtimedwait01, sigwait01~02） |
| 9 | `sched_getscheduler` / `sched_setscheduler` / `sched_getparam` / `sched_setparam` | `sched_getscheduler/`, `sched_setscheduler/`, `sched_getparam/`, `sched_setparam/` | 调度策略（LTP: 各 01~02） |
| 10 | `sched_get_priority_max` / `sched_get_priority_min` / `sched_rr_get_interval` | `sched_get_priority_max/`, `sched_get_priority_min/`, `sched_rr_get_interval/` | 调度参数范围（LTP: 各 01） |
| 11 | `sched_getattr` / `sched_setattr` | `sched_getattr/`, `sched_setattr/` | 扩展调度属性（LTP: sched_getattr01~02） |
| 12 | `vmsplice` / `splice` / `tee` | `vmsplice/`, `splice/`, `tee/` | 管道零拷贝操作（LTP: splice01~02, tee01~02, vmsplice01~02） |
| 13 | `recvmmsg` / `sendmmsg` | `recvmmsg/`, `sendmmsg/` | 批量收发消息（LTP: recvmmsg01, sendmmsg01~02） |
| 14 | `recv` / `send` | `recv/`, `send/` | 旧版 socket send/recv（LTP: recv01, send01），sendto/recvfrom 已实现 |
| 15 | `getdomainname` / `setdomainname` | `getdomainname/`, `setdomainname/` | NIS 域名（LTP: getdomainname01, setdomainname01） |
| 16 | `gethostname` / `sethostname` | `gethostname/`, `sethostname/` | 主机名（LTP: gethostname01, sethostname01~03） |
| 17 | `getsid` / `setsid` | `getsid/`, `setsid/` | 会话 ID（LTP: getsid01~02, setsid01~02） |
| 18 | `getpgrp` / `setpgrp` | `getpgrp/`, `setpgrp/` | 进程组（LTP: getpgrp01, setpgrp01~02） |
| 19 | `getpgid` / `setpgid` | `getpgid/`, `setpgid/` | 进程组 ID（LTP: getpgid01~02, setpgid01~03），**当前 stub 返回 0** |
| 20 | `chroot` | `chroot/` | 改变根目录（LTP: chroot01~04） |
| 21 | `faccessat2` | `faccessat2/` | 带 flags 的文件访问检查（LTP: faccessat201~02） |
| 22 | `openat2` | `openat2/` | 扩展 openat（LTP: openat201~03） |
| 23 | `sfork` / `fork` | `fork/` | `fork` 已通过 `clone` 实现 (syscall 401) |
| 24 | `sync_file_range` | `sync_file_range/` | 指定范围同步（LTP: sync_file_range01） |
| 25 | `reboot` | `reboot/` | 重启系统（LTP: reboot01~02） |

### Tier 3 — 低优先级/可跳过（约 60+ 个）

这些 syscall 在 LTP 中有测试目录，但对当前内核开发阶段影响较小：
- **NUMA**: `get_mempolicy`, `set_mempolicy`, `mbind`, `migrate_pages`, `move_pages`
- **密钥管理**: `add_key`, `request_key`, `keyctl`
- **BPF**: `bpf` (bpf_map01, bpf_prog01~07)
- **AIO**: `io_setup`, `io_destroy`, `io_submit`, `io_cancel`, `io_getevents`, `io_pgetevents`, `io_uring`
- **inotify**: `inotify_init`, `inotify`
- **fanotify**: `fanotify`
- **命名空间**: `unshare`, `setns`, `pivot_root`, `clone3`
- **挂载新 API**: `fsconfig`, `fsmount`, `fsopen`, `fspick`, `open_tree`, `move_mount`, `mount_setattr`
- **swap**: `swapon`, `swapoff`
- **capability**: `capget`, `capset`（可简单 stub）
- **扩展属性**: `getxattr`, `setxattr`, `lgetxattr`, `fgetxattr`, `listxattr`, `llistxattr`, `flistxattr`, `removexattr`, `lremovexattr`, `fremovexattr`
- **pidfd**: `pidfd_open`, `pidfd_getfd`, `pidfd_send_signal`
- **ptrace**: `ptrace`
- **perf**: `perf_event_open`
- **quota**: `quotactl`
- **userfaultfd**: `userfaultfd`
- **kcmp**: `kcmp`
- **ioprio**: `ioprio`
- **name_to_handle_at** / **open_by_handle_at**
- **process_madvise**
- **pkeys**: `pkeys`
- **membarrier**: `membarrier`
- **get_robust_list** / **set_robust_list**: `get_robust_list/`, `set_robust_list/`（`set_robust_list` 当前 stub 返回 0）
- **modify_ldt**, **ioperm**, **iopl**, **cacheflush**, **switch** (架构相关)
- **arch_prctl** (x86)
- **socketcall** (已废弃，x86)
- **tkill** / **tgkill** (旧信号接口，kill 已实现)
- **ustat**, **ulimit**, **stime**, **sgetmask**, **ssetmask** (已废弃)
- **ipc** (System V IPC: `msgget`, `msgsnd`, `msgrcv`, `semget`, `semop`, `semctl`, `shmat`, `shmdt`, `shmget`, `shmctl`) — LTP: `ipc/`
- **mq_open**, **mq_unlink**, **mq_timedsend**, **mq_timedreceive**, **mq_notify** (POSIX 消息队列)
- **acct** (进程记账)
- **delete_module**, **init_module**, **finit_module** (内核模块)
- **cma**, **confstr**, **fmtmsg**, **profil**, **nftw**, **realpath** (libc 函数或已过滤)

### 统计

| 类别 | 数量 |
|------|------|
| Tier 1 — 必须实现 | ~42 个 syscall 族 |
| Tier 2 — 应该实现 | ~25 个 syscall 族 |
| Tier 3 — 可跳过 | ~60+ 个 syscall 族 |
| **合计未实现但 LTP 需要** | **~130 个 syscall 族** |
| 当前已实现（含 stub） | ~65 个 |

### 实现建议

1. **先从 Tier 1 中选取与当前已实现 syscall 相近的开始**
   - `fchmodat`/`fchownat` 与 `mkdirat` 同属 `*at` 族，文件系统代码路径类似
   - `fsync`/`fdatasync`/`sync` 可用 no-op 实现（节省时间）
   - `getrlimit`/`setrlimit` 可返回固定值
   - `sysinfo`（当前 stub）可填充一个基础结构体
   - `sigaltstack`/`sigpending`/`sigsuspend` 在现有信号框架上扩展

2. **epoll** 和 **eventfd** 是最复杂但最重要的两个
   - epoll 需要内核维护关注列表、就绪队列
   - 建议参考 `lwext4_rust` 的实现模式，先写一个最小可用版本

3. **网络相关 syscall 基础框架已就位**，`recvmmsg`/`sendmmsg` 在现有 `recvfrom`/`sendto` 基础上扩展即可

## 目录

- [1. 概述](#1-概述)
- [2. 当前项目状态](#2-当前项目状态)
- [3. Phase 0: 环境准备](#3-phase-0-环境准备)
- [4. Phase 1: 编译 LTP 二进制](#4-phase-1-编译-ltp-二进制)
- [5. Phase 2: 内核侧改动](#5-phase-2-内核侧改动)
- [6. Phase 3: 运行测试](#6-phase-3-运行测试)
- [7. Phase 4: 结果分析与迭代](#7-phase-4-结果分析与迭代)
- [8. 附录](#8-附录)

---

## 1. 概述

LTP (Linux Test Project) 是 Linux 内核系统调用的标准回归测试套件，位于 `testsuits-for-oskernel/ltp-full-20240524/`。本项目使用 LTP 对自研 OS 内核进行系统调用正确性验证。

### 运行架构

```
make run ARCH=riscv64
  → QEMU 启动内核
    → main() → add_initproc()
      → ltp_runner (fork + exec busybox sh /ltp_testcode.sh)
        → busybox sh 遍历 ltp/testcases/bin/* 逐个执行 LTP 测试二进制
          → 每个测试二进制 → syscall → 内核处理
```

### 链接模式

| 模式 | 环境变量 | 内核改动 | 适用场景 |
|------|---------|---------|---------|
| **静态链接** (推荐) | `LTP_STATIC=1` | 无，开箱即用 | 首次打通流程 |
| 动态链接 | 默认 | 需实现 PT_INTERP + 复制 musl .so | 比赛标准环境 |

### 双模式

| 模式 | 说明 | 测试量 | 预计时间 |
|------|------|--------|----------|
| **子集** | 精选 ~40 个核心 syscall 测试 | ~40 测试 | 1-2 分钟 |
| **全量** | LTP runtest/syscalls 全部 | ~1799 测试 | 30-60 分钟 |

> **建议**：首次先跑子集，逐个修复；子集全部通过后再跑全量。

---

## 2. 当前项目状态

### 2.1 系统调用实现概览

内核在 `os/src/syscall/mod.rs` 中注册了约 65 个系统调用，按实现程度分为三类：

**已实现的（约 55 个）**：
- 进程管理: `clone/fork`, `exec`, `waitpid`, `exit/exit_group`, `nanosleep`, `getpid/ppid/uid/euid/gid/egid/tid`
- 文件 IO: `openat`, `close`, `read`, `write`, `lseek`, `pread`*(readv)*, `sendfile`, `readv`, `writev`
- 文件元数据: `fstat`, `fstatat`, `statx`, `getdents64`, `readlinkat`, `utimensat`, `ftruncate`
- 目录操作: `mkdirat`, `unlinkat`, `chdir`, `getcwd`, `renameat2`, `symlinkat`, `linkat`*(faccessat)*
- 内存管理: `brk`, `mmap`, `munmap`, `mprotect`, `madvise`
- IPC/管道: `pipe2`, `dup/dup2/dup3`, `fcntl`, `ioctl`
- 信号: `kill`, `sigaction`, `sigprocmask`, `sigreturn`
- 网络: `socket`, `bind`, `listen`, `accept`, `connect`, `sendto`, `recvfrom`, `setsockopt`, `getsockopt`, `shutdown(sock)`, `sendmsg`, `recvmsg`, `getsockname`, `getpeername`
- 挂载: `mount`, `umount2`
- 时间: `gettimeofday`, `times`, `clock_gettime`
- 系统信息: `uname`, `yield`, `syslog`, `umask`, `setuid`, `setgid`
- 其他: `getrandom`, `pselect6`, `ppoll`, `statfs`, `fstatfs`, `futex`*(basic)*

**Stub 返回 -38 (ENOSYS) 的**：
- `sysinfo`, `sched_getaffinity`, `epoll_create1`, `eventfd2`, `rseq`, `riscv_hwprobe`

**Stub 返回 0 的**：
- `set_robust_list`, `setpgid`, `getpgid`, `prctl`, `prlimit64`

**未实现的（返回 -38 ENOSYS）**：
- 其他所有未在 match 分支中出现的 syscall，如 `writev`, `renameat`*(旧版本)*, `symlink`*(旧版本)*, `chmod`, `chown`, `access`, `poll`, `select`, `utimes`, `sync`, `fsync`, `flock`, `fallocate` 等

### 2.2 关键注意事项

1. **使用动态链接**：LTP 二进制使用 musl 动态链接（ELF interpreter: `/lib/ld-musl-riscv64.so.1`）。内核需要**新实现 PT_INTERP 处理**来加载动态链接器，并将 musl 的 `libc.so` 和 `ld-musl-riscv64.so.1` 放入磁盘镜像的 `/lib/` 目录。
2. **无 /proc 实现**：部分 LTP 测试依赖 `/proc/sys/kernel/ngroups_max` 等文件，会失败。
3. **无 /dev/null 完整支持**：`/dev/null` 目前为简单实现。
4. **内存限制**：riscv64 默认 256MB，LoongArch 默认 2GB。LTP 某些测试可能内存不足。
5. **单核**：QEMU 以 `-smp 1` 启动，不支持真正的并发/多线程测试。

---

## 3. Phase 0: 环境准备

### 3.1 Docker 环境

LTP 编译需要交叉编译器工具链。推荐使用比赛官方 Docker 镜像：

```bash
# 进入 docker 容器（在项目根目录）
cd /home/dyh/os26/oskernel2026test
cd testsuits-for-oskernel

# 方式A：使用 Makefile 自带的 docker 目标
make docker

# 方式B：手动启动
docker run --rm -it -v .:/code --entrypoint bash -w /code --privileged \
  docker.educg.net/cg/os-contest:20250614
```

Docker 镜像已预装:
- `riscv64-linux-musl-gcc` (GCC 11.2.1)
- `riscv64-linux-gnu-gcc` (GCC 11.4.0)
- `loongarch64-linux-musl-gcc` (GCC 13.2.0)
- `loongarch64-linux-gnu-gcc` (GCC 13.2.0)
- `riscv64-unknown-elf-gcc` (GCC 8.2.0)
- QEMU 9.2.1, Rust nightly-2025-01-18
- autoconf, automake, make, pkg-config 等构建工具

### 3.2 非 Docker 环境（本地交叉编译器）

如需在本地编译 LTP，需安装对应架构的 musl 交叉编译器：

```bash
# Ubuntu/Debian
sudo apt install gcc-riscv64-linux-musl

# Arch Linux
yay -S riscv64-linux-musl

# 或者从 musl.cc 下载预编译工具链
wget https://musl.cc/riscv64-linux-musl-cross.tgz
tar xzf riscv64-linux-musl-cross.tgz -C /opt
export PATH="/opt/riscv64-linux-musl-cross/bin:$PATH"
```

> **注意**：非 Docker 环境还需要 autoconf >= 2.64, automake >= 1.11, pkg-config

---

## 4. Phase 1: 编译 LTP 二进制

支持两种链接模式，通过 `LTP_STATIC` 环境变量切换。

| 模式 | 命令 | 优点 | 缺点 |
|------|------|------|------|
| **静态链接** (推荐首次使用) | `LTP_STATIC=1 make ...` | 无需实现 PT_INTERP，内核改动最小 | 二进制体积较大 |
| 动态链接 | `make ...` (默认) | 二进制体积小 | 需内核支持 PT_INTERP + 复制 musl .so |

**建议**：先跑通静态链接流程，验证 LTP 能正确执行；之后再实现 PT_INTERP 切换动态链接。

### 4.1 静态链接编译（推荐第一次使用）

```bash
# 在 Docker 容器内执行

# ====== 静态编译 LTP ======
make -f Makefile.sub clean
LTP_STATIC=1 make -f Makefile.sub PREFIX=riscv64-linux-musl- DESTDIR=/code/sdcard/riscv/musl ltp

# 产物: /code/sdcard/riscv/musl/ltp/testcases/bin/
# 编译时间: 10-20 分钟
```

**验证**：
```bash
file /code/sdcard/riscv/musl/ltp/testcases/bin/close01
# 输出: ... statically linked ...
```

### 4.2 动态链接编译

```bash
# 在 Docker 容器内执行（默认模式）

make -f Makefile.sub clean
make -f Makefile.sub PREFIX=riscv64-linux-musl- DESTDIR=/code/sdcard/riscv/musl ltp
```

**验证**：
```bash
file /code/sdcard/riscv/musl/ltp/testcases/bin/close01
# 输出: ... dynamically linked, interpreter /lib/ld-musl-riscv64.so.1 ...
```

### 4.3 非 Docker 构建

```bash
cd testsuits-for-oskernel

# 静态
LTP_STATIC=1 make -f Makefile.sub PREFIX=riscv64-linux-musl- DESTDIR=/code/sdcard/riscv/musl ltp

# 动态
make -f Makefile.sub PREFIX=riscv64-linux-musl- DESTDIR=/code/sdcard/riscv/musl ltp
```

### 4.4 只跑子集

全量编译后通过 `ltp_testcode_subset.sh` 控制实际运行哪些二进制，无需单独编译子集。

---

## 5. Phase 2: 内核侧改动

### 5.0 动态链接器支持（仅动态链接模式需要）

> **使用静态链接（LTP_STATIC=1）则跳过本节，直接进入 5.1。** 静态链接的 LTP 二进制内核可直接加载，无需任何额外改动。

LTP 动态链接二进制依赖 ELF 解释器 `/lib/ld-musl-riscv64.so.1`。

#### 5.0.1 原理

动态链接的可执行文件包含 `PT_INTERP` 段，内核需要：

1. 解析可执行文件 ELF 中的 `PT_INTERP`，获取解释器路径（如 `/lib/ld-musl-riscv64.so.1`）
2. 打开并加载解释器 ELF 的 `PT_LOAD` 段
3. 将**解释器的入口点**作为进程入口（而非可执行文件本身的入口）
4. 解释器启动后自行加载 `libc.so` 等共享库并跳转到可执行文件入口

#### 5.0.2 修改 `os/src/mm/memory_set.rs`

需要修改 `from_elf()` 函数，使其返回的 `entry_point` 为解释器的入口点：

```rust
// LoadedElf 增加字段
pub struct LoadedElf {
    pub memory_set: MemorySet,
    pub user_sp: usize,
    pub entry_point: usize,
    pub ph_addr: usize,
    pub ph_entsize: usize,
    pub ph_num: usize,
    pub interp_entry: Option<usize>,  // 新增：解释器的入口点
}

// from_elf 中新增 PT_INTERP 处理
pub fn from_elf(elf_data: &[u8]) -> LoadedElf {
    // ... 原有 PT_LOAD 加载逻辑保持不变 ...

    // 新增：检查 PT_INTERP
    let mut interp_path: Option<String> = None;
    for i in 0..ph_count {
        let ph = elf.program_header(i).unwrap();
        if ph.get_type().unwrap() == xmas_elf::program::Type::Interp {
            let start = ph.offset() as usize;
            let end = start + ph.file_size() as usize;
            interp_path = Some(
                core::str::from_utf8(&elf.input[start..end])
                    .unwrap()
                    .trim_end_matches('\0')
                    .to_string()
            );
        }
    }

    let interp_entry = if let Some(ipath) = interp_path {
        // 打开解释器文件，加载其 ELF
        let interp_inode = open_file(&ipath, OpenFlags::RDONLY)
            .expect("Failed to open ELF interpreter");
        let interp_data = interp_inode.read_all();
        let interp_elf = xmas_elf::ElfFile::new(&interp_data).unwrap();
        // 加载解释器的 PT_LOAD 段到同一个 memory_set
        let interp_load_addr = load_interp_segments(&mut memory_set, &interp_elf);
        Some(interp_elf.header.pt2.entry_point() as usize + interp_load_addr)
    } else {
        None
    };

    LoadedElf {
        memory_set,
        user_sp: user_stack_top,
        entry_point: interp_entry.unwrap_or(elf.header.pt2.entry_point() as usize),
        // ... 其余字段
    }
}
```

具体实现细节：
- 解释器 PT_LOAD 段的 `virtual_addr` 可能和主程序冲突，需要将其加载到空闲区域并做重定位偏移
- 可以将解释器加载到 `0x4000_0000_0000` 基址，主程序加载到更高地址
- 或者将所有 PT_LOAD 段合并到一个 `memory_set`，解释器段偏移 + 主程序段偏移互不重叠
- AUXV 中的 `AT_ENTRY` 设为解释器入口，`AT_BASE` 设解释器加载基址

#### 5.0.3 复制 musl 动态库到磁盘镜像

musl 的动态链接器和 libc 需要放在磁盘镜像的 `/lib/` 下。

**Docker 容器中 musl 库位置**：
```
# riscv64 musl 工具链的 sysroot
/opt/riscv64--musl--bleeding-edge-2020.08-1/riscv64-buildroot-linux-musl/sysroot/

lib/
├── ld-musl-riscv64.so.1    → musl 动态链接器/解释器
└── libc.so                 → musl C 库 (动态链接版本)
```

**Makefile `fs-img` 目标中添加**：

```makefile
# ====== musl 动态库（仅动态链接模式需要） ======
# 静态链接模式下不需要本节
ifneq ($(LTP_BIN_DIR),)
	@echo "===== Copying musl dynamic linker & libc (dynamic mode only) ====="
	@mkdir -p $(ROOTFS_DIR)/lib
	@cp /opt/riscv64--musl--bleeding-edge-2020.08-1/riscv64-buildroot-linux-musl/sysroot/lib/libc.so $(ROOTFS_DIR)/lib/
	@cp /opt/riscv64--musl--bleeding-edge-2020.08-1/riscv64-buildroot-linux-musl/sysroot/lib/ld-musl-riscv64.so.1 $(ROOTFS_DIR)/lib/
	@ln -sf libc.so $(ROOTFS_DIR)/lib/ld-musl-riscv64.so.1 2>/dev/null || true
endif
```

**非 Docker 环境**：musl 库位于交叉编译器安装目录下，如：
- `/opt/riscv64-linux-musl-cross/riscv64-linux-musl/lib/libc.so`
- `/opt/riscv64-linux-musl-cross/riscv64-linux-musl/lib/ld-musl-riscv64.so.1`

> **注意**：当前 `mke2fs` 分配 128MB（`count=128`），LTP 全量二进制 + musl .so 约需要额外 100-200MB，可能需要调整 `count=256` 或 `count=512`。

### 5.1 新建 `user/src/bin/ltp_runner.rs`

`ltp_runner` 替代 `initproc`，启动 busybox sh 执行 LTP 测试脚本。

```rust
#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::{exec, fork, wait, yield_};

#[no_mangle]
pub fn main() -> i32 {

    // 子集模式 vs 全量模式：只需改脚本名
    // 子集: "/ltp_testcode_subset.sh\0"
    // 全量: "/ltp_testcode.sh\0"
    let script = b"/ltp_testcode_subset.sh\0";

    if fork() == 0 {
        // argv: [busybox, sh, /ltp_testcode.sh, NULL]
        let args: [*const u8; 4] = [
            b"busybox\0".as_ptr(),
            b"sh\0".as_ptr(),
            script.as_ptr(),
            core::ptr::null::<u8>(),
        ];
        exec("busybox\0", &args);
        // exec 失败时 exit
        user_lib::exit(-1);
    } else {
        loop {
            let mut exit_code: i32 = 0;
            let pid = wait(&mut exit_code);
            if pid == -1 {
                yield_();
                continue;
            }
            // 所有子进程结束，关机
            user_lib::shutdown();
        }
    }
    0
}
```

> **注意**: `user_lib::shutdown()` 需要先确认是否存在，如不存在则使用 `exit(0)` 替代。

### 5.2 新建 `tmp/ltp_testcode.sh`（全量脚本）

```bash
#!/bin/sh

echo "#### OS COMP TEST GROUP START ltp-musl ####"

target_dir="ltp/testcases/bin"

if [ -d "$target_dir" ]; then
    for file in "$target_dir"/*; do
        if [ -f "$file" ] && [ -x "$file" ]; then
            name=$(basename "$file")
            echo "RUN LTP CASE $name"
            "$file"
            ret=$?
            echo "FINISH LTP CASE $name : $ret"
        fi
    done
else
    echo "LTP_DIR_NOT_FOUND: $target_dir"
fi

echo "#### OS COMP TEST GROUP END ltp-musl ####"
```

### 5.3 新建 `tmp/ltp_testcode_subset.sh`（子集脚本）

```bash
#!/bin/sh

echo "#### OS COMP TEST GROUP START ltp-subset-musl ####"

target_dir="ltp/testcases/bin"

if [ ! -d "$target_dir" ]; then
    echo "LTP_DIR_NOT_FOUND: $target_dir"
    echo "#### OS COMP TEST GROUP END ltp-subset-musl ####"
    exit 1
fi

# =============================================
# LTP 子集测试列表（约 40 个核心测试）
# =============================================
SUBSET="\
close01 close02 \
open01 open02 \
read01 read02 \
write01 write02 \
lseek01 lseek02 \
fstat01 fstat02 fstat03 \
dup01 dup02 dup201 dup202 \
pipe01 pipe02 \
brk01 brk02 \
mmap01 mmap02 \
munmap01 munmap02 \
fork01 fork02 fork03 \
exit_group01 \
wait01 wait02 \
getpid01 getpid02 \
getcwd01 getcwd02 \
chdir01 chdir04 \
times01 \
uname01 uname02 \
nanosleep01 \
kill01 kill02 \
gettimeofday01 gettimeofday02 \
umask01 \
mkdirat01 mkdirat02 \
rename01 rename02 \
unlink01 unlink02 \
mprotect01 mprotect02 \
fcntl01 fcntl02 \
access01 \
"

for name in $SUBSET; do
    file="$target_dir/$name"
    if [ -f "$file" ] && [ -x "$file" ]; then
        echo "RUN LTP CASE $name"
        "$file"
        ret=$?
        echo "FINISH LTP CASE $name : $ret"
    else
        echo "SKIP LTP CASE $name (not found)"
    fi
done

echo "#### OS COMP TEST GROUP END ltp-subset-musl ####"
```

### 5.4 修改根目录 `Makefile`

在 `Makefile` 的 `fs-img` 目标中添加 LTP 集成逻辑。

**新增变量**（放在 Makefile 顶部 `TEST ?=` 后面）：

```makefile
# LTP 二进制根目录（空 = 不集成 LTP）
LTP_BIN_DIR ?=
# LTP 模式: subset | full
LTP_MODE ?= subset
```

**在 `fs-img` 目标中，`mke2fs` 命令之前**添加：

```makefile
# ====== LTP 集成 ======
ifeq ($(LTP_MODE), subset)
	@cp -f tmp/ltp_testcode_subset.sh $(ROOTFS_DIR)/ltp_testcode.sh
else ifeq ($(LTP_MODE), full)
	@cp -f tmp/ltp_testcode.sh $(ROOTFS_DIR)/ltp_testcode.sh
endif

ifneq ($(LTP_BIN_DIR),)
	@echo "===== Copying LTP binaries ====="
	@mkdir -p $(ROOTFS_DIR)/ltp/testcases/bin
	@if [ -d "$(LTP_BIN_DIR)/ltp/testcases/bin" ]; then \
		for f in $(LTP_BIN_DIR)/ltp/testcases/bin/*; do \
			if [ -f "$$f" ] && [ -x "$$f" ]; then \
				cp "$$f" $(ROOTFS_DIR)/ltp/testcases/bin/; \
			fi; \
		done; \
		echo "LTP binaries copied: $$(ls $(ROOTFS_DIR)/ltp/testcases/bin/ | wc -l) files"; \
	else \
		echo "ERROR: LTP_BIN_DIR not found: $(LTP_BIN_DIR)/ltp/testcases/bin"; \
	fi
	@echo "===== Using ltp_runner as initproc ====="
	@cp target/$(TARGET)/release/ltp_runner $(ROOTFS_BIN_DIR)/initproc
	@echo "===== Copying musl dynamic linker & libc (dynamic mode only) ====="
	@mkdir -p $(ROOTFS_DIR)/lib
	@cp /opt/riscv64--musl--bleeding-edge-2020.08-1/riscv64-buildroot-linux-musl/sysroot/lib/libc.so $(ROOTFS_DIR)/lib/ 2>/dev/null || true
	@cp /opt/riscv64--musl--bleeding-edge-2020.08-1/riscv64-buildroot-linux-musl/sysroot/lib/ld-musl-riscv64.so.1 $(ROOTFS_DIR)/lib/ 2>/dev/null || true
endif
```

> **非 Docker 环境**：将 musl 库路径改为本地交叉编译器对应的位置。

### 5.5 修改 `user/src/bin/initproc.rs`（备选方案）

若不想新建文件，可直接修改 `initproc.rs`。此处以新建 `ltp_runner.rs` 为主方案。

---

## 6. Phase 3: 运行测试

### 6.1 构建流程总览

```
[Phase 0] 进入 Docker 环境
[Phase 1] 编译 LTP 二进制 → sdcard/riscv/musl/ltp/testcases/bin/
[Phase 2] 修改内核 → ltp_runner.rs + PT_INTERP 支持 + Makefile
[Phase 3] 构建内核磁盘镜像 → make fs-img
[Phase 4] 启动 QEMU 测试 → make run
```

### 6.2 子集运行

```bash
# Step 1: 设置环境变量（或在 Makefile 中硬编码）
export LTP_BIN_DIR=$(pwd)/testsuits-for-oskernel/sdcard/riscv/musl
unset LTP_MODE
export LTP_MODE=subset（这行可以不用，默认子集）

# Step 2: 构建内核磁盘镜像（包含 LTP + musl 动态库）
cd /home/dyh/os26/oskernel2026test
make fs-img ARCH=riscv64

# Step 3: 启动 QEMU
make run ARCH=riscv64
```

### 6.3 全量运行

```bash
export LTP_BIN_DIR=$(pwd)/testsuits-for-oskernel/sdcard/riscv/musl
export LTP_MODE=full
make fs-img ARCH=riscv64
make run ARCH=riscv64

export LTP_BIN_DIR=$(pwd)/testsuits-for-oskernel/sdcard/riscv/musl
export LTP_MODE=full
make fs-img ARCH=riscv64 && make run ARCH=riscv64
```

### 6.4 磁盘大小调整

如果 LTP 全量二进制超过 128MB，需要调整 `Makefile` 中的 `dd` 参数：

```makefile
# 原: bs=1M count=128
# 改: bs=1M count=512   # 512MB
@dd if=/dev/zero of=$(FS_IMG) bs=1M count=512
```

### 6.5 结合 init.sh 运行（双盘模拟比赛环境）

如果希望更接近比赛评测环境，可修改 `tmp/init.sh`：

```bash
#!/bin/sh
echo "===== OS BOOT ====="
busybox --install -s /bin 2>/dev/null

# 挂载测试盘
busybox mount -t ext4 /dev/vda2 /mnt 2>/dev/null

if [ -f /mnt/ltp_testcode.sh ]; then
    echo "===== RUNNING LTP FROM TEST DISK ====="
    busybox sh /mnt/ltp_testcode.sh
elif [ -f /ltp_testcode.sh ]; then
    echo "===== RUNNING LTP FROM ROOTFS ====="
    busybox sh /ltp_testcode.sh
fi

echo "===== ALL TESTS COMPLETE ====="
```

此时还需要修改 `ltp_runner.rs`，使用 `/etc/init.sh`（busybox init）作为启动入口。

---

## 7. Phase 4: 结果分析与迭代

### 7.1 识别输出格式

```
#### OS COMP TEST GROUP START ltp-subset-musl ####
RUN LTP CASE close01
FINISH LTP CASE close01 : 0          ← 返回 0 = 成功
RUN LTP CASE open01
FINISH LTP CASE open01 : -1          ← 返回 -1 = 失败 (EPERM)
RUN LTP CASE gettimeofday01
FINISH LTP CASE gettimeofday01 : -38 ← 返回 -38 = ENOSYS (未实现)
RUN LTP CASE foo99
FINISH LTP CASE foo99 : 139          ← 返回 139 = SIGSEGV (段错误/panic)
#### OS COMP TEST GROUP END ltp-subset-musl ####
```

### 7.2 错误码含义

| 返回值 | 含义 | 处理策略 |
|--------|------|----------|
| 0 | 测试通过 | 无需处理 |
| -1 ~ -38 | syscall 返回负 errno | 检查 syscall 实现逻辑 |
| 139 (SIGSEGV) | 内核 panic 或段错误 | 检查内存管理/页表 |
| 其他正数 | 进程被信号杀死 | 检查信号处理逻辑 |

### 7.3 定位具体失败的 syscall

每个 LTP 测试二进制对应一个源代码文件：
```
ltp_full/ltp/testcases/bin/close01
  ← ltp-full-20240524/testcases/kernel/syscalls/close/close01.c
```

查看 C 源码可了解该测试调用了哪些 syscall。

### 7.4 迭代流程

```
┌──────────────────────────────────────────┐
│ 1. 运行 LTP 子集                          │
│    → 收集 FAIL 的测试名和错误码            │
├──────────────────────────────────────────┤
│ 2. 按错误码分类                           │
│    → ENOSYS: 需要实现新 syscall           │
│    → EPERM/EINVAL: 实现有 bug            │
│    → SIGSEGV: 内存管理/边界检查问题        │
├──────────────────────────────────────────┤
│ 3. 优先处理最基础的 syscall                │
│    → 先修复 open/close/read/write 失败    │
│    → 再修复 fork/exec/wait 失败           │
│    → 最后处理权限/时间/网络等边缘功能       │
├──────────────────────────────────────────┤
│ 4. 修复后重新运行子集                      │
│    → git commit 记录修复内容              │
│    → 确认之前 FAIL 的测试变 PASS          │
├──────────────────────────────────────────┤
│ 5. 子集全部通过后 → 跑全量                │
│    → 按同样流程处理新增失败               │
└──────────────────────────────────────────┘
```

---

## 8. 附录

### 8.1 LTP 子集测试列表详细说明

| 测试二进制 | syscall | 说明 | 备注 |
|-----------|---------|------|------|
| `close01` | close | 关闭有效/无效 fd | |
| `close02` | close | 关闭已关闭的 fd | 应返回 EBADF |
| `open01` | open/openat | 打开文件，检查 fd | |
| `open02` | open/openat | 打开不存在的文件 | 应返回 ENOENT |
| `read01` | read | 读取文件内容 | 需配合 open |
| `read02` | read | 读取无效 fd | 应返回 EBADF |
| `write01` | write | 写入文件 | 需配合 open |
| `write02` | write | 写入只读 fd | 应返回 EBADF |
| `lseek01` | lseek | SEEK_SET/SEEK_CUR/SEEK_END | |
| `lseek02` | lseek | 无效参数/无效 fd | |
| `fstat01` | fstat | stat 普通文件 | |
| `fstat02` | fstat | stat 无效 fd | |
| `fstat03` | fstat | stat 目录 | |
| `dup01` | dup | 复制 fd | |
| `dup02` | dup | 复制无效 fd | |
| `dup201` | dup2 | dup2 到指定 fd | |
| `dup202` | dup2 | dup2 无效参数 | |
| `pipe01` | pipe/pipe2 | 创建管道，读写 | |
| `pipe02` | pipe/pipe2 | 管道满/空测试 | |
| `brk01` | brk | 扩展 brk | |
| `brk02` | brk | 缩减 brk | |
| `mmap01` | mmap | 匿名映射读写 | |
| `mmap02` | mmap | 文件映射 | |
| `munmap01` | munmap | munmap 后访问 | 应失败 |
| `munmap02` | munmap | munmap 未映射区域 | |
| `fork01` | fork/clone | 基本 fork | |
| `fork02` | fork/clone | fork 后 fd 共享 | |
| `fork03` | fork/clone | fork 内存隔离 | |
| `exit_group01` | exit_group | 进程退出 | |
| `wait01` | waitpid | 等待子进程退出 | |
| `wait02` | waitpid | 等待不存在的 pid | |
| `getpid01` | getpid | 获取进程 PID | |
| `getpid02` | getpid | 父子和 pid 比较 | |
| `getcwd01` | getcwd | 获取当前目录 | |
| `getcwd02` | getcwd | 小缓冲区测试 | |
| `chdir01` | chdir | 切换目录 | |
| `chdir04` | chdir | 切换到不存在的目录 | |
| `times01` | times | 获取进程时间 | |
| `uname01` | uname | 获取系统信息 | |
| `uname02` | uname | 小缓冲区测试 | |
| `nanosleep01` | nanosleep | 基本睡眠 | |
| `kill01` | kill | 发送信号 | |
| `kill02` | kill | 无效信号测试 | |
| `gettimeofday01` | gettimeofday | 获取时间 | |
| `gettimeofday02` | gettimeofday | 时间单调性 | |
| `umask01` | umask | 设置文件创建掩码 | |
| `mkdirat01` | mkdirat | 创建目录 | |
| `mkdirat02` | mkdirat | 重复创建 | 应返回 EEXIST |
| `rename01` | renameat2 | 重命名文件 | |
| `rename02` | renameat2 | 重命名到已存在 | |
| `unlink01` | unlinkat | 删除文件 | |
| `unlink02` | unlinkat | 删除不存在的文件 | |
| `mprotect01` | mprotect | 修改内存保护 | |
| `mprotect02` | mprotect | 非法权限 | |
| `fcntl01` | fcntl | F_GETFL/F_SETFL | |
| `fcntl02` | fcntl | 无效操作 | 应返回 EINVAL |
| `access01` | faccessat | 文件访问检查 | |

### 8.2 内核当前系统调用实现状态

```
syscall 编号: 名称 → 状态
──────────────────────────────────────
17:  getcwd       ✓ 已实现
20:  epoll_create1 ✗ stub (-38)
23:  dup          ✓ 已实现
24:  dup3         ✓ 已实现
25:  fcntl        ✓ 已实现
29:  ioctl        ✓ 已实现
33:  dup2         ✓ 已实现
34:  mkdirat      ✓ 已实现
35:  unlinkat     ✓ 已实现
36:  symlinkat    ✓ 已实现
39:  umount2      ✓ 已实现
40:  mount        ✓ 已实现
43:  statfs       ✓ 已实现
44:  fstatfs      ✓ 已实现
46:  ftruncate    ✓ 已实现
48:  faccessat    ✓ 已实现
49:  chdir        ✓ 已实现
56:  openat       ✓ 已实现
57:  close        ✓ 已实现
59:  pipe2        ✓ 已实现
61:  getdents64   ✓ 已实现
62:  lseek        ✓ 已实现
63:  read         ✓ 已实现
64:  write        ✓ 已实现
65:  readv        ✓ 已实现
66:  writev       ✓ 已实现
71:  sendfile     ✓ 已实现
72:  pselect6     ✓ 已实现
73:  ppoll        ✓ 已实现
78:  readlinkat   ✓ 已实现
79:  fstatat      ✓ 已实现
80:  fstat        ✓ 已实现
88:  utimensat    ✓ 已实现
93:  exit         ✓ 已实现
94:  exit_group   ✓ 已实现
96:  set_tid_address ✓ 已实现
98:  futex        ✓ 基础实现
99:  set_robust_list  ✗ stub (0)
101: nanosleep    ✓ 已实现
113: clock_gettime ✓ 已实现
116: syslog       ✓ 已实现
123: sched_getaffinity ✗ stub (-38)
124: yield        ✓ 已实现
129: kill         ✓ 已实现
134: sigaction    ✓ 已实现
135: sigprocmask  ✓ 已实现
139: sigreturn    ✓ 已实现
144: setgid       ✓ 已实现
146: setuid       ✓ 已实现
153: times        ✓ 已实现
154: setpgid      ✗ stub (0)
155: getpgid      ✗ stub (0)
160: uname        ✓ 已实现
165: getrusage    ✓ 已实现
166: umask        ✓ 已实现
167: prctl        ✗ stub (0)
169: gettimeofday ✓ 已实现
172: getpid       ✓ 已实现
173: getppid      ✓ 已实现
174: getuid       ✓ 已实现
175: geteuid      ✓ 已实现
176: getgid       ✓ 已实现
177: getegid      ✓ 已实现
178: gettid       ✓ 已实现
179: sysinfo      ✗ stub (-38)
198: socket       ✓ 已实现
200: bind         ✓ 已实现
201: listen       ✓ 已实现
202: accept       ✓ 已实现
203: connect      ✓ 已实现
204: getsockname  ✓ 已实现
205: getpeername  ✓ 已实现
206: sendto       ✓ 已实现
207: recvfrom     ✓ 已实现
208: setsockopt   ✓ 已实现
209: getsockopt   ✓ 已实现
210: shutdown(sock) ✓ 已实现
211: sendmsg      ✓ 已实现
212: recvmsg      ✓ 已实现
214: brk          ✓ 已实现
215: munmap       ✓ 已实现
220: clone        ✓ 已实现
221: exec         ✓ 已实现
222: mmap         ✓ 已实现
226: mprotect     ✓ 已实现
233: madvise      ✓ 已实现
258: riscv_hwprobe ✗ stub (-38)
260: waitpid      ✓ 已实现
261: prlimit64    ✗ stub (0)
276: renameat2    ✓ 已实现
278: getrandom    ✓ 已实现
290: eventfd2     ✗ stub (-38)
291: statx        ✓ 已实现
293: rseq         ✗ stub (-38)
300: sleep        ✓ 已实现
401: fork         ✓ 已实现
403: shutdown     保留兼容
其他:             所有未匹配 → -38 (ENOSYS)
```

### 8.3 LTP 全量 syscalls 测试分类（runtest/syscalls, 1799 行）

| 类别 | 涉及 syscall | 测试数 | 当前状态 |
|------|-------------|--------|----------|
| 文件 IO | open/openat, close, read, write, lseek, pread, pwrite, sendfile, copy_file_range | ~120 | 大部分可实现 |
| 文件元数据 | stat/fstat/fstatat, chmod, chown, access, ftruncate, fallocate, utimes, utimensat | ~100 | 部分可实现 |
| 目录操作 | mkdir/mkdirat, unlink/unlinkat, rename/renameat2, chdir, getcwd, getdents, link, symlink, readlink | ~85 | 大部分可实现 |
| 进程管理 | fork, clone, vfork, execve, exit/exit_group, wait/waitpid/waitid | ~80 | 大部分可实现 |
| 内存管理 | brk, mmap, munmap, mprotect, madvise, mlock, mincore, msync, mremap, memfd_create | ~110 | 基础可实现 |
| IPC/管道 | pipe/pipe2, dup/dup2/dup3, fcntl | ~40 | 大部分可实现 |
| 信号 | kill, sigaction, sigprocmask, sigsuspend, sigpending, sigaltstack, rt_sigqueueinfo, signalfd | ~70 | 部分可实现 |
| 时间 | nanosleep, clock_gettime, gettimeofday, settimeofday, adjtimex, timer_create, timerfd | ~60 | 部分可实现 |
| UID/GID | setuid, getuid, setgid, getgid, setreuid, setregid, setresuid, setresgid, getgroups, setgroups | ~60 | 部分可实现 |
| 网络 | socket, bind, connect, listen, accept, sendto/recvfrom, sendmsg/recvmsg, getsockopt/setsockopt | ~180 | 部分可实现 |
| 文件系统 | mount, umount, statfs, fstatfs, sync, fsync, fdatasync, flock | ~40 | 部分可实现 |
| epoll | epoll_create1, epoll_ctl, epoll_wait, epoll_pwait | ~25 | 未实现（stub） |
| 调度 | sched_yield, sched_getaffinity, sched_setaffinity, setpgid, getpriority, setpriority, nice | ~30 | 部分 stub |
| 资源限制 | getrlimit, setrlimit, getrusage, prlimit64, ulimit | ~25 | 部分 stub |
| 系统信息 | uname, sysinfo, sysctl, syslog, sysconf, pathconf | ~30 | 部分可实现 |
| 其他 | getrandom, getcpu, personality, ptrace, reboot, kcmp, keyctl, ioprio, inotify, fanotify | ~140 | 大多数不可实现 |
| 高级 IO | select, poll, ppoll, pselect6, eventfd, io_uring, aio, splice, tee, vmsplice, readv, writev | ~80 | 少部分可实现 |
| 动态链接 | uselib | ~2 | 需 PT_INTERP 支持（Phase 2 新增） |

### 8.4 常见问题排查

**Q: LTP 二进制运行时出现 "Segmentation fault"？**
A: 检查 musl 动态链接器是否已放入 `/lib/ld-musl-riscv64.so.1`，以及 PT_INTERP 是否已正确实现。

**Q: 大量测试返回 -38 (ENOSYS)？**
A: 正常现象。说明该测试使用的 syscall 在内核的 match 分支中未被匹配，落入默认的 `_ => -38`。需要实现该 syscall。

**Q: busybox 找不到 /ltp_testcode.sh？**
A: 检查是否设置了 `LTP_BIN_DIR` 且 `fs-img` 目标正确复制了脚本和二进制。

**Q: 磁盘空间不足？**
A: LTP 全量二进制 + musl .so 约 200-400MB。当前 `fs-img` 分配 128MB，需改为 `count=512`。

**Q: 编译报错 "FILTER_OUT_DIRS" 或 configure 失败？**
A: 确保先执行 `make autotools` 生成 configure 脚本。

**Q: ltp_testcode.sh 找不到 LTP 二进制？**
A: 确保 `LTP_BIN_DIR` 指向正确的目录。Docker 中编译后产物在 `/code/sdcard/riscv/musl/`（主机对应 `testsuits-for-oskernel/sdcard/riscv/musl/`）。

---

*文档版本: 2026-05-30*
*目标架构: riscv64 (musl)*
*链接方式: 动态链接 (libc.so + ld-musl-riscv64.so.1)*
