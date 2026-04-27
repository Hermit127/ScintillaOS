# Basic 测试运行流程

记录日期：2026/04/27 Hermit（更新）

---

## 前提条件

- 在 WSL 环境中操作
- 交叉编译器 `riscv64-linux-gnu-gcc` 已安装（用于编译 basic 测试程序）
- 内核已编译，ext4 镜像构建脚本就绪

## 当前 basic 测试未实现的 syscall

1. **mount** (syscall 40) — 未注册，未实现
2. **umount2** (syscall 39) — 未注册，未实现
3. **nanosleep** (syscall 101) — 未注册。sleep() 库函数内部用的是 nanosleep，不是 sys_sleep(300)

注意：
- 在 RISC-V Linux ABI 中，fork 不是独立 syscall。fork() 是库函数，内部调用 clone(SIGCHLD,0)
- syscall.c 里 fork() 调用 `syscall(SYS_clone, SIGCHLD, 0)`，clone() 通过 clone.s 汇编调用 `syscall(SYS_clone, flags, stack, ptid, tls, ctid)`
- 内核中 SYSCALL_CLONE=220 对应 sys_clone（委托给 sys_fork），SYSCALL_FORK=401 供用户自己的 Rust 程序使用

## 已实现的 syscall 对照 basic 测试

| 测试程序 | syscall(nr) | mod.rs | 实现函数 | 状态 |
|---------|------------|--------|---------|------|
| brk | SYS_brk(214) | SYSCALL_BRK | sys_brk | OK |
| chdir | SYS_chdir(49) | SYSCALL_CHDIR | sys_chdir | OK |
| clone | SYS_clone(220) | SYSCALL_CLONE | sys_clone→sys_fork | OK(最小) |
| close | SYS_close(57) | SYSCALL_CLOSE | sys_close | OK |
| dup | SYS_dup(23) | SYSCALL_DUP | sys_dup | OK |
| dup2 | SYS_dup3(24) | SYSCALL_DUP3 | sys_dup3 | OK |
| execve | SYS_execve(221) | SYSCALL_EXEC | sys_exec | OK |
| exit | SYS_exit(93) | SYSCALL_EXIT | sys_exit | OK |
| fork | SYS_clone(220) | SYSCALL_CLONE | sys_clone→sys_fork | OK(最小) |
| fstat | SYS_fstat(80) | SYSCALL_FSTAT | sys_fstat | OK |
| getcwd | SYS_getcwd(17) | SYSCALL_GETCWD | sys_getcwd | OK |
| getdents | SYS_getdents64(61) | SYSCALL_GETDENTS64 | sys_getdents64 | OK |
| getpid | SYS_getpid(172) | SYSCALL_GETPID | sys_getpid | OK |
| getppid | SYS_getppid(173) | SYSCALL_GETPPID | sys_getppid | OK |
| gettimeofday | SYS_gettimeofday(169) | SYSCALL_GET_TIME | sys_get_time | OK |
| mkdir_ | SYS_mkdirat(34) | SYSCALL_MKDIRAT | sys_mkdirat | OK |
| mmap | SYS_mmap(222) | SYSCALL_MMAP | sys_mmap | OK |
| mount | SYS_mount(40) | 无 | 无 | 未实现 |
| munmap | SYS_munmap(215) | SYSCALL_MUNMAP | sys_munmap | OK |
| open/openat | SYS_openat(56) | SYSCALL_OPENAT | sys_openat | OK |
| pipe | SYS_pipe2(59) | SYSCALL_PIPE2 | sys_pipe2 | OK |
| read | SYS_read(63) | SYSCALL_READ | sys_read | OK |
| sleep | SYS_nanosleep(101) | 无 | 无 | 未实现 |
| times | SYS_times(153) | SYSCALL_TIMES | sys_times | OK |
| umount | SYS_umount2(39) | 无 | 无 | 未实现 |
| uname | SYS_uname(160) | SYSCALL_UNAME | sys_uname | OK |
| unlink | SYS_unlinkat(35) | SYSCALL_UNLINKAT | sys_unlinkat | OK |
| wait/waitpid | SYS_wait4(260) | SYSCALL_WAITPID | sys_waitpid | OK |
| write | SYS_write(64) | SYSCALL_WRITE | sys_write | OK |
| yield | SYS_sched_yield(124) | SYSCALL_YIELD | sys_yield | OK |

## 操作流程

### 第1步：编译 basic 测试程序（首次或需要重新编译时）

```bash
cd testsuits-for-oskernel/basic/user
./build-oscomp.sh
```

编译产物在 `build/riscv64/`，包含：所有测试二进制、run-all.sh、text.txt。

**注意**：Makefile 的 `fs-img` 目标会检查 `build/riscv64/` 是否已有编译产物。如果已经编译好，不会重新 `clean binary`，直接使用现有二进制。

### 第2步：制作 ext4 镜像

直接跑 `make fs-img` 即可。Makefile 会自动：

1. 编译 user/src/bin 下的 Rust 用户程序
2. 将 basic 测试的预编译二进制（brk, chdir, clone...）、run-all.sh、text.txt 全部拷贝到 `rootfs/bin/`
3. 用 `mke2fs` 打包成 ext4 镜像

```bash
# 在 WSL 中，项目根目录下执行
make fs-img ARCH=riscv64
```

镜像中实际目录结构：
- `text.txt` → 根目录
- `bin/` → user Rust 程序 + basic 测试所有二进制 + run-all.sh
- `mnt/` → 挂载点目录

### 第3步：启动内核并运行测试

```bash
make run ARCH=riscv64
```

内核启动后进入 `user_shell`，逐一运行 basic 测试：

```
# 由于测试程序都在 /bin/ 下
/bin/brk
/bin/chdir
/bin/clone
/bin/close
/bin/dup
/bin/dup2
/bin/execve
/bin/exit
/bin/fork
/bin/fstat
/bin/getcwd
/bin/getdents
/bin/getpid
/bin/getppid
/bin/gettimeofday
/bin/mkdir_
/bin/mmap
/bin/mount
/bin/munmap
/bin/open
/bin/openat
/bin/pipe
/bin/read
/bin/sleep
/bin/times
/bin/umount
/bin/uname
/bin/unlink
/bin/wait
/bin/waitpid
/bin/write
/bin/yield
```

也可以用 shell 的 `cd` 内置命令（已支持）切换到 /bin 目录后执行：

```
cd /bin
./brk
./chdir
...
```

### 第4步：查看测试结果

每个测试输出格式：
```
========== START test_xxx ==========
... 测试内容 ...
========== END test_xxx ==========
```

## 预期结果

- **已实现的 syscall**：输出 "xxx success" / "xxx successfully" 等文字
- **未实现的 syscall**（mount/umount/nanosleep）：返回 -1，测试会报 error 但不影响后续测试执行
- **clone/fork 测试**：当前最小语义（fork 等效），child 返回 0，parent 通过 wait 回收

## shell 内置命令

- `cd <dir>` — 切换工作目录（调用 chdir 系统调用）
- 命令路径规则：
  - 以 `/` 开头 → 绝对路径，直接执行
  - 以 `./` 或 `../` 开头 → 相对路径，直接执行
  - 其他 → 拼接到 `/bin/` 下再执行

## 注意事项

1. 未实现的 syscall 不会 panic（mod.rs 的 `_ => -1`），测试能继续跑完
2. 如果 basic 测试二进制编译失败（缺少交叉编译器），`make fs-img` 会跳过 basic 测试的复制步骤，Rust 用户程序仍正常打包
3. clone 当前为最小语义（fork 等效），对 basic 测试够用，不支持多线程
4. 评测系统会在 QEMU 退出或超时后停止，测试跑完后需主动 shutdown
