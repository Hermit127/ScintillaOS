# BusyBox 接入所需系统调用

## 总览

| 层级 | 数量 | 说明 |
|------|------|------|
| Tier 1（启动必现） | 5 | musl `_start` 阶段调用，缺少则进程崩溃 |
| Tier 2（基础命令） | 8 | `sh`/`ls`/`cat`/`echo`/`pwd`/`sleep` 可用 |
| Tier 3（扩展命令） | 5 | `cp`/`mv`/`rm`/`touch`/`chmod`/`test` 可用 |
| Tier 4（stub 即可） | 12 | 返回 `-ENOSYS` 让 musl fallback |
| **合计** | **30** | |

---

## Tier 1：启动必现（musl `_start` 立即调用）

### 1. exit_group (94)

**作用**：musl 的 `exit()` / `_Exit()` 最终调此 syscall，不是 `exit(93)`。缺少数进程无法正常退出。

**Linux 签名**：`void exit_group(int status)`

**实现**：
```rust
pub fn sys_exit_group(status: i32) -> ! {
    // 当前内核无进程组概念，直接调用 sys_exit 逻辑
    sys_exit(status)
}
```

**难度**：★☆☆ | **预估**：10 分钟

---

### 2. set_tid_address (96)

**作用**：musl 初始化线程本地存储（TLS）时，将 futex 地址注册到内核。**缺少则在 `_start` 阶段直接崩溃**，是 musl 的硬依赖。

**Linux 签名**：`long set_tid_address(int *tidptr)`

**实现**：
```rust
// TaskControlBlockInner 新增字段
pub tid_ptr: usize,  // futex 唤醒地址（暂不实现 futex）

pub fn sys_set_tid_address(tidptr: *mut i32) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    inner.tid_ptr = tidptr as usize;
    task.getpid() as isize  // 返回 PID 作为 TID
}
```

**难度**：★☆☆ | **预估**：15 分钟

---

### 3-5. brk / mmap / munmap — 已有 ✓

| syscall | 编号 | 状态 |
|---------|------|------|
| `brk` | 214 | 已有 `sys_brk` |
| `mmap` | 222 | 已有 `sys_mmap` |
| `munmap` | 215 | 已有 `sys_munmap` |

**确认项**：`clone(220)` 现有的 `sys_clone` 需验证能否正确处理 musl 传入的 `CLONE_CHILD_SETTID`/`CLONE_VM` 等 flags。

---

## Tier 2：基础命令可用

完成 Tier 2 后，以下 busybox 命令可正常工作：

```
sh    ls    cat    echo    pwd    sleep    true    false
```

### 6. lseek (62)

**作用**：文件读写偏移控制。没有它，所有文件只能从偏移 0 顺序读。`cat` 勉强可用，但 shell 脚本中重定位、`ls` 读目录偏移等全依赖此。

**Linux 签名**：`off_t lseek(int fd, off_t offset, int whence)`

**实现**：
```rust
pub fn sys_lseek(fd: usize, offset: i64, whence: i32) -> isize {
    // SEEK_SET = 0, SEEK_CUR = 1, SEEK_END = 2
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() { return -9; } // EBADF
    let Some(file) = &inner.fd_table[fd] else { return -9; };
    let file_size = file.stat().st_size;
    let new_offset = match whence {
        0 => offset,
        1 => { /* 当前偏移 + offset */ },
        2 => file_size + offset,
        _ => return -22, // EINVAL
    };
    // OSInode 需要暴露 set_offset 方法
    file.set_offset(new_offset as usize);
    new_offset as isize
}
```

**OSInode 改动**：增加 `fn set_offset(&self, offset: usize)` 方法。

**难度**：★★☆ | **预估**：30 分钟

---

### 7. newfstatat (79)

**作用**：**musl 的 `stat()` 实现入口**。`ls` 列出文件大小/类型/权限全靠它。`test`、`find` 等也依赖。

**Linux 签名**：`int newfstatat(int dirfd, const char *path, struct stat *buf, int flags)`

**实现**：
```rust
pub fn sys_newfstatat(dirfd: isize, path: *const u8, stat_buf: *mut u8, flags: usize) -> isize {
    // AT_EMPTY_PATH = 0x1000: 允许对 fd 自身 stat（path 为空时使用 dirfd）
    // AT_SYMLINK_NOFOLLOW = 0x100: 不跟随符号链接
    if flags & 0x1000 != 0 {
        // 直接对 dirfd 执行 fstat
        return sys_fstat(dirfd as usize, stat_buf);
    }
    // 否则打开 path 获取 OSInode，调用 stat()
    let token = current_user_token();
    let path = translated_str(token, path);
    // ... 类似 sys_openat 的路径解析，但不创建 fd
    // 获取 stat，写入 stat_buf，关闭临时 inode
}
```

**难度**：★★☆ | **预估**：1 小时

---

### 8. fcntl (25)

**作用**：busybox 对每个 fd 调 `F_GETFD`/`F_SETFD` 设置 `FD_CLOEXEC`。缺少时 busybox 的 fd 管理会出错。

**Linux 签名**：`int fcntl(int fd, int cmd, ...)`

**最小实现**：
```rust
pub fn sys_fcntl(fd: usize, cmd: usize, arg: usize) -> isize {
    match cmd {
        // F_GETFD = 1
        1 => { /* 返回该 fd 的 O_CLOEXEC 标志 */ },
        // F_SETFD = 2
        2 => { /* 设置 O_CLOEXEC */ },
        // F_DUPFD = 0
        0 => { /* 同 dup，但从 arg 起分配 */ },
        // F_DUPFD_CLOEXEC = 1030
        1030 => { /* dup + set CLOEXEC */ },
        _ => -22, // EINVAL
    }
}
```

**TaskControlBlockInner 改动**：`fd_table` 增加对应的 `cloexec` 标志。

**难度**：★★☆ | **预估**：1 小时

---

### 9. ioctl (29) — 初始 stub

**作用**：busybox 对 stdin/stdout 调 `TCGETS` 获取终端属性。stub 版本返回 `-ENOTTY` 也能让 busybox 继续跑。

**Linux 签名**：`int ioctl(int fd, unsigned long request, ...)`

**初始 stub**：
```rust
pub fn sys_ioctl(_fd: usize, _request: usize, _arg: usize) -> isize {
    -25 // ENOTTY — 不是终端
}
```

**后续增强**：对 stdin/stdout(0/1) 返回固定 termios 结构体。

**难度**：★☆☆（stub）/ ★★★（完整） | **预估**：10 分钟（stub）

---

### 10. clock_gettime (113)

**作用**：`sleep`/`timeout` 命令需要。sh 脚本中的 `sleep N` 依赖此 syscall。

**Linux 签名**：`int clock_gettime(clockid_t clk_id, struct timespec *tp)`

**实现**：
```rust
pub fn sys_clock_gettime(clk_id: usize, tp: *mut u8) -> isize {
    let now = polyhal::time::Time::now();
    let msec = now.to_msec() as usize;
    let ts = TimeSpec {
        tv_sec: msec / 1000,
        tv_nsec: (msec % 1000) * 1_000_000,
    };
    let token = current_user_token();
    *translated_refmut(token, tp as *mut TimeSpec) = ts;
    // CLOCK_REALTIME = 0, CLOCK_MONOTONIC = 1
    0
}
```

**难度**：★★☆ | **预估**：30 分钟

---

### 11. getuid / geteuid / getgid / getegid (174-177)

**作用**：busybox 做权限检查（判断是否 root）。

**实现**：
```rust
pub fn sys_getuid() -> isize { 0 }   // root
pub fn sys_geteuid() -> isize { 0 }
pub fn sys_getgid() -> isize { 0 }
pub fn sys_getegid() -> isize { 0 }
```

**难度**：★☆☆ | **预估**：5 分钟

---

### 12. gettid (178)

**作用**：musl 获取线程 ID。单线程等同 pid。

**实现**：
```rust
pub fn sys_gettid() -> isize {
    sys_getpid()
}
```

**难度**：★☆☆ | **预估**：5 分钟

---

### 13. readlinkat (78) — 初始 stub

**作用**：符号链接解析。目前 ext4 不支持 symlink。

**初始 stub**：
```rust
pub fn sys_readlinkat(_dirfd: isize, _path: *const u8, _buf: *mut u8, _bufsiz: usize) -> isize {
    -22 // EINVAL — 不是符号链接
}
```

**后续**：ext4 支持 symlink 后可实现真实读取。

**难度**：★☆☆（stub） | **预估**：5 分钟

---

## Tier 3：更多命令可用

完成 Tier 3 后，以下命令额外可用：

```
cp    mv    rm    touch    chmod    test
```

### 14. renameat2 (276)

**作用**：**`mv` 命令核心**。跨目录移动文件需要。

**Linux 签名**：`int renameat2(int olddirfd, const char *oldpath, int newdirfd, const char *newpath, unsigned int flags)`

**实现**：
```rust
pub fn sys_renameat2(olddirfd: isize, oldpath: *const u8,
                     newdirfd: isize, newpath: *const u8, flags: usize) -> isize {
    // 先实现 flags=0 的简单 rename（同目录内改名）
    // 1. 解析 oldpath → old_dentry
    // 2. 解析 newpath 的父目录 → new_parent_dentry + new_name
    // 3. 检查 old 和 new 在同一文件系统
    // 4. ext4 层面执行 rename（如果 ext4_rs 支持 link/unlink 组合）
    //
    // flags: RENAME_NOREPLACE=1, RENAME_EXCHANGE=2
    -22  // 暂时返回 EINVAL，逐步实现
}
```

**难度**：★★★ | **预估**：2 小时

---

### 15. faccessat (48)

**作用**：`test -f`、`test -d`、`test -r` 等命令依赖。

**Linux 签名**：`int faccessat(int dirfd, const char *path, int mode, int flags)`

| mode | 值 | 检查 |
|------|---|------|
| `F_OK` | 0 | 文件是否存在 |
| `R_OK` | 4 | 是否可读 |
| `W_OK` | 2 | 是否可写 |
| `X_OK` | 1 | 是否可执行 |

**实现**：
```rust
pub fn sys_faccessat(dirfd: isize, path: *const u8, mode: usize, flags: usize) -> isize {
    // AT_EACCESS = 0x200: 用有效 UID 而非实际 UID
    // 1. 解析 path → dentry
    // 2. F_OK: dentry 存在则返回 0
    // 3. R_OK/W_OK/X_OK: 在当前 root 模型下总是允许（返回 0）
    // 4. 文件不存在则返回 -ENOENT(2)
    0  // root 拥有全部权限
}
```

**难度**：★★☆ | **预估**：30 分钟

---

### 16. utimensat (88) — stub

**作用**：`touch` 命令修改文件时间戳。

**初始 stub**：
```rust
pub fn sys_utimensat(_dirfd: isize, _path: *const u8, _times: *const u8, _flags: usize) -> isize {
    0  // 假装成功，不做时间修改
}
```

**后续**：通过 ext4 设置 inode 的 atime/mtime。

**难度**：★☆☆（stub） | **预估**：5 分钟

---

### 17. umask (166)

**作用**：文件创建权限掩码。

**实现**：
```rust
pub fn sys_umask(mask: u32) -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let old = inner.umask;
    inner.umask = mask & 0o777;
    old as isize
}
```

**TaskControlBlockInner 新增**：`pub umask: u32`（初始值 `0o022`）。

**难度**：★☆☆ | **预估**：10 分钟

---

### 18. mprotect (226) — stub

**作用**：musl 设置内存页保护（rwx）。由于内核无实际页保护，stub 即可。

**stub**：
```rust
pub fn sys_mprotect(_addr: usize, _len: usize, _prot: usize) -> isize {
    0  // 所有内存可读写执行
}
```

**难度**：★☆☆ | **预估**：5 分钟

---

## Tier 4：stub 清单

以下 syscall 均可返回 `-38`（`ENOSYS`），musl 有 fallback 或 busybox 不依赖。

| syscall | 编号 | stub 返回值 | 原因 |
|---------|------|-----------|------|
| `set_robust_list` | 99 | `0` | 线程 robust mutex 列表。单线程 busybox 不需要，但 musl 调了 |
| `getrandom` | 278 | `-38` | 栈 canary 随机数。musl 失败后会 fallback 到弱随机 |
| `ppoll` | 73 | `-38` | busybox sh 用阻塞 read，不用 poll |
| `epoll_create1` | 291 | `-38` | 简单场景不需要 epoll |
| `eventfd2` | 290 | `-38` | 非必需 |
| `prctl` | 167 | `0` | 线程名称等控制，返回成功即可 |
| `prlimit64` | 261 | `0` | 资源限制，返回成功 |
| `sysinfo` | 179 | `-38` | 系统信息，busybox 可降级 |
| `statfs` | 43 | `-38` | 文件系统统计，`df` 命令需要（后续实现） |
| `fstatfs` | 44 | `-38` | 同上 |
| `sched_getaffinity` | 123 | `-38` | CPU 亲和性，单核场景可忽略 |
| `rseq` | 293 | `-38` | 可重启序列，musl 尝试注册，失败则跳过 |
| `riscv_hwprobe` | 258 | `-38` | RISC-V 硬件探测，非必需 |

---

## 实现顺序

```
Phase 1: Tier 1 全部 + Tier 2 全部
  ┌ exit_group(94) ── set_tid_address(96) ── lseek(62)
  │ newfstatat(79) ── fcntl(25) ── ioctl(29,stub)
  │ clock_gettime(113) ── getuid*4(174-177) ── gettid(178)
  └ readlinkat(78,stub)
    → BusyBox sh 可用

Phase 2: Tier 3 全部 + Tier 4 stub 批量
  ┌ renameat2(276) ── faccessat(48) ── utimensat(88,stub)
  │ umask(166) ── mprotect(226,stub)
  └ Tier 4 × 13 个 stub
    → cp/mv/rm/touch/test 可用

Phase 3: 加固与补全
  ┌ ioctl 完整实现（termios）
  │ renameat2 完整实现（跨目录）
  │ utimensat 完整实现（时间戳写入）
  │ statfs/fstatfs 实现（df 命令）
  └ ppoll 实现（可选）
```

---

## 数据结构变更汇总

### `TaskControlBlockInner` 新增字段

```rust
pub tid_ptr: usize,      // set_tid_address(96) 设置的 futex 地址
pub umask: u32,          // umask(166) 文件创建权限掩码
```

### `OSInode` 新增方法

```rust
fn set_offset(&self, offset: usize);
fn get_offset(&self) -> usize;  // 已有 inner.offset
```

### `fd_table` 配套

每个 fd 增加 `cloexec: bool` 标志（随 `fcntl F_SETFD` 控制）。

---

## 边界情况处理

| 场景 | 处理 |
|------|------|
| 未知 syscall 编号 | 当前返回 `-1`。Tier 4 stub 加入后，所有 Linux 常见编号都有明确返回值 |
| `-1` vs `-EPERM` | `-1` 是 `EPERM`(不允许)，musl 可能误判。对 stub 应返回 `-38`(ENOSYS) |
| 多线程 (clone flags) | 当前内核无真线程支持，`clone` 行为同 `fork`。busybox 单线程足够 |
| `AT_FDCWD = -100` | 保留，与当前一致 |

---

## 预估总工作量

| 阶段 | syscall 数 | 时间 |
|------|----------|------|
| Phase 1（Tier 1+2） | 13 个 | 4~6 小时 |
| Phase 2（Tier 3+4） | 18 个 | 2~4 小时 |
| Phase 3（加固） | 若干 | 视需要 |
| **合计** | **~30 个** | **1~2 天** |
