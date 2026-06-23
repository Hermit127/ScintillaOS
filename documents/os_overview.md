# OS 大赛操作系统架构全景

## 1. 这是什么

一个基于 Rust 编写的 **RISC-V / LoongArch 教学级操作系统内核**，参加 2026 全国大学生 OS 内核设计大赛。

- **内核**：`os/` — 纯 Rust，`#![no_std]`，运行在 S-mode
- **用户态**：`user/` — 基于 `user_lib` 的静态链接 ELF 二进制
- **文件系统**：`lwext4_rust/` — 嵌入式 ext4 C 库的 Rust 绑定
- **虚拟化**：QEMU 7.0 / 9.2，模拟 RISC-V VirtIO 平台

### 核心设计理念

| 特点 | 说明 |
|------|------|
| 单处理器 | 1 CPU，轮转调度 |
| 静态链接 | 所有用户程序静态链接，无动态链接器 |
| 全内核态锁 | UPSafeCell（基于 RefCell，假设中断关闭） |
| ext4 文件系统 | 通过 lwext4 C 库读写 ext4 磁盘镜像 |
| 双盘架构 | bus0=测试盘，bus1=根文件系统盘 |
| Linux ABI 兼容 | syscall 编号遵循 RISC-V Linux |

---

## 2. 启动流程

> **入口**：`os/src/main.rs:160` `fn main(hartid: usize)`

```
OpenSBI → 内核入口 (0x80200000)
  ├── init_heap()         内核堆（128MB）            os/src/mm/heap_allocator.rs:24
  ├── add_frames_range()  向帧分配器注册全部空闲内存  os/src/mm/frame_allocator.rs:71
  ├── mount_root()        挂载 bus1 根 ext4 文件系统   os/src/fs/vfs.rs:893
  ├── net_init()          探测 VirtIO 网卡             os/src/net/mod.rs
  ├── list_apps()         列 /bin 目录                  os/src/fs/vfs.rs:910
  ├── init_kernel_page()  捕获启动页表                  os/src/task/processor.rs:89
  ├── add_initproc()      加载 /bin/initproc→就绪队列   os/src/task/mod.rs:439
  └── run_tasks()         主调度循环→切到 initproc      os/src/task/processor.rs:39
```

### 陷阱处理 (`os/src/main.rs:43`)

```rust
fn kernel_interrupt(ctx, trap_type)
  SysCall → syscall_dispatch() → 返回 → ctx[TrapFrameArgs::RET] = result
  StorePageFault / LoadPageFault → handle_page_fault() → 失败则 SIGSEGV
  Timer → net_poll() + check_itimer() + suspend_current_and_run_next()
  ── 最后 ──
  handle_signals()                        os/src/task/mod.rs:619
  check_signals_error_of_current()        os/src/task/mod.rs:444
```

### 调度循环 (`os/src/task/processor.rs:39`)

```rust
loop {
    task = fetch_task();                      // FIFO 从就绪队列取  manager.rs:55
    task.status = Running;
    context_switch_pt(idle, task);             // 切页表 + 跳转
    // ... 任务运行 ...
    // 时钟中断 → suspend_current_and_run_next() → 回到 loop    mod.rs:61
}
```

---

## 3. 内存管理

### 物理内存

> `os/src/mm/frame_allocator.rs`

| 概念 | 位置 | 说明 |
|------|------|------|
| 帧分配器 | frame_allocator.rs | `buddy_system_allocator`（伙伴系统），受 Mutex 保护 |
| FrameTracker | `:25` | RAII 物理帧，Drop 时归还 |
| is_shared | `:38` | 检查帧是否被多个进程共享（引用计数>1） |
| shared_clone | `:34` | 增加引用计数，用于 CLONE_VM / shmat |
| add_frames_range | `:71` | 启动时注册全部空闲 RAM（约 126MB） |

### 虚拟内存

> `os/src/mm/memory_set.rs`

| 概念 | 位置 | 说明 |
|------|------|------|
| MemorySet | `:13` | `Arc<PageTableWrapper>` + `Vec<MapArea>` |
| MapArea | `:977` | 虚拟地址范围、数据帧映射、权限、文件后备 |
| MapPermission | `:25` | 位标志：R/W/X/U |
| 页表 | — | RV64 Sv39（三级页表），4KB 页面 |

### ELF 加载 (`from_elf`)

> `os/src/mm/memory_set.rs:169`

```rust
from_elf(elf_data: &[u8]) -> LoadedElf
  1. 解析 ELF PT_LOAD 程序头
  2. 每个段创建 MapArea → push() → map() 分配全部物理帧   :140, :1026
  3. BSS 末尾添加 USER_STACK_SIZE(256KB) 用户栈            config.rs:3
  4. 返回 { memory_set, entry_point, user_sp, ph_addr, ... }
```

### 写时复制 (fork)

> `os/src/task/task.rs:577` `fn fork()` → `os/src/mm/memory_set.rs:471` `from_existed_user()`

```
父进程 fork()
  ├── MemorySet::from_existed_user()
  │     ├── 新页表
  │     ├── 复制所有 MapArea
  │     └── 对每个共享帧: 父页只读 + 子分配新帧可写
  └── 创建新 TCB，pid+1
```

### CLONE_VM (线程共享内存)

> `os/src/task/task.rs:434` `fn fork_with_flags()`

```
clone(CLONE_VM) → fork_with_flags(flags)
  ├── CLONE_VM 设置 → 共享父进程页表 Arc → 父子看到同一物理内存
  ├── 线程退出时 recycle_data_pages() 跳过           memory_set.rs:632
  └── clone_vm 标志存在 TaskControlBlockInner:97     task.rs:97
```

### mmap/munmap

> `os/src/syscall/process.rs:1021` `sys_mmap` / `:1102` `sys_munmap`

- `mmap(addr, len, prot, flags, fd, offset)`：匿名/文件映射
- `munmap(start, len)`：解除映射，分裂/删除 MapArea  `memory_set.rs:704`
- COW 缺页处理：写共享帧时 `handle_page_fault` 分配新帧  `memory_set.rs:502`

### 缺页处理

> `os/src/mm/memory_set.rs:502` `fn handle_page_fault(vaddr, write_fault)`

1. 定位到包含 vaddr 的 MapArea
2. VPN 不在 data_frames 中 → 分配新帧，文件映射则从文件填充
3. 仅读缺页 → 重建 PTE
4. 写缺页 + 帧共享 → COW：分配新帧，拷贝页，更新 PTE

---

## 4. 进程管理

### TCB 结构 (`TaskControlBlockInner`)

> `os/src/task/task.rs:49`

```
──────────────────────────────────────────────
 trap_cx      陷阱帧（用户寄存器快照）
 task_cx      内核上下文（内核栈+PC）
 memory_set   虚拟地址空间
 fd_table     文件描述符表（Max 1024）
 cwd/root     当前目录 / 根目录
 parent/children  父子进程关系
 signals      待处理信号集
 signal_mask  阻塞信号掩码
 signal_actions  [64] 信号处理动作表
 killed/frozen  标记（被杀死/被冻结）
 exit_code    退出码
 heap_bottom/program_brk  堆边界
 clone_vm     是否共享内存（线程标记）            task.rs:97
 tid_ptr      线程清除地址（CLONE_CHILD_CLEARTID）
 uid/euid/gid/egid  凭证
 utime/stime  用户/内核时间（纳秒）
──────────────────────────────────────────────
```

### fork / exec / clone

| 操作 | 函数 | 位置 | 内存 | fd表 | 信号 |
|------|------|------|:--:|:--:|:--:|
| `fork()` | `TaskControlBlock::fork()` | task.rs:577 | COW复制 | 复制(引用+1) | 复制 |
| `clone(flags)` | `sys_clone()`→`fork_with_flags()` | process.rs:306, task.rs:434 | CLONE_VM=共享页表 | 复制 | CLONE_SIGHAND=共享 |
| `exec()` | `sys_exec()`→`exec()` | process.rs:367, task.rs:333 | 全新加载 | 保留非CLOEXEC | 保留 |

### 退出流程

> `os/src/task/mod.rs:358` `fn exit_current_and_run_next(exit_code)`

```
exit_current_and_run_next(exit_code)
  ├── status = Zombie
  ├── recycle_data_pages() (非线程→CLONE_VM 时跳过)
  ├── clear_fd_table()
  ├── 孤儿 → initproc 收养
  ├── 向父进程发 SIGCHLD (除非被阻塞)
  ├── 线程退出: 清 tid_ptr + futex WAKE              task/mod.rs LATEST
  └── 父在 WAITPID_QUEUE → 唤醒

TaskControlBlockInner::alloc_fd()   →   task.rs:116
TaskControlBlockInner::close_fd()   →   task.rs:130
```

### 调度器

> `os/src/task/manager.rs:55` `fetch_task()` / `:43` `add_task()`

- `TASK_MANAGER`：FIFO VecDeque
- `PID2TCB`：BTreeMap<pid, Arc<TaskControlBlock>>，用于按 PID 查找

### 阻塞队列

> `os/src/task/mod.rs:50-60`

| 队列 | 位置 | 用途 |
|------|------|------|
| SLEEP_QUEUE | `:50` | nanosleep/定时器阻塞 |
| NET_BLOCK_QUEUE | `:53` | 网络轮询阻塞 |
| WAITPID_QUEUE | `:56` | waitpid 等待子进程 |
| FUTEX_QUEUE | LATEST | futex 等待/唤醒 |

---

## 5. 信号子系统

### 数据结构

> `os/src/task/signal.rs:3` `MAX_SIG` / `:6` `SignalFlags`

```rust
SignalFlags: u64  // 位掩码，支持 0-63 位信号
  SIGKILL(9), SIGTERM(15), SIGSTOP(19), SIGCHLD(17),
  SIGSEGV(11), SIGCANCEL(32), SIGSETXID(33), ...

SignalAction {                        os/src/task/action.rs:7
    handler: usize,    // 处理函数地址
    flags: usize,      // SA_NODEFER, SA_SIGINFO 等
    restorer: usize,   // sigreturn 跳板地址
    mask: u64,         // 处理期间额外阻塞的信号集
}
SignalActions { table: [SignalAction; 64] }  action.rs:26
```

### 用户态 ABI

> `user/src/lib.rs` `pub struct SignalAction` — 与内核 `action.rs:7` 布局一致（32 字节）

```rust
pub fn sigaction(signum, action, old_action) → lib.rs:271
pub fn sigprocmask(how, set, oldset)          → lib.rs:283
pub fn sigreturn()                            → lib.rs:287
pub fn kill(pid, signum)                      → lib.rs:267
```

### 信号发送

> `os/src/syscall/process.rs:765` `sys_kill(pid, signum)` / LATEST `sys_tgkill(tgid, tid, sig)`

```
kill(pid, signum)
  ├── pid2task(pid) 查找目标进程
  ├── 检查是否已在待处理集中（去重）
  ├── signals |= (1<<signum)
  ├── SIGKILL/TERM → killed = true, exit_signal = -signum
  ├── 其他信号 + handler=0(默认) → 判断 fatal 列表
  │     fatal 列表: SIGSEGV|ABRT|ILL|FPE|BUS|SYS|TRAP|ALRM|
  │                 VTALRM|PROF|TERM|INT|QUIT|HUP|PIPE|USR1/2|
  │                 XCPU|XFSZ|IO|PWR
  └── 若目标阻塞在 futex → wake_futex_for_task()    task/mod.rs LATEST
```

### 信号递送

> `os/src/task/mod.rs:619` `handle_signals()` / `:576` `check_pending_signals()`

每次陷阱返回前执行：
```
handle_signals()
  └── check_pending_signals()
        └── for sig in 0..MAX_SIG:
              if signals 有 & signal_mask 无:
                ├── SIGKILL/STOP/CONT/TERM/DEF → call_kernel_signal_handler() :467
                └── 其他 → call_user_signal_handler(sig, signal)             :496
                      ├── handler=1(SIG_IGN) → 丢弃
                      ├── handler≠0 → 备份trapframe → SEPC=handler → RA=restorer
                      └── handler=0(默认) → 检查 fatal → killed=true / 忽略
```

### 默认信号行为 (`call_kernel_signal_handler`)

> `os/src/task/mod.rs:467`

| 信号 | 行为 |
|------|------|
| SIGSTOP | frozen = true |
| SIGCONT | frozen = false |
| SIGCHLD, SIGURG, SIGWINCH, SIGIO | 忽略（仅清除待处理标志） |
| SIGTSTP, SIGTTIN, SIGTTOU | frozen = true |
| 其余 | killed = true |

### 信号处理期间的掩码

> `os/src/task/mod.rs:496` `call_user_signal_handler`

```
SA_NODEFER=0:  signal_mask |= (1<<sig) | sa_mask     // 阻塞当前信号 + 用户指定掩码
handling_sig = sig
SEPC → handler
RA → restorer (sigreturn 跳板)
a0 → 信号编号
```

> `os/src/syscall/process.rs:902` `sys_sigreturn` → 恢复 trapframe → 返回原执行点。

### SIGCHLD 机制

> `os/src/task/mod.rs:358` `exit_current_and_run_next`

```
子进程退出
  └── exit_current_and_run_next()
        ├── 父在 WAITPID_QUEUE → 唤醒
        └── 若父未屏蔽 SIGCHLD → signals |= SIGCHLD
```

### sigtimedwait

> `os/src/syscall/process.rs:1983` `sys_rt_sigtimedwait(set, info, timeout, sigsetsize)`

给 `runtest.exe` 提供带超时的信号等待（5 秒超时等待子进程 SIGCHLD）：
1. 保存当前 signal_mask → 设为只允许目标信号
2. 循环检查待处理信号 → 找到则返回
3. 超时检查 → 到期则返回 -EAGAIN
4. 未到期 → suspend 让出 CPU → 下次被调度时重新检查

---

## 6. 文件系统

### VFS 层

> `os/src/fs/vfs.rs`

| 结构 | 位置 | 说明 |
|------|------|------|
| VfsSuperBlock | `:54` | 超级块，包装 lwext4 实例，UPSafeCell 保护 |
| Dentry | `:545` | 目录项缓存节点：name、path、fs_path、children |
| Mount | — | `{ mnt_point, mnt_root }` 挂载条目 |
| MOUNT_TABLE | `:627` | `Vec<Mount>` 全局挂载表 |

### 路径解析

> `vfs_resolve_path()` `os/src/fs/vfs.rs:737` / `vfs_lookup()` `:688`

```
open("/path/to/file") → sys_openat() → open_file_new()  inode.rs:385
  └── vfs_resolve_with_fallback(start, "/path/to/file")
        └── vfs_resolve_path()
              ├── "/" → ROOT_DENTRY
              ├── "path" → vfs_lookup(root, "path")
              │     ├── 查 Dentry.children 缓存
              │     ├── 缓存未命中 → ext4 dir_get_entries()
              │     └── 创建 Dentry 加入缓存 + lookup_mount() :659
              ├── "to" → vfs_lookup(path_dir, "to")
              └── "file" → vfs_lookup(to_dir, "file")
```

### 挂载系统

> `os/src/syscall/fs.rs:1664` `sys_mount()` / `os/src/fs/vfs.rs:848` `vfs_mount()`

```
mount(source, target, fstype, flags, data)
  ├── vfs_resolve_path(cwd, target) → mnt_point dentry
  ├── VfsSuperBlock::new_ext4(BLOCK_DEVICE, "ext4")  vfs.rs:61
  └── vfs_mount(mnt_point, sb)
        ├── MOUNT_TABLE.push({ mnt_point, mnt_root })
        └── 此后 /mnt/* 路径解析会查表跳转到新 FS
```

### ext4 集成

> `os/src/fs/vfs.rs:61` `VfsSuperBlock::new_ext4(blk_device, fs_type)`

所有 FS 操作都通过 `with_fs(|ext4| { ... })` 闭包模式，持有锁访问 lwext4 C 库：

| 操作 | 函数 |
|------|------|
| dir_get_entries(path) | 读目录内容 |
| get_inode_info(path) | 获取 inode 信息（mode, size, uid, gid, atime...） |
| read_at(path, offset, buf) | 读文件 |
| write_at(path, offset, buf) | 写文件 |
| create_file(path) | 创建文件 |
| truncate_file(path, len) | 截断文件 |
| fsync(path) / sync_all() | 同步到磁盘 |

### open_file 流程

> `os/src/fs/inode.rs:385` `open_file_new(base, path, flags, mode)`

1. 若 `O_CREATE`：先查是否存在 → 存在则打开/截断；不存在则 `vfs_create()` 创建
2. 若 `O_TMPFILE`：在目标目录创建随机名临时文件 (`:232`)
3. 常规打开：`vfs_resolve_path()` → 返回 `OSInode`
4. `/proc/` 路径在 `sys_openat` 中先被 `open_proc()` 拦截 (`fs.rs:289`)

### 特殊文件

| 路径 | 类型 | 实现 | 位置 |
|------|------|------|------|
| `/proc/` | procfs | ProcDirFile | proc.rs:7 |
| `/proc/mounts` | procfs | ProcMountsFile | proc.rs:166 |
| `/proc/meminfo` | procfs | ProcMeminfoFile | proc.rs:220 |
| `/proc/<pid>/stat` | procfs | ProcPidStatFile | proc.rs:92 |
| `/dev/null` | 字符设备 | DevNull | stdio.rs:136 |
| `/dev/zero` | 字符设备 | DevZero | stdio.rs |
| `/dev/urandom` | 字符设备 | Urandom | stdio.rs:104 |
| `/dev/rtc` | 字符设备 | DevRtcFile | proc.rs |

所有 proc/dev 路径在 `sys_openat` 中通过 `open_proc()` 拦截（`fs.rs:289`），在 ext4 查找之前执行。

### stdin/stdout

> `os/src/fs/stdio.rs`

- **Stdin**（`:37`）：通过 `DebugConsole::getchar()` 逐字符读取，锁住 CPU 直到有输入
- **Stdout**：原子 `STDOUT_OWNER` 锁，获取所有权的进程独占输出，进程挂起时释放
- **Pipe**（`pipe.rs:158` `make_pipe()`）：环形缓冲区（4KB），支持阻塞/非阻塞读写

---

## 7. 系统调用

### 分发入口

> `os/src/syscall/mod.rs` — `fn syscall(syscall_id, [args; 6]) -> isize`

每个 syscall 6 个参数，编号遵循 RISC-V Linux ABI。返回值写入 `TrapFrameArgs::RET`。最后调用的 syscall 信息存储在全局原子变量中用于调试。

### 完整分类（~110 个）

**进程**（`os/src/syscall/process.rs`）
fork, clone, exec, exit, exit_group, waitpid, waitid,
getpid, getppid, gettid, brk, yield, sleep, nanosleep,
shutdown(403), set_tid_address, getrandom

**信号**（`os/src/syscall/process.rs`）
kill, tgkill, sigaction, sigprocmask, sigreturn,
rt_sigtimedwait, getitimer, setitimer, sigaltstack, rt_sigpending

**文件系统**（`os/src/syscall/fs.rs`）
open/openat, close, read/write/readv/writev, pread64/pwrite64,
lseek, pipe2, dup/dup2/dup3, fcntl, fstat/fstatat/stat/statx,
getdents64, getcwd, chdir, mkdirat, unlinkat, symlinkat, readlinkat,
renameat2, ftruncate, faccessat, fchmod/fchmodat, fchown/fchownat,
utimensat, mount, umount2, sync/fsync/fdatasync, statfs/fstatfs

**内存**（`os/src/syscall/process.rs`）
mmap, munmap, mprotect, msync, madvise, mremap, mincore,
mlock/munlock/mlockall/munlockall/brk

**IPC**（`os/src/syscall/process.rs`）
shmget, shmat, shmctl, shmdt

**网络**（`os/src/net/syscall.rs`）
socket, bind, listen, accept, connect, sendto, recvfrom,
sendmsg, recvmsg, setsockopt, getsockopt, getsockname, getpeername,
sock_shutdown

**凭证**（`os/src/syscall/process.rs`）
getuid/euid/gid/egid, setuid/gid, getresuid/gid, setreuid/gid,
setresuid/gid, getgroups/setgroups, umask

**系统**（`os/src/syscall/process.rs`）
uname, times, get_time, clock_gettime, clock_getres, getrusage,
syslog, sysinfo, getrlimit/setrlimit, prlimit64

**存根（返回 -ENOSYS）**: eventfd2, rseq, epoll_create1, riscv_hwprobe, sched_getaffinity

---

## 8. 网络栈

### 架构

> `os/src/net/mod.rs`（初始化）/ `os/src/net/syscall.rs`（syscall 实现）

```
用户程序 (iperf3, netperf)
    │ syscall (socket/bind/connect/send/recv)
    ▼
本地环回栈 (TCP/UDP)          smoltcp 网络栈
(local_tcp/udp_*)             (通过 VirtIO 网卡)
    │                              │
    ▼                              ▼
VecDeque 缓冲区               VirtIO 网卡 ↗ QEMU SLIRP
```

### 本地环回 (127.0.0.1)

- TCP：`LOCAL_TCP_LISTENERS` 按端口号 + `LOCAL_TCP_CONNS` 双向缓冲（256KB 上限）
- UDP：`LOCAL_UDP_QUEUE` VecDeque（最多 256 个包）
- 阻塞/非阻塞模式：`NET_BLOCK_QUEUE` + `BlockReason` 枚举管理等待任务

### 已知限制

- 无 IPv6
- 无原始套接字
- 仅 SLIRP 用户模式网络
- RISC-V / AArch64 有 VirtIO 网卡，LoongArch 无

---

## 9. 用户态运行时

### _start 入口

> `user/src/lib.rs:35` `pub extern "C" fn _start(argc, argv)`

```
_start(argc, argv)
  ├── HEAP.lock().init(HEAP_SPACE, USER_HEAP_SIZE=8MB)   lib.rs:21,37
  ├── 解析 argc/argv 指针数组 → Vec<&str>                lib.rs:40-53
  └── exit(main(argc, &argv))                            lib.rs:54
```

### 系统调用包装

> `user/src/syscall.rs` — 所有原始 syscall 函数

```rust
// RISC-V 内联汇编:                      syscall.rs:57
asm!("ecall", in("x17") id, inlateout("x10") arg0 => ret, ...)
```

> `user/src/lib.rs` — 安全包装函数

```rust
pub fn fork()           → lib.rs:135   pub fn chdir(path)   → lib.rs:295
pub fn exec(path, args) → lib.rs:138   pub fn mount(...)    → lib.rs:311
pub fn waitpid(pid,..)  → lib.rs:156   pub fn shutdown()    → lib.rs:307
pub fn open(path, flags)→ lib.rs:81    pub fn kill(pid,sig) → lib.rs:267
```

### 信号常量

> `user/src/lib.rs:230-265`

```rust
bitflags! { pub struct SignalFlags: i64 { SIGKILL=1<<9, SIGCHLD=1<<17, ... }}
pub struct SignalAction { handler, flags, restorer, mask }   lib.rs:183
pub const SIGKILL: i32 = 9;                                   lib.rs:204
pub const SIGCHLD: i32 = 17;                                  lib.rs:214
```

---

## 10. 驱动层

### VirtIO 块设备

> `os/src/drivers/block/virtio_blk.rs:237` `fn probe_all()`

```
VirtIOBlock (virtio-drivers crate)                :42
  ├── MMIO 探测 (RISCV/AArch64): 扫描 8 个地址 (0x10001000-0x10008000)
  └── PCI 探测 (LoongArch): ECAM 地址空间 (0x20000000)

probe_all() → Vec<Arc<dyn BlockDevice>>
  ├── [0] = BLOCK_DEVICE (bus0, 测试盘)          block/mod.rs:41
  └── [1] = ROOT_BLOCK_DEVICE (bus1, 根盘)       block/mod.rs:46
```

### 双重磁盘

> `os/src/drivers/block/mod.rs:41-59`

```
bus 0 (0x10001000): TEST_DISK     ─── 大赛评测提供的测试盘
bus 1 (0x10002000): disk.img      ─── 参赛者的根文件系统
```

- `mount_root()` 使用 `ROOT_BLOCK_DEVICE` 创建根文件系统（`vfs.rs:893`）
- `sys_mount()` 使用 `BLOCK_DEVICE` 挂载测试盘（`fs.rs:1680`）
- LoongArch 无真实 VirtIO 块设备时降级为 `RamDiskBlock`

### 块设备驱动细节

> `os/src/drivers/block/virtio_blk.rs`

- 块 I/O 以 4096 字节 DMA 缓冲区为单位（循环处理分块）
- `VirtioHal`：提供 `dma_alloc`（分配物理连续帧）/ `dma_dealloc` / `mmio_phys_to_virt`
- 探测通过检查 magic 值（0x74726976 = "virt"）和设备 ID（2 = 块设备）判断

---

## 11. 构建系统

> `Makefile`

```
make all                       # 大赛提交: kernel-rv + kernel-la + disk*.img
make kernel ARCH=riscv64       # 只编译内核
make fs-img ARCH=riscv64       # 编译用户程序 + 创建 ext4 根文件系统镜像
make run ARCH=riscv64          # 单盘 QEMU 运行（bus0=bus1=同一镜像）
make run-dual TEST_DISK=...    # 双盘模拟大赛环境
make kernel-rv / kernel-la     # 特定架构内核
make disk.img / disk-rv.img    # 特定根文件系统镜像
```

### 产物

| 文件 | 说明 | 位置 |
|------|------|------|
| `kernel-rv` | RISC-V 内核二进制 | 通过 rust-objcopy 从 ELF 生成 |
| `kernel-la` | LoongArch 内核 ELF | target/loongarch64-unknown-none/release/os |
| `disk.img` | 通用根文件系统镜像 (512MB ext4) | target/*/release/fs.img |
| `disk-rv.img` | RISC-V 专用根文件系统 | target/riscv64gc-unknown-none-elf/release/fs.img |

### fs-img 构建流程

> `Makefile:137-304`

1. `cd user && make build` — 编译所有 `user/src/bin/*.rs` → `/bin/`
2. 拷贝 busybox（musl/glibc）、测试脚本、LTP/LMBench 等 → rootfs
3. `dd + mke2fs -t ext4` — 从 rootfs 目录创建 512MB ext4 镜像
4. 输出 `target/{ARCH}/release/fs.img` 并复制为 `fs-img.img`

---

## 12. 测试框架集成

### initproc 执行流程

> `user/src/bin/initproc.rs` — 大赛版 init 进程

```
mount(bus0, /mnt)
  ├── 扫描 /mnt/ 根目录 → 查找 basic + busybox 测试脚本
  ├── 扫描 /mnt/musl/ → 运行 libctest/lmbench/iozone/lua 等测试
  ├── 扫描 /mnt/glibc/ → 同上（glibc 版本）
  └── 若都无 → rootfs fallback（/bin/basic_testcode.sh + busybox_testcode.sh）
```

### 12 个测试组

| 测试组 | 测例数 | 类型 | 裁判脚本 |
|--------|--------|------|----------|
| basic | 30 | 单测例 ELF，逐项 fork+exec | judge_basic.py |
| busybox | ~200 | busybox sh 批量执行 | judge_busybox.py |
| libc-test | 217 | 静态/动态链接 libc 一致性 | judge_libctest.py |
| iozone | 7 | 磁盘 I/O 吞吐量 | judge_iozone.py |
| lua | 9 | Lua 脚本执行 | judge_lua.py |
| libc-bench | 22 | libc 性能基准 | 未实现 |
| lmbench | ~30 | 系统微基准 | lmbench.sh |
| iperf/netperf | ~5 | 网络吞吐量 | 未实现 |
| unixbench | ~200 | 综合基准 | 未实现 |
| cyclictest | ~10 | 实时延迟 | 未实现 |
| LTP | subset | Linux Test Project 子集 | 未实现 |

### 裁判机制

> `oskernel-testsuits-cooperation/judge/` — Python 裁判脚本

```
测试输出:
  #### OS COMP TEST GROUP START <name> ####
  ... 测试 stdout ...
  #### OS COMP TEST GROUP END <name> ####

裁判脚本: parse(group_markers) → {"name": ..., "pass": N, "score": N}[/N]
评分输出: JSON 数组 [{name, pass, total, score}, ...]
```

### test_iozone.sh 脚本

> 独立脚本 `test_iozone.sh`，可跳过 Docker 直接用本机 QEMU 跑 iozone 测试。
