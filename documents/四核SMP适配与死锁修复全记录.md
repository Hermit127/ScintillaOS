# 四核 SMP 适配与死锁修复全记录

## 1. 范围与结论口径

本文记录 ScintillaOS 从 `main` 基线 `3bd02c7021b31a57fb9ce23c5af46ff4360738f1`
到 `smp-riscv4` 工作树的四核适配、死锁诊断、修复和验证。固定验收环境为
RISC-V QEMU `virt`、`-smp 4`、`-m 256M`、两块 VirtIO block 盘。

“四核通过”必须同时满足：

1. 启动输出完整的 `[smp] online mask 0xf`；
2. `smpstress` 输出 `observed=0xf`，证明四个 hart 实际执行用户任务；
3. 同一 TCB 不能同时由两个 hart 拥有，也不能同时位于 running、ready 和 wait 队列；
4. 不出现 lockdep、队列所有权、panic 或停滞诊断；
5. 官方脚本、ELF 和参数不修改，musl/glibc 测试自然输出 `END`；
6. 输入盘运行前后 SHA-256 相同。

截至 2026-07-25，本轮最终普通 release 内核 SHA-256 为
`65d932015f85035edb905464387bba5eb55abd22bf21219ccce00e880a577c50`。
最新内核已完成 20 轮四核定向冷启动、musl/glibc BusyBox 与 lmbench 各三轮、
musl/glibc libc-test 各三轮；所有官方脚本均自然输出测试组 `END`，没有
`SKIP`、panic、Segmentation fault、lockdep 或停滞判定。musl libc-test 每轮
217/217 Pass；glibc 运行同一套以 musl 语义为基准的 libc-test 时有稳定的
实现差异，本文在 8.4 节如实列出，不把这些 FAIL 伪装成内核通过。

## 2. 四核启动和调度设计

### 2.1 拓扑与每核状态

- `os/src/config.rs` 的 `MAX_CPUS = 4` 是内核唯一的最大 hart 数定义。
- 每个 hart 有独立 `CpuLocal`：当前 TCB、idle 上下文和 tick 计数均不共享。
- boot protocol 的 hart ID 与 polyhal per-CPU base 建立一次性映射，调度器不再
  依赖次级 hart 上不可靠的旧 `hart_id()` 返回值。
- 启动采用 release/acquire 发布：次级 hart 初始化自己的 per-CPU 状态后等待
  hart 0 完成堆、VFS、设备和页表初始化，再注册 online bit。
- 最后一个 online hart 输出 `online mask 0xf`。控制台现在对一次完整格式化输出
  加锁，不再发生多核逐字节交织和 END 标记破坏。

### 2.2 TCB 唯一所有权

每个 TCB 维护两个独立原子不变量：

- `enqueued`：是否存在且只存在一个 ready-queue 条目；
- `running_cpu`：当前运行 hart，空闲值为 `usize::MAX`。

`fetch_task()` 在 `SchedulerLock -> TaskLock` 下完成
`Ready -> Running` 和 `running_cpu` 发布。任务让出、阻塞或退出时不提前放回
ready queue；只有 context switch 已把寄存器保存到 idle 上下文之后，
`complete_context_switch()` 才发布下一状态。这关闭了“旧上下文尚未保存，
另一 hart 已恢复同一 TCB”的窗口。

启动日志中 hart 0 和 hart 1 可能先后打印同一个 `tid=1/tcb=...`。这本身不表示
并发重复运行：init 进程可在首次 timer preemption 后迁移，日志是每个 hart 的
“第一次 dispatch”而非同一时间快照。真正的并发冲突会被 `running_cpu` 的
compare-exchange 直接 panic；本轮 `smpstress` 还观察到四个 hart 分别执行不同
worker，`observed=0xf`。

### 2.3 IPI 和远端 TLB

- ready work 发布后，目标 idle hart 通过 SBI IPI 被唤醒；
- PENDING_IPI 在发送前 release 发布，在 SSIP handler 中 acq_rel 消费；
- 地址空间记录 active CPU mask；
- `munmap`、COW 权限变化和共享 VM 元数据变化先修改 PTE，再对其他 active hart
  执行 SBI remote `sfence.vma`；
- 退休 frame 在远端刷新完成后才释放，避免另一 hart 用陈旧 TLB 访问已复用页。

## 3. 统一阻塞状态机

阻塞状态转换为：

```text
Running -> Blocking -> Blocked -> Ready
                    \-> WakePending -> Ready
```

协议顺序是：

1. 在 SchedulerLock 下把任务标记为 `Blocking` 并登记 wait source；
2. 释放调度锁后重新检查对象条件；
3. 条件已满足则 `cancel_block_before_switch()` 恢复 Running；
4. 条件仍不满足才切换；
5. 并发 wake 看到 `Blocking` 时写 `WakePending`，idle handoff 在上下文保存完成后
   才加入 ready queue。

pipe 使用此协议覆盖空管道、满管道、关闭端和跨 hart ping-pong。wait4、futex、
signal wait 和带 deadline 的睡眠共享相同的 scheduler 状态机。仍使用
`suspend_current_and_run_next()` 的 stdin、poll 和部分网络路径属于
“重新轮询式 yield”，不会登记后睡死，因此没有 lost wakeup，但会增加调度开销；
它们是后续迁移为事件等待队列的性能风险，不应被误记成已完成的事件化阻塞。

## 4. release 可用的锁诊断

`smp-diagnostics` feature 在优化构建中启用 lockdep。锁顺序为：

```text
CpuLocal(0) -> PidRegistry(1) -> Futex(2) -> Scheduler(3)
-> Task(4) -> SignalActions(5) -> Vm(6) -> FrameRef(7)
-> FrameAllocator(8) -> Vfs(9) -> FsGlobal(10) -> Fs(11)
-> Dentry(12) -> FsAux(13) -> Device(14)
```

lockdep 为每个持有项记录：

- lock class、实例地址、静态名称；
- 获取文件和行号；
- 当前 hart 和持有深度。

它禁止同实例递归、等级逆序和同等级多实例的地址逆序。切换、阻塞和 idle `wfi`
前断言没有遗留 domain lock。停滞快照包括每 hart 的 phase、TID/TGID、TCB、
pending IPI、running count、ready/wait/sleep 队列、任务状态、阻塞原因和持锁栈。

owner 原子只用于诊断快照，底层 `spin::Mutex` 是唯一互斥裁决。递归和 ABBA
由每 hart lock stack 判断，不能用一个与真实 mutex 分开更新的 owner 原子作为
第二套互斥状态。

## 5. 本轮复现并修复的问题

| 编号 | 复现条件和调用链 | 根因 | 修复 |
|---|---|---|---|
| D1 | 四核启动，VirtIO request | 先持 bounce DeviceLock，再取 block DeviceLock，同等级顺序不稳定 | 固定先锁设备，再锁 bounce |
| D2 | 双盘 VFS guard 析构 | guard 字段按声明顺序析构，先释放了全局 lwext4 锁 | 字段重排，保证 local Fs 先释放 |
| D3 | COW/frame 回收 | frame refcount 与 buddy allocator 共用同一 lock class | 拆成 FrameRef 与 FrameAllocator 两级锁；加入 SignalActions 后最终 rank 为 7/8 |
| D4 | wait4 回收子进程 | 父 TaskLock 内检查/析构子 TCB，形成 Task->Task/PID | 移出子 Arc，释放父锁后快照和回收 |
| D5 | exit/exec/close pipe fd | TaskLock 内调用 `File::close_ref()`，回调进入 Pipe/Scheduler/CpuLocal | TaskLock 内只 move fd，锁外调用 close callback |
| D6 | fork COW | 同时持 parent/child `Vm.address_space_areas`，地址顺序不稳定 | child 尚未发布，先在局部 Vec 构造，释放 parent 后一次写入 child |
| D7 | `run_pipe_test` | PipeRingBuffer 内直接写用户 slice；COW store fault 进入 trap 后请求 CpuLocal | 预分配 kernel scratch，锁内只读写 scratch，锁外复制用户内存 |
| D8 | signal final test/进程退出 | 持 INITPROC TaskLock 时逐个取 orphan child TaskLock | 分别更新 child parent，全部释放后再单独更新 INITPROC children |
| D9 | BusyBox `mkdir/cp` | 持 FsGlobal/Fs 时更新较低等级 VfsPathOwnerCache | `with_fs` 返回后再更新 owner cache |
| D10 | SIGCHLD/BusyBox | TaskLock 内写跨页 rt_sigframe，缺页 trap 递归取 TaskLock | 写 frame 前预解析并补齐覆盖页 |
| D11 | `rt_sigaction` | signum 35..64 被伪装成成功；用户指针在 TaskLock 内访问 | 超出实现范围返回 EINVAL；用户 action/old action 在 TaskLock 外复制 |
| D12 | 多核串口 | os console 逐字符直接 putchar | 复用 polyhal 整次格式化输出锁 |
| D13 | glibc `pthread_cancel` | `CLONE_SIGHAND` 线程仍各自复制 signal action，安装和投递跨 hart 竞态 | signal disposition 改为 `Arc<SignalActionsLock<_>>`；fork 深拷贝、`CLONE_SIGHAND` 共享、exec 解除共享并重置 |
| D14 | glibc cancellation handler | `rt_sigaction.sa_mask` 的用户 ABI 布局与内核内部结构直接混用；`siginfo.si_code/pid/uid` 缺失 | 使用独立 `UserSignalAction` 转换；pending signal 保存 `SI_USER/SI_TKILL` 和发送者身份 |
| D15 | glibc signal unwind | signal 返回地址指向不可读的伪地址，unwinder 取指失败 | 每个 ELF 地址空间映射只读可执行的 RISC-V `rt_sigreturn` trampoline 页 `0x7ffeff000` |
| D16 | musl `pthread_cancel` | RISC-V `ucontext_t` 的 `mcontext` 偏移误写为 168，musl 修改的 PC 与内核恢复的 PC 相差 8 字节 | 按 RISC-V Linux ABI 对齐为 176，完整保存/恢复 32 GPR、PC 和 mask |
| D17 | musl `pthread_cond` 多等待者 | 普通共享 `FUTEX_WAIT` 擅自根据数值推断 owner 已退出，把 musl `__thread_list_lock` 从 TID 改成 `0x40000000` | 删除普通 wait 的 owner-gone 猜测；`FUTEX_OWNER_DIED` 只由已注册 robust-list 的退出清理发布 |
| D18 | futex/requeue 诊断 | 停滞快照只能看到任务状态，不能还原 wait/wake/clear-tid 时序 | diagnostics feature 增加 128 项 futex 事件环、key、原始 op、返回值和 waiter dump；正式内核不输出高频日志 |
| D19 | libc-test `utime` | `tmpfile()` 返回匿名 `TmpFile`，`futimens` 只接受 `OSInode`，错误返回 EBADF | 匿名文件保存独立 64 位秒/纳秒 stat；实现 `UTIME_NOW/OMIT`、范围校验和 Y2038 时间 |
| D20 | normal release 构建 | 仅 diagnostics 存在的 futex dump 被普通 RISC-V 停滞函数无条件引用 | 调用点按 feature 编译，普通和 diagnostics release 均独立检查与构建 |

pipe 修复后原始 `run_pipe_test` 连续执行 4 次，每次包含 5 个
`pipe_large_test` 随机数据迭代，共 20 个内部迭代全部自然返回 prompt。

### 5.1 `pthread_cond` 死锁的完整证据链

失败现场最初看起来像条件变量在 private/shared futex 间不一致。未剥离的同工具链
分析副本把地址 `0x9db80` 定位为 musl `__thread_list_lock`，而非测试局部
`pthread_cond_t`。低扰动事件环记录到：

```text
tid 6/7: FUTEX_WAIT(__thread_list_lock, expected=8)
tid 8:   CLONE_CHILD_CLEARTID, 8 -> 0
tid 6/7: FUTEX_WAIT(__thread_list_lock, expected=0x40000000)
```

错误来自内核 `mark_futex_owner_died()`：它在任何共享 `FUTEX_WAIT` 上都把 futex
低 30 位当作 owner TID。等待者发现 TID 8 进入 Zombie/被移出 PID 表后，把普通
musl 内部锁改写成 `FUTEX_OWNER_DIED`，与正确的 clear-tid 清零竞争。Linux 只会
在已注册 robust-list 的退出遍历中设置 owner-died；普通 futex 数值没有 owner
语义。删除该非标准路径后，官方 musl 静态和动态 `pthread_cond` 在四核冷启动下
连续 20 轮、共 40 次全部通过。

## 6. 从 main 开始的原 44 文件四核增量

下表是实施本轮计划前已确认的 44 文件增量；本轮新增的 console、自动化脚本和
本文档列在下一节。表中“作用”描述该文件在四核链路中的职责，不表示每个文件
都是本轮新写。

| 文件 | 作用 |
|---|---|
| `Makefile` | SMP 数量、双盘 QEMU 和构建入口 |
| `artifacts/lmbench-validation/VALIDATION.md` | 原 lmbench/BusyBox 验证证据 |
| `basic/user/lib/syscall_ids.h` | SMP 定向程序所需 syscall ID |
| `os/Cargo.toml` | 内核依赖与 feature |
| `os/src/config.rs` | CPU 数和内存常量 |
| `os/src/drivers/block/virtio_blk.rs` | 块设备共享队列、bounce 锁 |
| `os/src/drivers/net/virtio_net.rs` | 网卡共享状态 |
| `os/src/fs/block_device_stream.rs` | lwext4 block stream 串行化 |
| `os/src/fs/inode.rs` | inode/fd 并发访问 |
| `os/src/fs/pipe.rs` | pipe ring、引用计数和阻塞唤醒 |
| `os/src/fs/proc.rs` | `/proc` SMP 快照 |
| `os/src/fs/stdio.rs` | stdin/stdout 多任务访问 |
| `os/src/fs/vfs.rs` | VFS、双盘 lwext4 锁层次 |
| `os/src/main.rs` | 次级 hart 启动和 trap/IPI |
| `os/src/mm/frame_allocator.rs` | frame allocator/refcount 并发 |
| `os/src/mm/memory_set.rs` | COW、共享 VM、active CPU、退休页 |
| `os/src/mm/mod.rs` | MM 导出和页故障接口 |
| `os/src/net/mod.rs` | 网络初始化/轮询 |
| `os/src/net/socket.rs` | socket 共享对象 |
| `os/src/net/syscall.rs` | 网络阻塞和 syscall |
| `os/src/sync/lock.rs` | domain locks 与 lockdep |
| `os/src/sync/mod.rs` | 锁类型导出 |
| `os/src/sync/up.rs` | 旧单核 cell 兼容边界 |
| `os/src/syscall/fs.rs` | fd table 与 close 生命周期 |
| `os/src/syscall/process.rs` | fork/clone/exec/wait/futex/signal/mmap |
| `os/src/task/manager.rs` | per-CPU ready queue、wait queue、状态机 |
| `os/src/task/mod.rs` | 调度、退出、信号和阻塞公共接口 |
| `os/src/task/pid.rs` | PID registry 并发 |
| `os/src/task/processor.rs` | CpuLocal、dispatch、IPI、TLB |
| `os/src/task/task.rs` | TCB、运行/排队所有权、地址空间 |
| `user/Makefile` | 定向 SMP 测试构建 |
| `user/src/bin/affinitytest.rs` | 四核 affinity 验证 |
| `user/src/bin/clonesmptest.rs` | clone + 远端 TLB 验证 |
| `user/src/bin/cpuinfotest.rs` | online mask 验证 |
| `user/src/bin/futextest.rs` | futex mismatch/timeout |
| `user/src/bin/mprotecttest.rs` | mprotect 与缺页 |
| `user/src/bin/shmtest.rs` | 共享内存 |
| `user/src/bin/smpstress.rs` | 四 hart 实际用户执行 |
| `user/src/lib.rs` | 用户库 syscall/信号/CPU 接口 |
| `user/src/syscall.rs` | 用户 syscall 封装 |
| `vendor/polyhal-trap/src/trap/mod.rs` | trap 回调扩展 |
| `vendor/polyhal-trap/src/trap/riscv64.rs` | RISC-V timer/SSIP/trap |
| `vendor/polyhal/src/mem.rs` | SMP-safe bootstrap allocator |
| `vendor/virtio-drivers/src/queue.rs` | VirtIO queue 并发修正 |

## 7. 本轮新增文件和额外变动

- `os/src/console.rs`：整条格式化输出跨核串行。
- `scripts/validate_smp4.py`：构建可选、双盘 qcow2 overlay、冷启动、串口交互、
  输入哈希、END 核对、禁止标记、宿主 QEMU monitor dump 和证据清单。
- `documents/四核SMP适配与死锁修复全记录.md`：本文。
- `smp-diagnostics`：release lockdep、获取位置和停滞快照。
- Makefile：`run-smp4`、`run-dual-smp4`、`run-dual-smp4-prebuilt`，并把
  `SMP_DIAGNOSTICS` 传入子 make。
- signal ABI：共享 signal disposition、完整 siginfo 来源、真实 trampoline 和
  RISC-V `ucontext` 偏移 176。
- futex：统一 key/wait 状态机、退出 clear-tid、robust-list 和 diagnostics
  事件环；删除普通 wait 的伪 owner 修复。
- 匿名文件时间：`TmpFile` stat 与 `futimens/utimensat` 的 64 位时间语义。

### 7.1 可信测试基线

libc-test 源码和工具链来自赛事镜像
`docker.educg.net/cg/os-contest:20250624`。musl 使用
`riscv64-buildroot-linux-musl-`，glibc 使用赛事 RISC-V glibc 工具链；动态盘
同时放入对应 PT_INTERP、libc 和 `libgcc_s`。包装器与 ELF 不修改，运行时通过
`chroot /mnt/{musl,glibc}` 保持其绝对加载器路径。最终只读输入哈希：

| 输入 | SHA-256 |
|---|---|
| 最终 `kernel-rv` | `65d932015f85035edb905464387bba5eb55abd22bf21219ccce00e880a577c50` |
| root fs | `ae7dfd63cf838a0e19c24ac4ff9e8ee45c99020e2df7a6d07964f93457ce5b43` |
| 官方 BusyBox/lmbench 合并盘 | `fe72922a5c0b49ee10665e1a7b8ab81445cb4d6bf4b7bd6de3067e128a9b05ad` |
| 官方 musl libc-test 盘 | `1793d1345c61353fca60aa5d4cddd361ddc7d6de96a6466432156e190420940f` |
| 官方 glibc libc-test 盘 | `090d40b3058994b96b654daf80432af5527f9a8373f171c47a112a4fa8758316` |

验证器为 root/test 两盘各建一个 qcow2 overlay，基础镜像从不作为可写盘启动。
整个盘的哈希固定了脚本、ELF 和库的字节内容；lmbench 文件级哈希继续记录在
`artifacts/lmbench-validation/VALIDATION.md`。

## 8. 验证结果

### 8.1 构建和四核定向测试

- `git diff --check` 通过；
- RISC-V 普通 release 与 `smp-diagnostics` release 的 cargo check、完整构建通过；
- 最终普通 release 定向冷启动 20/20：
  `artifacts/smp4-validation-final-directed-20260725-r1/`；
- 每轮均看到 `online mask 0xf` 和
  `smpstress passed: online=0xf, observed=0xf`；
- `cpuinfotest`、`affinitytest`、`clonesmptest`、`futextest`、
  `mprotecttest`、`shmtest`、`run_pipe_test`、`sig_simple`、`sig_simple2`
  每轮自然完成；
- diagnostics 定向冷启动 20/20：
  `artifacts/smp4-validation-directed-diagnostics-20260725-r2/`；
- 修复 D17 后，官方 musl 静态/动态 `pthread_cond` 冷启动 20/20：
  `artifacts/smp4-validation-libctest-musl-pthread-cond-both-20260725-r2/`。

所有上述日志均无重复运行同一 TCB 的所有权断言、lockdep、panic、停滞或
Segmentation fault。

### 8.2 官方 BusyBox 和 lmbench

最终内核的正式三轮结果：

| 测试 | 结果 | 证据目录 |
|---|---:|---|
| musl BusyBox | 3/3 END | `artifacts/smp4-validation-final-busybox-musl-20260725-r2/` |
| glibc BusyBox | 3/3 END | `artifacts/smp4-validation-final-busybox-glibc-20260725-r2/` |
| musl lmbench | 3/3 END | `artifacts/smp4-validation-final-lmbench-musl-20260725-r2/` |
| glibc lmbench | 3/3 END | `artifacts/smp4-validation-final-lmbench-glibc-20260725-r2/` |

脚本内容、命令、`-P/-N` 参数均未改变，没有跳过 `lat_pipe`、绑核或单核回退。
`/code/lmbench_src/bin/build`、PATH 中的 BusyBox `cp` 和校准环境变量只是恢复
官方包装器要求的赛事目录布局，不改变测试列表或并发度。

### 8.3 官方 musl libc-test

- diagnostics 完整轮次：
  `artifacts/smp4-validation-official-libctest-musl-diagnostics-20260725-r2/`，
  217 START、217 Pass、217 END；
- 最终普通 release 三次冷启动：
  `artifacts/smp4-validation-final-libctest-musl-20260725-r2/`；
- 三轮每轮都是 217 START、217 Pass、217 END，测试组 END 正常；
- 没有 timeout、FAIL、SKIP、panic、Segmentation fault 或 lockdep。

### 8.4 glibc 上运行 musl libc-test 的解释

`artifacts/smp4-validation-final-libctest-glibc-20260725-r2/` 三轮均自然执行
217 START/END，无内核异常。每轮结果固定为 178 Pass、39 FAIL，其中一个
`setvbuf_unget` 是 wrapper timeout。FAIL 的唯一名称集合为：

```text
clocale_mbfuncs fnmatch fscanf fwscanf mbc pthread_cancel_points sscanf
strftime strtol swprintf wcstol daemon_failure dn_expand_empty dn_expand_ptr_0
fgetwc_buffering printf_fmt_n regex_ere_backref regex_escaped_high_byte
setvbuf_unget tls_get_new_dtv
```

除 `tls_get_new_dtv` 只在动态组出现外，其余在 static/dynamic 各出现一次。这套
libc-test 以 musl 行为为判定基准，字符/locale、printf abort、regex、daemon、
TLS 和取消点在 glibc 上并不保证相同结果。因此本记录只把“脚本完整自然结束且
内核无异常”列为 glibc 内核兼容证据，不宣称这些跨-libc 语义断言通过。

### 8.5 已知边界和剩余风险

1. `user/src/bin/sig_tests.rs::kernel_sig_test_ignore` 把
   `SignalFlags::SIGSTOP.bits()`（`1 << 19`）当作 `kill(2)` signum，而 Linux ABI
   要求传 19。内核返回 EINVAL；未修改该测试，也未加入私有兼容。
2. LoongArch 交叉检查仍因宿主缺 `loongarch64-linux-gnu-gcc` 无法完成；这是
   环境缺口，不能据此声称 LoongArch 已回归。
3. stdin 和若干物理网卡路径仍采用重新轮询式 yield。pipe、poll 和本地 UDP 已使用
   统一的登记等待者—二次检查—提交/取消协议；尚未迁移的路径仍保留性能和未来扩展
   风险。
4. “没有任何问题”只能解释为本文固定的 RISC-V QEMU 四核、256 MiB、双盘矩阵
   未发现未处理内核故障，不是对未覆盖硬件、网络负载或 LoongArch 的无限保证。

## 9. 自动化使用

定向矩阵三次冷启动：

```sh
scripts/validate_smp4.py \
  --suite directed --rounds 3 --diagnostics --build \
  --test-disk /path/to/official.img \
  --output-dir artifacts/smp4-validation/directed
```

官方 musl lmbench：

```sh
scripts/validate_smp4.py \
  --suite lmbench-musl --rounds 3 \
  --test-disk /path/to/lmbench-rv-official.img \
  --output-dir artifacts/smp4-validation/lmbench-musl
```

官方 libc-test（每个 ABI 使用自己的只读盘）：

```sh
scripts/validate_smp4.py \
  --suite libctest-musl --rounds 3 \
  --test-disk artifacts/test-images/libctest-rv-musl-official-20250624.img \
  --output-dir artifacts/smp4-validation/libctest-musl

scripts/validate_smp4.py \
  --suite libctest-glibc --rounds 3 \
  --test-disk artifacts/test-images/libctest-rv-glibc-official-20250624.img \
  --output-dir artifacts/smp4-validation/libctest-glibc
```

每轮使用两个 qcow2 overlay，原始 root/test image 不写入。程序在宿主机检测长时间
无串口输出；失败时通过 QEMU `-nographic` monitor multiplexer 请求 CPU、寄存器
和内存树，不向 guest 测试注入 timeout。结果包含 `MANIFEST.md`、输入哈希、
完整 QEMU 命令、串口日志、失败 monitor dump 和 JSON 汇总。正式验收从普通
release `kernel-rv` 启动；`--diagnostics --build` 只用于 lockdep/停滞定位。

### 9.1 证据归档策略

仓库只保留最终 `*-r2`、20 轮普通定向测试、20 轮 diagnostics 定向测试、
diagnostics libc-test 和 `pthread_cond` 20 轮测试的 `MANIFEST.md` 与紧凑 JSON
摘要。早期失败定位和重复运行目录已清理。79 轮保留测试的完整串口日志、QEMU
命令、monitor dump 与 qcow2 overlay 已迁移到宿主机目录：

```text
/home/lyz/OS/oskernel2026test-validation-raw-20260726/
```

归档共 316 个文件、216,776,425 字节。按路径排序的逐文件 SHA-256 清单再做
SHA-256 后为：

```text
8eaa4a9341370373ebe6bf31d5b46d69367ed63c4273e60181b3b686f3f3dd70
```

`.gitignore` 排除后续生成的 `round-*`、串口日志和 qcow2 overlay，防止大型
运行产物再次进入提交。

## 10. 2026-07-26：适配 main 的 iperf 改动

### 10.1 上游增量审计与取舍

本轮审计的上游提交是
`c4d1dfef16cec8b590200b9a6d585781f5525e1f`（“iperf修复实现，本地干净镜像
可以通过”）。它基于单核 `main`，不能直接合并到已经重构调度、阻塞、vfork、
VFS 锁和任务生命周期的 `smp-riscv4`。实际采用“按语义移植，再用四核不变量
补强”的方式：

| 上游改动 | 当前四核分支处理 |
|---|---|
| open 后 unlink、live fd truncate | 移植语义；注册表使用 `VfsLock`，进入 lwext4 前释放元数据锁，保持 `VfsLock → FsLock` 锁序 |
| TCP listener poll、UDP peer 过滤、socket close 清理 | 移植，并补充本地无 VirtIO 网卡时的 UDP 可写语义 |
| UDP 本地队列满时丢最旧包并让出 | 移植；保留队列容量和官方负载参数 |
| 单核快照式 UDP/poll 唤醒 | 不直接移植；改用 `SchedulerLock` 内的原子 waiter 状态转换 |
| 单核 `vfork_parent` weak pointer | 不移植；当前分支已有原子 vfork owner、统一阻塞和退出发布协议 |
| 网络定时唤醒 vfork | 不移植；vfork 只由 exec/exit 的真实完成事件唤醒 |

因此这不是把 `main` 的八个文件机械覆盖到当前系统，也没有回退四核调度或覆盖
既有未提交修改。

### 10.2 实际发现的四核 iperf 故障

最初官方 TCP 三项可以通过，UDP 三项在握手后报 `Broken pipe`。诊断链最终定位
到四个相互关联的问题：

1. 本地 loopback UDP socket 在没有物理 VirtIO net 后端时被报告为不可写；
2. nonblocking server 在本地 UDP 队列暂时为空时继续进入不存在的物理后端，
   返回 `-1`，而 Linux 语义应为 `-EAGAIN`；
3. UDP/poll 唤醒通过任务快照和 `try_inner_borrow` 实现，TaskLock 短暂竞争会
   跳过 waiter，且“检查为空—登记阻塞”之间存在丢失唤醒窗口；
4. 用户时钟为每一次全局读取强制至少加一微秒/一个 tick，多 hart 和多进程共同
   读取会人为加速时间，poll 还额外注入 10 ms，破坏 iperf 的真实两秒测量。

修复后的状态转换与 pipe 使用同一协议：

```text
检查条件
  → SchedulerLock 内登记 waiter（Running → Blocking）
  → 释放调度锁后二次检查
  → 条件已满足：取消阻塞并继续
  → 条件仍不满足：切换；并发 wake 记录 WakePending
  → idle handoff 保存完上下文后发布 Ready
```

`wake_udp_waiters()` 和 `wake_poll_waiters()` 现在直接在调度器事务内识别
`BlockReason` 并完成唤醒，UDP blocking read 在登记后同时复查本地队列和物理
socket。nonblocking 本地空队列返回 `-EAGAIN`；本地队列写满时丢最旧 datagram，
不把成功发送伪装成错误。`clock_gettime/gettimeofday/times` 使用硬件时间的
非递减最大值，相同瞬间允许返回相同值，不再跨 hart 人工推进全局时间。

### 10.3 四核所有权日志说明

启动日志中 hart 0 和 hart 1 可能先后打印相同的 init `tid=1/tcb`。这是 init
任务让出后发生的合法迁移；“first dispatched”是每个 hart 的首次调度日志，不
表示两行发生在同一时刻。实际调度由同一个 `SchedulerLock` 完成
`Ready → Running`，随后
`compare_exchange(NO_RUNNING_CPU, cpu)` 声明唯一 TCB owner；如果第二个 hart
并发取得同一 TCB，内核会立即触发所有权断言。保留的 20 轮定向测试均无该断言、
队列重复、lockdep 或 panic，并且每轮都有
`smpstress passed: online=0xf, observed=0xf`。

### 10.4 官方输入与不可变性

官方脚本来自
`testsuits-for-oskernel/scripts/iperf/iperf_testcode.sh`，SHA-256 为
`81f782cdfd777cda3e0a588a1b27984e89939c2d9467beba0e0709f3dfc425ac`。
musl/glibc 静态 iperf3 均使用赛事镜像
`docker.educg.net/cg/os-contest:20250624` 的官方工具链从同一上游源码构建：

| 输入 | SHA-256 |
|---|---|
| musl iperf3 ELF | `ee6107f6012cf38e0f15a3126ff348b205f6aef0959b9e1a74e4d4bced7c34f7` |
| glibc iperf3 ELF | `746bb191be37a3576d38aa80c52b590f89c2b90fe3090cee63c8af51bdd89535` |
| 官方双 ABI iperf 测试盘 | `b57fba410d89ea6b651ceff33fee6f9c573b71cb9b40fa9244c0a3c68cc866a4` |
| 本轮普通 release `kernel-rv` | `f2a591bef08e26b9357f5915c798ca9d387fe0aeb7922e9aa893cab251d29951` |
| root fs | `ae7dfd63cf838a0e19c24ac4ff9e8ee45c99020e2df7a6d07964f93457ce5b43` |

测试脚本没有注释、删减、修改参数或插入 guest timeout。每轮原样执行
BASIC/PARALLEL/REVERSE 的 UDP/TCP 六项，`-t 2`、UDP `-b 1000G` 保持不变。

### 10.5 最终结果与回归

| 测试 | 结果 | 仓库摘要 |
|---|---:|---|
| musl 官方 iperf | 3/3 冷启动；18/18 case success；每轮 END | `artifacts/smp4-validation/iperf-musl-final-r2/` |
| glibc 官方 iperf | 3/3 冷启动；18/18 case success；每轮 END | `artifacts/smp4-validation/iperf-glibc-final-r2/` |
| diagnostics musl iperf | 1/1；6/6 case success；END | `artifacts/smp4-validation/iperf-musl-diagnostics-r2/` |
| 四核定向 | 20/20；online/observed 均为 `0xf` | `artifacts/smp4-validation/directed-after-iperf-r2/` |
| BusyBox musl/glibc 回归 | 各 1/1 END | `artifacts/smp4-validation/busybox-*-after-iperf-r2/` |
| lmbench musl/glibc 回归 | 各 1/1 END | `artifacts/smp4-validation/lmbench-*-after-iperf-r2/` |
| libc-test musl/glibc 回归 | 各 1/1 END | `artifacts/smp4-validation/libctest-*-after-iperf-r2/` |

构建侧通过 `git diff --check`、RISC-V 普通 release 和
`smp-diagnostics` release `cargo check`，以及普通 release 完整构建。新增
validator suite 会强制每轮恰好出现六个 iperf success、不得出现 case fail，并
继续要求自然组 `END`。

完整串口、QEMU 命令和 qcow2 overlay 已移到
`/home/lyz/OS/oskernel2026test-validation-raw-20260726/`。加入本轮证据后归档
共有 112 个冷启动轮次、488 个文件、311,418,095 字节；按相对路径排序的逐文件
SHA-256 清单再做 SHA-256 的结果为
`c7fce4a7126a1ca2e584b82d2463949db5ba696f1628f8826a4a0a705f41451d`。
