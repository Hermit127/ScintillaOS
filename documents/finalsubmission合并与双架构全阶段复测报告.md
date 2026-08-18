# finalsubmission 合并与双架构全阶段复测报告

本文记录 `on-site-toolchain-compat` 合入决赛提交分支后的源码状态、增量修复、RISC-V 8 核与 LoongArch 12 核复测方法、证据位置和结论边界。目标是验证从初赛基础矩阵、决赛 CAgent/BuildStorm、2025 现场赛，到 2026 Python/六工具准备负载之间没有可见回退；“通过”只表示本文列出的输入、拓扑、命令和检查条件真实完成，不代表未来任意镜像或未公开题目必然通过。

## 1. 合并结果

用户所说的 `final-submission` 在当前 GitLab 远程中的实际名称是 `origin/finalsubmission`。合并前重新检查到：

- 当前分支：`on-site-toolchain-compat`；
- 当前侧提交：`a66a0b5`（六工具现场开发与验收 SOP）；
- 远程决赛分支：`origin/finalsubmission@0566fb67d4b2e94e6aea9e1895ca6b8751dd5f26`；
- 合并提交：`9341bbe0b03f9a1e10ee8aaccb822163689a5d9f`；
- 合并方式：保留双亲的普通 merge commit，没有 rebase、squash 或历史改写；
- 冲突：无。

这次远程增量只有 `ScintillaOS演示ppt_决赛.pptx` 一个 2,385,473 字节的演示文件，没有内核、用户程序、构建脚本或启动入口变化。因此“合并本身没有破坏代码”可以由树差异直接证明；后续运行暴露的问题来自既有能力组合及验证覆盖不足，不能错误归因给 PPT。

## 2. 合并后新增修复与验证能力

### 2.1 Linux `wait4/waitpid` 的 `ECHILD` 语义

初赛官方 BusyBox 矩阵最初在每次成功回收子进程后打印：

```text
waitpid: Operation not permitted
```

根因是 `sys_waitpid` 在没有匹配子进程时返回历史 rCore 值 `-1`。Linux 系统调用 ABI 中负数表示 `-errno`，所以 glibc/musl 将它解释为 `EPERM`；正确结果应是 `-ECHILD`，即 `-10`。修复位于 `os/src/syscall/process.rs`，并在 `clonesmptest` 中加入“空子进程集合必须返回 `-10`”的定向断言。RISC-V 8 核和 LoongArch 12 核定向回归、两架构初赛矩阵均验证该断言和官方脚本不再出现 waitpid EPERM。

### 2.2 十二核压力程序不再截断 CPU 掩码

`smpstress` 原来只读取 `sched_getaffinity` 返回数组的第一个字节，因此 LoongArch 的 `0xfff` 被误报为 `0xff`；同时仅创建 8 个工作线程，不能稳定覆盖 12 个调度队列。现在它折叠整个 affinity 字节数组并创建 12 个工作线程。通过条件分别为：

```text
RISC-V:    online=0xff,  observed=0xff
LoongArch: online=0xfff, observed=0xfff
```

### 2.3 LoongArch 12 核完整验证器

`scripts/validate_loongarch12_boot.py` 从单一启动探针扩展为：

- `--suite boot`：在线掩码和 `cpuinfotest`；
- `--suite directed`：SMP、clone/wait、futex、mprotect、共享内存、管道和信号；
- `--suite matrix`：双 VirtIO 块设备运行 2025 初赛官方计分矩阵。

矩阵验证不会只等待结束字符串：它要求 5 个正式计分组全部结束、musl libc-test 精确出现 217 个 `Pass!`、零个 `FAIL`，并拒绝 waitpid EPERM。两个 qcow2 覆盖层都必须通过 `qemu-img check`。`validate_smp4.py` 使用同样的判定规则。

### 2.4 Docker 外部镜像只读挂载

官方 15 GB 镜像位于 `/mnt/h/OSkernel/contest-images/`，不在仓库目录。四个 Docker 包装器现在可通过 `FINAL_2026_IMAGE_DIR_HOST` 把镜像目录以只读方式挂到容器中的相同绝对路径；LoongArch 初赛验证器会从 `--test-image` 自动解析并只读挂载目录。镜像不复制进 Git，不写入原始 raw 文件，所有客体修改进入独立 qcow2 覆盖层。

### 2.5 Ext4 创建时间戳与 BuildStorm 性能

之前为了使新文件立即具有正确的 atime、mtime 和 ctime，VFS 在创建文件后又分别调用三次全路径 Ext4 时间戳更新。语义正确，但每个新文件会触发四次独立文件系统事务。BuildStorm 会生成大量中间文件，LoongArch 12 核官方 8 GiB 镜像因此长时间停留在 `ax-api` 编译阶段。

本轮将创建时间直接传入 `ext4_create_at_with_attrs`，在原有创建事务中一次设置三个时间戳，删除创建后的三次重复路径查找和事务。这是通用 Ext4/VFS 修复，没有识别 BuildStorm 进程名或绕过持久化语义。修复后 LoongArch 12 核全量 BuildStorm 用时 1105.23 秒，产物 1,714,568 字节，两个覆盖层检查正常。

### 2.6 多线程信号帧的安全复制与 VMA 竞态

合并后的 LoongArch 12 核完整初赛矩阵曾在 musl 的 `pthread_cancel_sem_wait`、`pthread_cancel_points` 和 `tls_get_new_dtv` 附近偶发失败。第一次故障是内核态直接解引用用户栈地址，在 `handle_signals` 写 `rt_sigframe` 时触发内核页故障；改为经页表物理直映复制后，内核不再 panic，但完整矩阵仍捕获到一次：

```text
[signal-frame] write failed tid=995 sig=34 sp=0x10000e2b60
FAIL tls_get_new_dtv [signal Segmentation fault]
```

根因是 `CLONE_VM` 线程共享地址空间时，信号栈预缺页与真正复制之间存在窗口：另一个核可在该窗口执行 COW、`madvise` 或 VMA/PTE 更新。最终修复包含两层：

1. `write_signal_frame` 先在内核 `Vec<u8>` 中完整构造 1088 字节信号帧，`rt_sigreturn` 也先安全快照用户帧，不再从内核直接解引用用户虚拟地址；
2. `MemorySet::copy_to_user_stable` 在整个复制期间持有 VMA 读锁并逐页通过页表物理地址写入；若 COW 或 `madvise` 只造成短暂 PTE 变化，则重新以写访问缺页并有限重试。真正未映射或不可写的信号栈仍失败，不伪造成功。

验证器新增 `--suite signal`，定向运行上述三个 libc 用例并要求 3 个 `Pass!`、零 `FAIL`；`make verify-loongarch12-signal-docker TEST_DISK=...` 提供固定入口。稳定写入完成后，定向套件和随后完整 12 核矩阵都通过；代码复审后再加入的对称稳定读取只在最终 RISC-V 矩阵中动态覆盖，LoongArch 仅完成 release 编译。准确的代码时间边界和证据见第 4、9 节。

## 3. 固定输入与哈希

### 3.1 官方决赛镜像

| 架构 | 路径 | 字节数 | SHA-256 |
|---|---|---:|---|
| RISC-V | `/mnt/h/OSkernel/contest-images/sdcard-rv-pub.img` | 15,032,385,536 | `d74e436522f5946ca17280a7a25f17dbb6604b71fe675bb8a021ce8e849b334c` |
| LoongArch | `/mnt/h/OSkernel/contest-images/sdcard-la-pub.img` | 15,032,385,536 | `d1410544e677e11efb1c240be6ffb201c89d6de58c9675e73314a696e4cefdc5` |

### 3.2 2025 初赛矩阵盘

测试程序来自 `../testsuits-for-oskernel@origin/pre-2025`，在官方 `docker.educg.net/cg/os-contest:20250624` 环境中构建。镜像使用 Ext4，关闭当年 lwext4 不支持的 `orphan_file` 与 `metadata_csum_seed`；补齐官方测试使用的动态解释器链接及 DSO 布局。正式计分组为 BusyBox musl/glibc、lmbench musl/glibc、libc-test musl，共 5 组；glibc libc-test 是额外诊断，不属于该正式计分矩阵。

| 架构 | 临时镜像 | SHA-256 |
|---|---|---|
| RISC-V | `/tmp/oscomp-pre2025-clean.UmORrq/sdcard-rv-pre2025.img` | `e0c5fa71726deb33096fd39577b8ac8f3984bcca6da5fa8835929c26cdbe6957` |
| LoongArch | `/tmp/oscomp-pre2025-clean.UmORrq/sdcard-la-pre2025.img` | `5849055ae6c54c91cf4b0b3395dba7c7de971ee83d4f53581cdeefdb988932e6` |

两盘均通过只读 `e2fsck`。它们位于 `/tmp`，不是长期归档；需要复现时应从相同测试仓库提交和官方容器重新生成，而不是依赖临时路径永久存在。

### 3.3 本轮内核和根盘

| 文件 | SHA-256 |
|---|---|
| `kernel-rv` | `14f1781540372f6a4ad19bed4da98738d7853222aa664f112a2dcbbf5bc5f50f` |
| `kernel-la` | `01ecc436df1d253f5d2d13e54b5f9d5079b0fcd82d1c087b68be3db197cce198` |
| `disk.img` | `af7710812a0ae932646f3d4da1136e45021c87fd0e8ea3708b288e3d36b96b36` |
| `disk-la.img` | `5d657519634335c98c246142215335c25b70cb0425775287e52bfe719d826959` |

`disk.img` 的 Ext4 UUID 在每次 `make fs-img` 时重新生成，因此表中是最后一次正式 BuildStorm 后的工作区哈希；每个证据目录中的 `metadata.json` 才是对应测试轮次的权威输入哈希。

## 4. 已完成的初赛与内核定向回归

| 架构/套件 | 证据目录 | 结果 |
|---|---|---|
| RISC-V 8 核 directed（最终源码） | `artifacts/finalsubmission-merge-rv8-directed-signalstable-final/round-01/` | `online/observed=0xff`；全部 10 个定向命令通过；ECHILD 断言通过 |
| LoongArch 12 核 directed（信号加固前） | `artifacts/finalsubmission-merge-la12-directed-atomic-time/20260818T081325Z/` | `online/observed=0xfff`；全部 10 个定向命令通过；随后信号复制代码有变化，不能冒充最终哈希 |
| LoongArch 12 核信号/TLS 定向（最终源码） | `artifacts/finalsubmission-merge-signal-diag-la12/20260818T094259Z/` | 三个高风险 libc 用例 3/3 `Pass!`，零 `FAIL` |
| RISC-V 8 核初赛计分矩阵（最终源码） | `artifacts/finalsubmission-merge-pre2025-signalstable-final-rv8/round-01/` | `online mask 0xff`；5/5 组；217/217；零 FAIL/EPERM；双覆盖层完整 |
| LoongArch 12 核初赛计分矩阵（稳定写入版） | `artifacts/finalsubmission-merge-pre2025-signalstable-la12/20260818T095059Z/` | `online mask 0xfff`；5/5 组；217/217；零 FAIL/EPERM；双覆盖层完整；晚于此轮又加入对称的稳定读取 |

信号竞态稳定写入修复后的 LoongArch 完整矩阵成功运行 1 轮。之后代码复审又把 `rt_sigreturn` 改为在 VMA 读锁下稳定快照；该对称通用路径在最终 RISC-V 8 核 217 项矩阵中通过，LoongArch 最终二进制编译成功，但受宿主权限限制没有再次动态运行。尝试再做 LoongArch 重复时，宿主执行环境拒绝新的 Docker/QEMU 提权请求，因此本文不会写成“最终二进制连续多轮通过”。失败前的两个矩阵目录仍保留，用于证明验证器确实能捕获内核 panic 和单个 libc `FAIL`，而不是只匹配结束字符串：

- `artifacts/finalsubmission-merge-pre2025-atomic-la12/20260818T092658Z/`：旧实现内核页故障；
- `artifacts/finalsubmission-merge-pre2025-signalfix2-la12/20260818T094517Z/`：捕获 `tls_get_new_dtv` 的信号帧竞态。

复现命令：

```bash
python3 scripts/validate_smp4.py \
  --kernel kernel-rv --root-disk disk.img \
  --test-disk /tmp/oscomp-pre2025-clean.UmORrq/sdcard-rv-pre2025.img \
  --suite matrix --rounds 1 --smp 8 --memory 2G \
  --output-dir artifacts/pre2025-rv8-$(date -u +%Y%m%dT%H%M%SZ)

make verify-loongarch12-pre2025-docker \
  TEST_DISK=/tmp/oscomp-pre2025-clean.UmORrq/sdcard-la-pre2025.img

make verify-loongarch12-signal-docker \
  TEST_DISK=/tmp/oscomp-pre2025-clean.UmORrq/sdcard-la-pre2025.img
```

## 5. 决赛正式复测方法

正式证据把 CAgent、Q2.1 和 BuildStorm 分三次冷启动，每次创建新的测试盘和根盘覆盖层。`mode=all` 是额外的跨题资源回收压力测试，不能代替分题证据。官方准备指南和运行器的默认内存为 8 GiB。

```bash
export FINAL_2026_IMAGE_DIR_HOST=/mnt/h/OSkernel/contest-images

make run-final-2026-rv FINAL_2026_MODE=cagent FINAL_2026_MEMORY=8G
make run-final-2026-rv FINAL_2026_MODE=q21 FINAL_2026_MEMORY=8G
make run-final-2026-rv FINAL_2026_MODE=buildstorm FINAL_2026_MEMORY=8G

make run-final-2026-la-docker FINAL_2026_MODE=cagent FINAL_2026_MEMORY=8G
make run-final-2026-la-docker FINAL_2026_MODE=q21 FINAL_2026_MEMORY=8G
make run-final-2026-la-docker FINAL_2026_MODE=buildstorm FINAL_2026_MEMORY=8G
```

每轮必须同时具备：正确在线掩码、官方脚本成功标记、驱动 `rc=0`、无 panic/fault/超时、两个覆盖层检查通过。BuildStorm 还必须生成至少 500,000 字节的真实构建产物；CAgent 必须 10/10 case pass。

本轮最终结果在测试结束后记录于第 9 节。

## 6. 2025 现场赛复测方法

2025 官方现场赛只提供 RISC-V Alpine 镜像，所以这一部分只能声称 RISC-V 8 核动态结果，不能虚构 LoongArch 对等镜像。基础、Vim、网络、ABI 和完整 HTTPS clone 应分别从全新覆盖层运行：

```bash
make verify-onsite-2025-regressions QEMU_SMP=8
make verify-onsite-2025-vim QEMU_SMP=8
make verify-onsite-2025-network QEMU_SMP=8
make verify-onsite-2026-abi QEMU_SMP=8

# 另一个终端启动只监听本机/QEMU 网关所需端口的代理中继
make onsite-2025-proxy-relay
make verify-onsite-2025-git-clone QEMU_SMP=8
```

检查点不是版本输出，而是 POSIX timer、TTY/termios、Ext4 覆盖 rename 与截短长度、Vim 交互保存、GCC/Rustc 新编译产物、DHCP/DNS/TCP/EOF/临时端口和完整 Git 对象写盘。GitHub push/pull 需要用户交互输入 PAT 和网页确认远端副作用，自动化不会保存令牌，也不能替代该人工步骤。

## 7. 2026 现场准备复测方法

当前已实现的准备内容包括 Python 3.13 模拟题和 Make/CMake/Ninja/Cargo/Python/curl 六工具模拟题。两者使用官方 2026 双架构镜像，但不是尚未公布的正式现场题目。

```bash
export FINAL_2026_IMAGE_DIR_HOST=/mnt/h/OSkernel/contest-images

make verify-onsite-python-rv PYTHON_ONSITE_MODE=all QEMU_SMP=8
make verify-onsite-python-la-docker PYTHON_ONSITE_MODE=all

make verify-onsite-toolchain-rv TOOLCHAIN_ONSITE_MODE=all QEMU_SMP=8
make verify-onsite-toolchain-la-docker TOOLCHAIN_ONSITE_MODE=all
```

Python 验证版本、文件系统、SQLite、线程/进程/信号、asyncio、DNS/HTTPS；六工具验证真实工程生成、并行构建、链接/运行、Cargo、虚拟环境、证书和 curl 网络副作用。

## 8. 低内存串联压力测试的失败边界

一次额外的 `mode=all --memory 2G` LoongArch 测试先成功完成 CAgent，再立即进入 BuildStorm，在编译早期触发内核堆耗尽：

```text
[heap-debug] user=92422451 actual=130576232 total=130576384
[task-debug] ... net=9 waitpid=5 ...
Heap allocation error, layout size 0xc0 align 8
```

证据位于 `artifacts/finalsubmission-merge-final2026-echild-la/20260818T064806Z-loongarch64-all/`。官方 CAgent 脚本终止后台服务器后没有显式 `wait`，低内存下跨题残留资源与后续大编译共同耗尽约 124.5 MiB 内核堆。该结果不能被删去或写成通过；它也不能推翻每题从新覆盖层、8 GiB 客体运行的正式证据。RISC-V 的同类非正式 2 GiB 串联测试在 CAgent 通过、BuildStorm 开始后被主动停止，覆盖层检查结果被保留但不计为成功或失败。

如果未来要求在同一 2 GiB 启动中连续运行所有决赛题，应把它作为独立资源回收/缓存上界需求继续修复，而不能引用本文的分题通过结果冒充。

## 9. 本轮最终结果

### 9.1 当前最终源码的双架构核心证据

| 阶段 | 架构/拓扑 | 结果 | 证据 |
|---|---|---|---|
| 初赛定向 | RISC-V 8 核 | 10 个命令全部通过，`online/observed=0xff` | `artifacts/finalsubmission-merge-rv8-directed-signalstable-final/round-01/` |
| 初赛矩阵 | RISC-V 8 核 | 5 组、217/217、零 FAIL/EPERM | `artifacts/finalsubmission-merge-pre2025-signalstable-final-rv8/round-01/` |
| 初赛矩阵 | LoongArch 12 核 | 稳定写入版 5 组、217/217、零 FAIL/EPERM，`online mask 0xfff`；最终稳定读取增量仅编译 | `artifacts/finalsubmission-merge-pre2025-signalstable-la12/20260818T095059Z/` |
| 信号/TLS 定向 | LoongArch 12 核 | 3/3 `Pass!` | `artifacts/finalsubmission-merge-signal-diag-la12/20260818T094259Z/` |
| 决赛 CAgent | RISC-V 8 核、8 GiB | 10/10 case，`FINAL2026_CAGENT_RC=0` | `artifacts/finalsubmission-merge-final2026-current-rv8/20260818T102704Z-riscv64-cagent/` |
| 决赛 Q2.1 | RISC-V 8 核、8 GiB | toolchain/minibuild 均成功，`FINAL2026_Q21_RC=0` | `artifacts/finalsubmission-merge-final2026-current-rv8/20260818T102923Z-riscv64-q21/` |
| 决赛 BuildStorm | RISC-V 8 核、8 GiB | 2014.12 秒、8 核、1,681,000 字节、`rc=0`、评分器通过 | `artifacts/finalsubmission-merge-final2026-current-rv8/20260818T103150Z-riscv64-buildstorm/` |

以上 RISC-V 三项决赛证据在稳定写入信号帧与最终 Ext4 代码上重新生成；随后增加的对称稳定读取由最终 RISC-V 217 项矩阵覆盖。每个决赛目录均包含输入哈希、QEMU 命令、未剪辑串口日志、两个 qcow2 覆盖层及 `qemu-img check` 结果。

### 9.2 LoongArch 决赛证据及代码时间边界

LoongArch 官方决赛三项在 Ext4 原子创建时间戳修复完成后、最终信号帧 VMA 加固前分别冷启动通过：

| 题目 | 结果 | 证据 |
|---|---|---|
| CAgent | 10/10，`FINAL2026_CAGENT_RC=0` | `artifacts/finalsubmission-merge-final2026-atomic-la/20260818T083526Z-loongarch64-cagent/` |
| Q2.1 | toolchain/minibuild 成功，`FINAL2026_Q21_RC=0` | `artifacts/finalsubmission-merge-final2026-atomic-la/20260818T083747Z-loongarch64-q21/` |
| BuildStorm | 1105.23 秒、12 核、1,714,568 字节、`rc=0` | `artifacts/finalsubmission-merge-final2026-atomic-la/20260818T081348Z-loongarch64-buildstorm/` |

稳定写入信号帧只改变信号用户内存复制路径；它随后由 LoongArch 12 核三个信号/TLS 定向用例和完整初赛矩阵覆盖。最后加入的对称稳定读取在 RISC-V 最终 217 项矩阵中通过，LoongArch 最终 release 编译成功，但未获得新的 LoongArch 动态轮次。由于收尾阶段宿主策略拒绝新的 Docker/QEMU 提权，三项 LoongArch 决赛也没有用最终二进制再跑一遍。因此可支持的工程结论是“决赛工作负载在最终 Ext4/调度实现上通过，信号写入加固由 12 核矩阵覆盖，信号读取加固由 8 核矩阵和 LoongArch 编译覆盖”，不能把旧 `metadata.json` 的内核哈希写成最终哈希。

### 9.3 2025 现场赛与 2026 准备负载

当前最终 RISC-V 8 核重新执行并通过：

- `verify-onsite-2025-regressions`：POSIX timer、TTY/ICRNL/退格、Ext4 覆盖 rename；
- `verify-onsite-2025-vim`：真实 Vim 交互写入，`stat=wc=81`，GCC/Rustc 新编译产物运行成功；
- `verify-onsite-2026-abi`：procfs、eventfd/epoll、timerfd、文件映射、glibc 线程和资源回收全部通过；
- Python `core`、`fs`、`concurrency`：证据分别位于 `artifacts/finalsubmission-merge-python-current-rv8/20260818T100325Z-riscv64-core/`、`20260818T100746Z-riscv64-fs/`、`20260818T101038Z-riscv64-concurrency/`；
- 六工具 `versions`、`build`、`cargo`、`python`：证据位于 `artifacts/finalsubmission-merge-toolchain-current-rv8/` 下对应时间戳目录。

本轮 `verify-onsite-2025-network`、Python `all` 和 curl 网络组均在客体启动前因宿主沙箱禁止创建临时监听 socket 而终止；这不是客体测试失败，但也不构成本轮网络通过证据。网络能力使用合并工作树较早的完整通过记录：

- Python RISC-V/LoongArch `all`：`artifacts/finalsubmission-merge-python-rv/20260818T045639Z-riscv64-all/`、`artifacts/finalsubmission-merge-python-la/20260818T050109Z-loongarch64-all/`；
- 六工具 RISC-V/LoongArch `all`：`artifacts/finalsubmission-merge-toolchain-rv/20260818T054344Z-riscv64-all/`、`artifacts/finalsubmission-merge-toolchain-la/20260818T054902Z-loongarch64-all/`。

2025 Git 完整 clone/push/pull 已在此前人工验收中真实完成；push/pull 依赖用户 PAT 和网页远端副作用，本轮不重复保存认证交互。最终信号/Ext4 增量不修改 VirtIO、DHCP、DNS、TCP 或 Git 路径，但严格来说仍不能把较早网络日志冒充最终哈希的重新执行结果。

## 10. 结论边界

- `finalsubmission` 合并已完成且无代码冲突，远程增量只有决赛 PPT。
- RISC-V 8 核定向回归和初赛正式矩阵已在最终源码上通过；LoongArch 12 核稳定写入版矩阵通过 1 轮，最后的对称稳定读取仅完成 LoongArch 编译，不写成最终二进制多轮证明。
- RISC-V 8 核决赛三项已在最终 Ext4 与稳定信号写入代码、官方镜像和 8 GiB 条件下逐项冷启动通过；LoongArch 12 核三项在最终 Ext4 实现上通过，后续信号路径增量按上一条边界验证，但未再次重跑三项决赛。
- 2025 现场赛官方动态目标只有 RISC-V；不得声称 LoongArch 通过不存在的官方 Alpine 现场盘。
- Python 和六工具是针对可能题型的真实兼容性准备，不是对 2026 未公布题目的预测性保证。
- 仓库没有一套可执行的“在本内核上启动 StarryOS”正式测试，因此本文不声称 StarryOS 通过。
- 当前证据支持“在列出的公开赛事镜像、QEMU 拓扑和命令下可行”；QEMU、官方镜像和公开脚本覆盖不到实体开发板、隐藏测试、未来镜像变化与故障注入，因此不能给出超出这些输入的绝对保证。
