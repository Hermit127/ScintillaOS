# MachineTest 近期改动迁移说明

本文记录 2026-06-29 从 `MachineTest` 评测分支向本地 `main` 迁移的改动范围。官网提交已封板，远端 `MachineTest` 不再改动；这次迁移只用于把近期修复沉淀到本地 `main`，方便后续继续开发和复盘。

## 迁移来源

- 基础迁移点：`b5fc5cb2`。
- 额外包含：在封板后本地验证过的最后一版 `libcbench` 修复。
- 已故意保留 `main` 的 `user/src/bin/initproc.rs`，不迁移 `MachineTest` 自动评测入口。
- 未迁移磁盘镜像、测试输出日志、Docker 临时产物等构建/运行产物。
- 未执行 `push`，远端 `MachineTest` 应继续保持封板提交。

## 主要改动范围

### 评测链路与用户态工具

- 保留 `ltp_runner` 对官方 judge 兼容的 case 结算输出，确保 LTP case 能被评分脚本识别。
- 迁移近期为 basic、busybox、lua、libctest、ltp、lmbench、iozone 等评测链路补充的用户态 syscall 封装和辅助 runner 改动。
- `main` 仍然默认进入 shell，不自动启动比赛评测流程。

### 进程、线程与 futex 语义

- 补充线程组、tgid/tid、退出路径、wait/exit-group、clear_child_tid、robust futex list 等语义。
- 修复线程退出时的 futex 唤醒和 owner-died 标记，降低 `libcbench`、pthread/musl/glibc 相关测试卡死概率。
- 避免在清理已经退出的同地址空间线程时切换当前硬件页表，防止误伤仍在运行的同进程线程。

### 内存管理与 mmap 语义

- 迁移匿名映射、文件映射、MAP_FIXED/MAP_PRIVATE/MAP_SHARED、lazy fault、COW、munmap/mprotect/brk 等近期补充。
- 补充共享页表元数据同步，保证同线程组中的 page fault、COW 和映射变更能被其他线程看见。
- 为非当前任务增加 inactive recycle 路径，避免清理僵尸线程地址空间时把当前页表切到 retired page table。

### 文件系统、管道与 procfs

- 迁移 openat/fstat/statx/getdents/readlink/rename/link/symlink/unlink/mkdir/chdir/mount/umount 等语义修复。
- 补充 pipe、stdio、procfs 相关行为，修复 basic pipe、libctest 和部分 LTP 对 fd、路径、权限、目录项的依赖。
- 修复 stdout owner 可能滞留在已退出 task 上的问题，避免后续进程写 stdout 时自旋等待。

### 网络与 netperf 基础支持

- 迁移 main/MachineTest 中已经实现的本地 TCP/UDP socket、socketpair、poll/select 相关改动。
- 保留 virtio-net 驱动、socket syscall 和 loopback/本地连接路径的近期修复，作为后续 netperf 继续优化的基础。

### LoongArch 与 RISC-V 支持

- 迁移 LoongArch trap、pagetable、virtio block/net 相关兼容性修复。
- 保留 RISC-V 与 LoongArch 双架构构建路径中的公共内核改动。

## 最后一版 libcbench 修复说明

最后补入的本地 `libcbench` 修复重点在退出和清理路径：

- `MemorySet` 新增 inactive 回收路径，清理非当前任务地址空间时不再切换当前页表。
- `force_exit_task` 清理 peer thread 时使用 inactive recycle，避免同地址空间线程退出互相破坏页表。
- robust futex 与 clear_child_tid 访问用户地址前主动处理 lazy page fault，避免退出路径访问未落页用户地址失败。
- stdout acquire 会发现并清理已经不存在的 owner pid，避免输出锁被退出进程永久占用。
- fd close 路径减少持锁执行 drop 的风险，降低 benchmark 退出阶段死锁概率。

迁移前这版修复已在本地做过 RV/LA 聚焦验证，日志显示 `libcbench-musl`、`libcbench-glibc` 都能输出 END 并回到 `[initproc] === DONE ===`。迁移到 `main` 后仍需要重新做完整构建/长测验证。

## 后续建议

- 若继续开发，优先在 `main` 上验证 `cargo check`、`make kernel-rv`、`make kernel-la`。
- `MachineTest` 远端封板后不要再 push；如需比赛复测，应确认远端仍是 `b5fc5cb2`。
- 后续若要恢复自动评测入口，应在新的测试分支中改 `initproc`，不要直接污染 `main` 的 shell 入口。
