# BusyBox 接入所需系统调用

## 总览

| 层级 | 数量 | 说明 | 状态 |
|------|------|------|------|
| Tier 1（启动必现） | 5 | musl `_start` 阶段调用，缺少则进程崩溃 | ✅ 全部已实现 |
| Tier 2（基础命令） | 13 | `sh`/`ls`/`cat`/`echo`/`pwd`/`sleep` 可用 | 7 已实现 / 6 未实现 |
| Tier 3（扩展命令） | 8 | `cp`/`mv`/`rm`/`touch`/`chmod`/`test` 可用 | 0 已实现 / 8 未实现 |
| Tier 4（stub 即可） | 14 | 返回 `-ENOSYS` 让 musl fallback | 0 已实现 / 14 未实现 |
| **合计** | **40** | | **20 已实现 / 20 未实现** |

---

## 完整系统调用状态表

### 已实现 ✓

| syscall | 编号 | 实现函数 | 说明 |
|---------|------|---------|------|
| `getcwd` | 17 | `sys_getcwd` | 获取当前工作目录 |
| `dup` | 23 | `sys_dup` | 复制 fd |
| `dup3` | 24 | `sys_dup3` | 复制 fd（带 flags） |
| `dup2` | 33 | `sys_dup2` | 复制 fd 到指定编号（兼容） |
| `mkdirat` | 34 | `sys_mkdirat` | 创建目录 |
| `unlinkat` | 35 | `sys_unlinkat` | 删除文件/目录 |
| `umount2` | 39 | `sys_umount2` | 卸载文件系统 |
| `mount` | 40 | `sys_mount` | 挂载文件系统 |
| `shutdown` | 48 | `sys_shutdown` | **自定义**，冲突 `faccessat(48)` |
| `chdir` | 49 | `sys_chdir` | 切换工作目录 |
| `openat` | 56 | `sys_openat` | 打开文件 |
| `close` | 57 | `sys_close` | 关闭文件 |
| `pipe2` | 59 | `sys_pipe2` | 创建管道 |
| `getdents64` | 61 | `sys_getdents64` | 读取目录项 |
| `read` | 63 | `sys_read` | 读取文件 |
| `write` | 64 | `sys_write` | 写入文件 |
| `fstat` | 80 | `sys_fstat` | 获取 fd 对应文件状态 |
| `exit` | 93 | `sys_exit` | 进程退出（单线程） |
| `exit_group` | 94 | `sys_exit_group` | 进程组退出 |
| `set_tid_address` | 96 | `sys_set_tid_address` | TLS tidptr 注册 |
| `nanosleep` | 101 | `sys_nanosleep` | 纳秒级睡眠 |
| `sched_yield` | 124 | `sys_yield` | 让出 CPU |
| `kill` | 129 | `sys_kill` | 发送信号 |
| `rt_sigaction` | 134 | `sys_sigaction` | 设置信号处理 |
| `rt_sigprocmask` | 135 | `sys_sigprocmask` | 信号屏蔽 |
| `rt_sigreturn` | 139 | `sys_sigreturn` | 信号返回 |
| `setgid` | 144 | `sys_setgid` | 设置组 ID |
| `setuid` | 146 | `sys_setuid` | 设置用户 ID |
| `times` | 153 | `sys_times` | 进程时间统计 |
| `uname` | 160 | `sys_uname` | 系统信息 |
| `gettimeofday` | 169 | `sys_get_time` | 获取时间 |
| `getpid` | 172 | `sys_getpid` | 获取进程 ID |
| `getppid` | 173 | `sys_getppid` | 获取父进程 ID |
| `getuid` | 174 | `sys_getuid` | 获取用户 ID |
| `geteuid` | 175 | `sys_geteuid` | 获取有效用户 ID |
| `getgid` | 176 | `sys_getgid` | 获取组 ID |
| `getegid` | 177 | `sys_getegid` | 获取有效组 ID |
| `gettid` | 178 | `sys_gettid` | 获取线程 ID |
| `brk` | 214 | `sys_brk` | 堆内存分配 |
| `munmap` | 215 | `sys_munmap` | 解除内存映射 |
| `clone` | 220 | `sys_clone` | 创建子进程（fork 等效） |
| `execve` | 221 | `sys_exec` | 执行程序 |
| `mmap` | 222 | `sys_mmap` | 内存映射 |
| `wait4` | 260 | `sys_waitpid` | 等待子进程 |

### 未实现 — Tier 2：基础命令缺少

| syscall | 编号 | 优先级 | 说明 |
|---------|------|--------|------|
| `ioctl` | 29 | ★★☆ | 终端属性查询（TCGETS）。至少需要 stub 返回 `-ENOTTY` |
| `fcntl` | 25 | ★★★ | fd 属性控制（`F_GETFD`/`F_SETFD`/`F_DUPFD`），busybox 对每个 fd 调用 |
| `lseek` | 62 | ★★★ | 文件偏移控制。`cat`/`sh` 重定向等必需 |
| `readlinkat` | 78 | ★☆☆ | 符号链接解析。当前 ext4 无 symlink，stub 返回 `-EINVAL` |
| `newfstatat` | 79 | ★★★ | **musl `stat()` 入口**。`ls`/`test`/`find` 等核心命令必需 |
| `clock_gettime` | 113 | ★★☆ | `sleep`/`timeout` 命令需要。替换当前非标 `sys_sleep(300)` |

### 未实现 — Tier 3：扩展命令缺少

| syscall | 编号 | 优先级 | 说明 |
|---------|------|--------|------|
| `faccessat` | 48 | ★★★ | `test -f`/`-d`/`-r`/`-w` 等条件判断。⚠️ 与当前 `shutdown(48)` 冲突 |
| `fchmodat` | 53 | ★★☆ | `chmod` 命令核心 |
| `fchownat` | 54 | ★☆☆ | `chown` 命令核心 |
| `utimensat` | 88 | ★★☆ | `touch` 命令修改文件时间戳 |
| `umask` | 166 | ★★☆ | 文件创建权限掩码 |
| `mprotect` | 226 | ★☆☆ | 内存页保护。stub 返回 0 即可 |
| `renameat2` | 276 | ★★★ | `mv` 命令核心。跨目录移动文件必需 |

### 未实现 — Tier 4：Stub 即可

| syscall | 编号 | stub 返回值 | 说明 |
|---------|------|-----------|------|
| `eventfd2` | 19 | `-38` (ENOSYS) | 事件通知，busybox 不依赖 |
| `epoll_create1` | 20 | `-38` (ENOSYS) | epoll，简单场景不需要 |
| `statfs` | 43 | `-38` (ENOSYS) | 文件系统统计，后续实现 `df` 命令 |
| `fstatfs` | 44 | `-38` (ENOSYS) | 同上 |
| `sendfile` | 71 | `-38` (ENOSYS) | 零拷贝发送，少量 busybox applet 使用 |
| `ppoll` | 73 | `-38` (ENOSYS) | poll，busybox sh 用阻塞 read |
| `set_robust_list` | 99 | `-38` (ENOSYS) | 线程 robust mutex 列表，单线程不需要 |
| `sched_getaffinity` | 123 | `-38` (ENOSYS) | CPU 亲和性，单核可忽略 |
| `prctl` | 167 | `0` | 线程名称等控制，返回成功 |
| `sysinfo` | 179 | `-38` (ENOSYS) | 系统信息，busybox 可降级 |
| `madvise` | 233 | `0` | 内存建议，musl 分配内存时调用 |
| `prlimit64` | 261 | `0` | 资源限制，返回成功 |
| `getrandom` | 278 | `-38` (ENOSYS) | 随机数，musl fallback 到弱随机 |
| `rseq` | 293 | `-38` (ENOSYS) | 可重启序列，musl 尝试注册 |

---

## Tier 1：启动必现 ✅ 全部已实现

musl `_start` → `__libc_start_main` 阶段立即调用的 5 个 syscall 全部就绪：

| syscall | 编号 | 状态 |
|---------|------|------|
| `exit_group` | 94 | ✅ `sys_exit_group` |
| `set_tid_address` | 96 | ✅ `sys_set_tid_address` |
| `brk` | 214 | ✅ `sys_brk` |
| `mmap` | 222 | ✅ `sys_mmap`（支持 anonymous + file-backed） |
| `munmap` | 215 | ✅ `sys_munmap`（支持跨多 area 范围） |

---

## Tier 2：基础命令（`sh`/`ls`/`cat`/`echo`/`pwd`/`sleep`）

完成 Tier 2 后以下 busybox 命令可用：

```
sh    ls    cat    echo    pwd    sleep    true    false    which
```

### 待实现（6 个）

#### 1. lseek (62)

**作用**：文件读写偏移控制。没有它，`cat` 只能读一次，shell 重定向会出错。

**Linux 签名**：`off_t lseek(int fd, off_t offset, int whence)`

**实现要点**：
```rust
pub fn sys_lseek(fd: usize, offset: i64, whence: i32) -> isize {
    // SEEK_SET=0, SEEK_CUR=1, SEEK_END=2
    // 通过 OSInode 的 inner.offset 获取/设置偏移
    // OSInode 需新增 fn get_offset() 和 fn set_offset()
}
```

**改动文件**：`syscall/fs.rs`、`fs/inode.rs`（新增 offset getter/setter）

**难度**：★★☆ | **预估**：30 分钟

---

#### 2. newfstatat (79)

**作用**：**musl 的 `stat()` libc 函数入口**。`ls` 列出文件大小/类型/权限全靠它。比 `fstat(80)` 更通用（支持路径解析）。

**Linux 签名**：`int newfstatat(int dirfd, const char *path, struct stat *buf, int flags)`

**实现要点**：
```rust
pub fn sys_newfstatat(dirfd: isize, path: *const u8, _stat_buf: *mut u8, flags: usize) -> isize {
    // AT_EMPTY_PATH = 0x1000: path 为空时对 dirfd 本身 stat → 委托 sys_fstat
    // AT_SYMLINK_NOFOLLOW = 0x100: 不跟随符号链接（暂忽略，无 symlink 支持）
    // 否则：解析 path → 获取 dentry → 调用 ext4 get_inode_ref → 填充 stat 结构
}
```

**改动文件**：`syscall/fs.rs`、`syscall/mod.rs`

**难度**：★★★ | **预估**：1 小时

---

#### 3. fcntl (25)

**作用**：busybox 对每个打开的 fd 调用 `F_GETFD`/`F_SETFD` 管理 `FD_CLOEXEC` 标志。缺少时 fd 管理出错。

**Linux 签名**：`int fcntl(int fd, int cmd, ...)`

**最小实现**：
```rust
pub fn sys_fcntl(fd: usize, cmd: usize, arg: usize) -> isize {
    match cmd {
        0 => /* F_DUPFD: 同 dup，但从 arg 起分配 fd */,
        1 => /* F_GETFD: 返回 cloexec 标志 */,
        2 => /* F_SETFD: 设置 cloexec 标志 */,
        1030 => /* F_DUPFD_CLOEXEC: dup + 设置 cloexec */,
        _ => -22, // EINVAL
    }
}
```

**改动文件**：
- `syscall/fs.rs` — 新增 `sys_fcntl`
- `syscall/mod.rs` — 注册 `SYSCALL_FCNTL = 25`
- `task/task.rs` — `fd_table` 每项改为 `(Option<Arc<dyn File>>, bool)` 增加 cloexec 标志

**难度**：★★★ | **预估**：1 小时

---

#### 4. ioctl (29)

**作用**：busybox 对 stdin/stdout 调 `TCGETS` 获取终端属性。stub 版本返回 `-ENOTTY` 即可让 busybox 继续运行。

**Linux 签名**：`int ioctl(int fd, unsigned long request, ...)`

**初始 stub**：
```rust
pub fn sys_ioctl(_fd: usize, _request: usize, _arg: usize) -> isize {
    -25 // ENOTTY: 不是终端设备
}
```

**后续增强**：对 fd 0/1/2 返回固定的 `termios` 结构体（支持 ANSI 转义等）。

**改动文件**：`syscall/fs.rs`、`syscall/mod.rs`

**难度**：★☆☆（stub）/ ★★★（完整） | **预估**：10 分钟（stub）

---

#### 5. clock_gettime (113)

**作用**：`sleep`/`timeout` 命令需要。musl 的 `clock_gettime()` 是 `sleep()` 的时间基准，可替代当前非标准的 `sys_sleep(300)`。

**Linux 签名**：`int clock_gettime(clockid_t clk_id, struct timespec *tp)`

**实现要点**：
```rust
pub fn sys_clock_gettime(clk_id: usize, tp: *mut u8) -> isize {
    // CLOCK_REALTIME=0, CLOCK_MONOTONIC=1
    let now_ms = polyhal::time::Time::now().to_msec() as usize;
    let ts = TimeSpec {
        tv_sec: now_ms / 1000,
        tv_nsec: (now_ms % 1000) * 1_000_000,
    };
    // 写入用户空间 tp
    0
}
```

**改动文件**：`syscall/process.rs`、`syscall/mod.rs`

**难度**：★★☆ | **预估**：30 分钟

---

#### 6. readlinkat (78)

**作用**：符号链接解析。当前 ext4 文件系统无 symlink 支持，stub 返回 `-EINVAL` 即可。后续如需支持 `ls -l` 显示链接目标，需扩展 ext4。

**Linux 签名**：`ssize_t readlinkat(int dirfd, const char *path, char *buf, size_t bufsiz)`

**初始 stub**：
```rust
pub fn sys_readlinkat(_dirfd: isize, _path: *const u8, _buf: *mut u8, _bufsiz: usize) -> isize {
    -22 // EINVAL: 不是符号链接
}
```

**改动文件**：`syscall/fs.rs`、`syscall/mod.rs`

**难度**：★☆☆（stub） | **预估**：5 分钟

---

### 已实现（7 个）✅

| syscall | 编号 | 备注 |
|---------|------|------|
| `getuid`/`geteuid`/`getgid`/`getegid` | 174-177 | 返回 uid/euid/gid/egid |
| `gettid` | 178 | 委托 `sys_getpid` |
| `gettimeofday` | 169 | `sys_get_time` |
| `nanosleep` | 101 | `sys_nanosleep` |

---

## Tier 3：扩展命令（`cp`/`mv`/`rm`/`touch`/`chmod`/`test`）

完成 Tier 3 后以下命令额外可用：

```
cp    mv    rm    touch    chmod    chown    test    find    stat
```

### 待实现（8 个）

#### 7. renameat2 (276)

**作用**：**`mv` 命令核心**。跨目录移动/重命名文件。

**Linux 签名**：`int renameat2(int olddirfd, const char *oldpath, int newdirfd, const char *newpath, unsigned int flags)`

**实现要点**：解析 old/new 路径 → 在同一文件系统内执行 rename（需 ext4_rs 支持 link/unlink 组合操作）。

**改动文件**：`syscall/fs.rs`、`syscall/mod.rs`、`fs/vfs.rs`（新增 rename）

**难度**：★★★ | **预估**：2 小时

---

#### 8. faccessat (48) ⚠️ 冲突

**作用**：`test -f`/`-d`/`-r`/`-w`/`-x` 等条件判断的核心 syscall。

**冲突说明**：当前 `SYSCALL_SHUTDOWN = 48` 占用了 `faccessat` 的编号。需要在放弃 `shutdown` 自定义编号或重新映射之间选择。

**选项 A**：移除 `shutdown(48)`，改用 `faccessat(48)`
**选项 B**：将 `shutdown` 移到其他编号，保留两者

**Linux 签名**：`int faccessat(int dirfd, const char *path, int mode, int flags)`

**实现要点**：解析路径 → 检查 dentry 存在性(F_OK) / 返回 0(root 全权限)

**改动文件**：`syscall/fs.rs`、`syscall/mod.rs`

**难度**：★★☆ | **预估**：30 分钟

---

#### 9. fchmodat (53)

**作用**：`chmod` 命令核心。修改文件权限位。

**Linux 签名**：`int fchmodat(int dirfd, const char *path, mode_t mode, int flags)`

**实现要点**：解析路径 → ext4 `set_inode_mode()` → 更新权限位

**改动文件**：`syscall/fs.rs`、`syscall/mod.rs`

**难度**：★★☆ | **预估**：30 分钟

---

#### 10. fchownat (54)

**作用**：`chown` 命令核心。修改文件所有者。

**Linux 签名**：`int fchownat(int dirfd, const char *path, uid_t owner, gid_t group, int flags)`

**实现要点**：解析路径 → ext4 `set_inode_uid()`/`set_inode_gid()`

**改动文件**：`syscall/fs.rs`、`syscall/mod.rs`

**难度**：★★☆ | **预估**：30 分钟

---

#### 11. utimensat (88)

**作用**：`touch` 命令核心。修改文件 atime/mtime。

**Linux 签名**：`int utimensat(int dirfd, const char *path, const struct timespec times[2], int flags)`

**初始实现**：stub 返回 0（假装成功，不做实际时间修改）
**后续增强**：通过 ext4 的 `set_inode_atime()`/`set_inode_mtime()` 写入时间戳

**改动文件**：`syscall/fs.rs`、`syscall/mod.rs`

**难度**：★☆☆（stub）/ ★★☆（完整） | **预估**：5 分钟（stub）

---

#### 12. umask (166)

**作用**：设置文件创建权限掩码。busybox 创建文件前调用。

**Linux 签名**：`mode_t umask(mode_t mask)`

**实现要点**：
```rust
pub fn sys_umask(mask: u32) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let old = inner.umask;
    inner.umask = mask & 0o777;
    old as isize
}
```

**改动文件**：`syscall/process.rs`、`syscall/mod.rs`、`task/task.rs`（新增 `umask` 字段）

**难度**：★☆☆ | **预估**：10 分钟

---

#### 13. mprotect (226)

**作用**：musl 设置内存页保护（rwx）。内核无实际页保护机制，stub 返回 0 即可。

**Linux 签名**：`int mprotect(void *addr, size_t len, int prot)`

**初始 stub**：
```rust
pub fn sys_mprotect(_addr: usize, _len: usize, _prot: usize) -> isize {
    0 // 所有内存可读写执行
}
```

**改动文件**：`syscall/process.rs`、`syscall/mod.rs`

**难度**：★☆☆ | **预估**：5 分钟

---

## Tier 4：Stub 清单

以下 syscall 均可返回 `-38`（`ENOSYS`）或 `0`，musl 有 fallback 机制或 busybox 不依赖。

| syscall | 编号 | stub 返回值 | 说明 |
|---------|------|-----------|------|
| `eventfd2` | 19 | `-38` | 事件通知机制 |
| `epoll_create1` | 20 | `-38` | epoll 多路复用 |
| `statfs` | 43 | `-38` | 文件系统统计（后续 `df` 命令需要） |
| `fstatfs` | 44 | `-38` | 同上 |
| `sendfile` | 71 | `-38` | 零拷贝文件发送 |
| `ppoll` | 73 | `-38` | 轮询 I/O，busybox sh 用阻塞 read |
| `set_robust_list` | 99 | `0` | 线程 robust mutex 列表 |
| `sched_getaffinity` | 123 | `-38` | CPU 亲和性，单核可忽略 |
| `prctl` | 167 | `0` | 进程控制（线程名等） |
| `sysinfo` | 179 | `-38` | 系统统计信息 |
| `madvise` | 233 | `0` | 内存使用建议 |
| `prlimit64` | 261 | `0` | 资源限制 |
| `getrandom` | 278 | `-38` | 随机数（musl fallback 弱随机） |
| `rseq` | 293 | `-38` | 可重启序列 |

---

## 实现顺序建议

```
Phase 1: Tier 2 全部（基础命令可用）
  ┌ lseek(62) ── newfstatat(79) ── fcntl(25)
  │ ioctl(29,stub) ── clock_gettime(113) ── readlinkat(78,stub)
  └ → 完成后 sh/ls/cat/echo/pwd/sleep 可用

Phase 2: Tier 3 全部 + Tier 4 批量 stub
  ┌ renameat2(276) ── faccessat(48) ── fchmodat(53)
  │ fchownat(54) ── utimensat(88,stub) ── umask(166)
  │ mprotect(226,stub)
  └ Tier 4 × 14 个 stub
    → 完成后 cp/mv/rm/touch/chmod/chown/test 可用

Phase 3: 加固与补全
  ┌ faccessat 编号冲突解决（shutdown 重新分配）
  │ ioctl 完整实现（termios 支持 ANSI 转义）
  │ renameat2 跨目录实现
  │ statfs/fstatfs 实现（df 命令）
  │ utimensat 完整实现（时间戳写入）
  └ newfstatat 完整 stat 结构体填充
```

---

## 数据结构变更汇总

### `TaskControlBlockInner` 需新增字段

```rust
pub umask: u32,          // umask(166) 文件创建权限掩码（初始值 0o022）
```

### `fd_table` 改造

```rust
// 当前：
fd_table: Vec<Option<Arc<dyn File + Send + Sync>>>

// 改造为（支持 cloexec）：
fd_table: Vec<Option<FdEntry>>

struct FdEntry {
    file: Arc<dyn File + Send + Sync>,
    cloexec: bool,       // fcntl F_SETFD 控制
}
```

### `OSInode` 需新增方法

```rust
fn get_offset(&self) -> usize;   // 读取 inner.offset（lseek SEEK_CUR 需要）
fn set_offset(&self, offset: usize);  // 已有 fn seek（lseek SEEK_SET 需要）
```

---

## 边界情况说明

| 场景 | 处理方式 |
|------|---------|
| 未知 syscall 编号 | 当前返回 `-1`（`EPERM`）。Tier 4 stub 加入后应改为逐编号明确返回 |
| `-1` vs `-38` | `-1` 在 Linux 上是 `EPERM`，musl 可能误判。stub 应返回 `-38`(`ENOSYS`) 或 `0` |
| `shutdown(48)` vs `faccessat(48)` 冲突 | Phase 3 时需将 `shutdown` 移至其他编号（建议 0xFF 或未用编号） |
| `sleep(300)` vs `clock_gettime(113)` + `nanosleep(101)` | `clock_gettime` 实现后可用 musl 标准 `sleep()`，废弃自定义 `sys_sleep` |
| 多线程 (clone flags) | 当前 `sys_clone` 行为同 `fork`，单线程 busybox 足够 |
| `AT_FDCWD = -100` | 保留，与当前实现一致 |

---

## 预估总工作量

| 阶段 | syscall 数 | 时间 |
|------|----------|------|
| Phase 1（Tier 2） | 6 个实现 | 3~4 小时 |
| Phase 2（Tier 3 + Tier 4） | 8 + 14 个 | 3~5 小时 |
| Phase 3（加固） | 若干 | 视需要 |
| **合计** | **~28 个新 syscall** | **1~2 天** |
