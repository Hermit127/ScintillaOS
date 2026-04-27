# Basic 测试运行流程

记录日期：2026/04/27 Hermit

---

## 前提条件

- 在 WSL 环境中操作
- 内核已编译，ext4 镜像构建脚本就绪
- basic 测试程序已编译（位于 `testsuits-for-oskernel/basic/user/build/riscv64/`）

## 当前 basic 测试未实现的 syscall（共4个 + 1个编号已修复）

1. **mount** (syscall 40) — mod.rs 完全没有，函数体也完全没有
2. **umount2** (syscall 39) — mod.rs 完全没有，函数体也完全没有
3. **nanosleep** (syscall 101) — mod.rs 完全没有。sleep() 库函数内部调用的是 nanosleep，不是现有的 sys_sleep(300)
4. **clone** (syscall 220) — 已修复编号冲突。原来的 SYSCALL_FORK=220 改为 SYSCALL_CLONE=220，sys_clone 当前委托给 sys_fork 实现最小功能

注意：
- 在 RISC-V Linux ABI 中，fork 不是独立 syscall。fork() 是库函数，内部调用 clone(SIGCHLD,0)  
- syscall.c 里 fork() 调用 `syscall(SYS_clone, SIGCHLD, 0)`，clone() 通过 clone.s 汇编调用 `syscall(SYS_clone, flags, stack, ptid, tls, ctid)`
- 当前 `sys_clone` 实现为 fork 等效语义（忽略 flags/stack/ptid/tls/ctid 参数），对 basic 的 fork 和 clone 测试均可用

## 操作流程

### 第1步：编译 basic 测试程序（如果还没有）

```bash
cd testsuits-for-oskernel/basic/user
./build-oscomp.sh
```

编译产物在 `build/riscv64/`，包含所有测试二进制 + run-all.sh + text.txt。

### 第2步：制作 ext4 镜像（重点：目录结构要对）

basic 测试期望的 ext4 目录结构：
- `text.txt` → 根目录（测试会读这个文件）
- `basic/` → 目录，存放：
  - `run-all.sh` — 测试入口脚本
  - `brk`, `chdir`, `clone`, `close`, `dup`, `dup2`, `execve`, `exit`, `fork`, `fstat`, `getcwd`, `getdents`, `getpid`, `getppid`, `gettimeofday`, `mkdir_`, `mmap`, `mount`, `munmap`, `open`, `openat`, `pipe`, `read`, `sleep`, `times`, `umount`, `uname`, `unlink`, `wait`, `waitpid`, `write`, `yield` — 各测试二进制
- `bin/` → 存储 user/src/bin 的 Rust 用户程序

**用 Makefile 一步构建**：

当前 Makefile 已经通过 `OSCOMP_DIR` 变量（`testsuits-for-oskernel/basic/user`）把 OSCOMP 的编译产物拷到 rootfs。但要注意它会拷到 `rootfs/bin/` 而不是 `rootfs/basic/`。

最简单的方式是直接跑 `make fs-img`，然后手动把 basic 的目录结构调整：

```bash
# 在 WSL 中
make fs-img ARCH=riscv64

# 然后创建 basic 目录并拷贝
ROOTFS=target/riscv64gc-unknown-none-elf/release/rootfs
rm -rf $ROOTFS/basic
cp -r testsuits-for-oskernel/basic/user/build/riscv64 $ROOTFS/basic

# 重新打包 ext4 镜像
FSIMG=target/riscv64gc-unknown-none-elf/release/fs.img
rm -f $FSIMG
dd if=/dev/zero of=$FSIMG bs=1M count=128
mke2fs -t ext4 -b 4096 -F -d $ROOTFS $FSIMG
tune2fs -O ^has_journal $FSIMG 2>/dev/null || true
cp $FSIMG fs-img.img
```

### 第3步：启动内核运行测试

```bash
make run ARCH=riscv64
```

内核启动后进入 user_shell。在 shell 中手动运行 basic 测试：

```sh
cd /basic
./run-all.sh
```

如果想自动化（让内核启动后直接跑测试并关机），需要修改 `os/src/task/mod.rs` 中 INITPROC 的入口程序。

### 第4步：查看测试结果

每个测试的输出格式：
```
Testing brk :
========== START test_brk ==========
Before alloc,heap pos: ...
========== END test_brk ==========
Testing chdir :
...
```

- `========== START xxx ==========` 到 `========== END xxx ==========` 之间的内容会被评分器解析
- 外层需要 `#### OS COMP TEST GROUP START basic ####` 和 `#### OS COMP TEST GROUP END basic ####` 标记

basic_testcode.sh 脚本内容：
```sh
./busybox echo "#### OS COMP TEST GROUP START basic ####"
cd ./basic
./run-all.sh
cd ..
./busybox echo "#### OS COMP TEST GROUP END basic ####"
```

**注意**：你需要有 busybox 才能执行 basic_testcode.sh。或者直接在 shell 里跑 run-all.sh 并手动输出标记行。

## 预期结果

- **已实现的 syscall 测试**（brk/chdir/close/dup/dup2/execve/exit/fork/fstat/getcwd/getdents/getpid/getppid/gettimeofday/mkdir_/mmap/munmap/open/openat/pipe/read/times/uname/unlink/wait/waitpid/write/yield）：如果内核实现正确，输出 "xxx success" 之类的文字，测试通过
- **未实现的 syscall 测试**（mount/umount/sleep）：因为 `_ => -1` 现在已经不 panic 了，应该看到 "mount error" 或 "mmap error" 之类的失败输出，但不影响其他测试继续执行
- clone 测试：用当前 fork 等效语义，child 返回 0 后在 clone.s 中加载 func 执行，然后 exit。parent 通过 wait 回收。如果一切正常，输出 "clone process successfully"

## 注意事项

1. clone 当前实现是最小语义（fork 等效），不处理子进程栈切换。对 basic 的 clone.c 测试够用，但不支持多线程场景
2. mount/umount/nanosleep 返回 -1 不会 panic，测试会显示失败但不影响后续测试
3. 评测系统会在 QEMU 退出或超时后停止，所以测试跑完后要主动 `shutdown`
