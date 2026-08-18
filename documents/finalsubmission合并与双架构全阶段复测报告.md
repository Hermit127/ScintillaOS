# finalsubmission 合并与双架构全阶段复测报告

本文记录 `on-site-toolchain-compat` 分支合入 `origin/finalsubmission` 后，从初赛、决赛、2025 现场赛到 2026 现场准备负载的最终源码、修复过程、复现命令、证据位置和结论边界。

文中“通过”只表示指定公开镜像、QEMU 拓扑、命令和判据真实完成；不代表实体开发板、隐藏测试、尚未公布的 2026 题目或任意第三方镜像必然通过。

## 1. 最终源码与提交链

本轮关键提交如下：

| 提交 | 作用 |
|---|---|
| `9341bbe` | 将 `origin/finalsubmission@0566fb67` 合入当前分支；无冲突，远程增量只有决赛 PPT |
| `36f58e2` | 合并决赛验证能力、双架构回归和既有兼容修复 |
| `b1d0ffc` | 添加 2026 预测盘；修正 `mkdirat` 的 Linux `EEXIST` 语义，使 `mkdir -p` 可用 |
| `fd57333` | 不再从内核直接解引用用户 futex 地址；通过页表稳定读取并发 futex 字 |
| `c54ba3d` | 保持 PTE 安全检查，同时移除 futex 热路径上的 VMA 全局读锁，恢复 BuildStorm 性能 |

最终动态测试对应源码为：

```text
c54ba3d90d1e33d83a647c24456353699d9dbf47
```

`finalsubmission` 合并没有更改内核代码，因此后续发现的 `mkdirat`、futex 安全性和性能问题属于既有功能组合被更高覆盖率测试暴露，不应错误归因给 PPT 合并。

## 2. 本轮新增问题与修复原理

### 2.1 `mkdir -p` 已存在前缀返回错误

预测盘第一次运行时，脚本的 `mkdir -p` 在已有目录前缀上失败。根因是 `sys_mkdirat` 没有先区分“目标已存在”和“父目录解析失败”，最终给出了不兼容的 `ENOENT`。

修复后：

- 根目录、`.` 或已存在目标返回 `EEXIST`；
- BusyBox `mkdir -p` 将 `EEXIST` 视为正常并继续；
- 真正不存在父目录、非法地址和文件系统错误仍返回对应 errno；
- 没有按应用名特判，也没有伪造脚本输出。

回归标记为 `MKDIR_P_OK`，已加入 `verify-onsite-2025-regressions`。

### 2.2 LoongArch 多线程 futex 随机内核缺页

`b1d0ffc` 上的一轮 LoongArch 12 核初赛矩阵在 `tls_get_new_dtv` 附近触发内核读缺页。地址反查落在 `futex_wait_current`：内核把用户虚拟地址转换成 `AtomicU32` 引用后直接读取，另一个核可同时执行 COW、`munmap` 或权限更新，使这个引用失效。

`fd57333` 的修复方式是：

1. 检查 futex 地址按 `u32` 对齐且不跨页；
2. 经任务地址空间页表查找物理页；
3. 验证 PTE 同时具有 `P`、`U` 以及读/写权限；
4. 在页表读保护范围内通过物理直映地址执行原子加载；
5. 无效或竞态失败返回 `EFAULT`，不让内核直接访问失效用户虚拟地址。

该版本消除了随机内核 panic，但最初还持有 VMA 拓扑读锁。BuildStorm 中大量线程频繁进入 futex，VMA 锁成为全局热点：LoongArch 一轮运行在 3600 秒后超时，证据保留在：

```text
artifacts/finalsubmission-merge-futexpte-final-la/20260818T141132Z-loongarch64-buildstorm/
```

该失败没有 panic，两个覆盖层仍然完整，但不能算通过。

### 2.3 安全 futex 读取的热路径优化

`c54ba3d` 移除了单字 futex 读取上的 VMA 全局锁，只保留地址空间页表读保护。理由是：

- 已对齐的 4 字节 futex 不跨页；
- 页表读保护覆盖“查 PTE—检查权限—物理原子读取”整个窗口；
- COW、撤销映射、修改权限和物理页释放必须等待页表写侧；
- PTE 的 `P/U/R/W` 已足以决定这一个字的可访问性；
- 因而无需在每次 futex 等待中再串行化整个 VMA 拓扑。

优化后同时满足正确性和性能：LoongArch 12 核初赛矩阵与连续 3 轮信号/TLS 测试通过，BuildStorm 从一小时超时恢复到 686.64 秒。

## 3. 固定镜像与最终内核

### 3.1 官方决赛镜像

| 架构 | 路径 | SHA-256 |
|---|---|---|
| RISC-V | `/mnt/h/OSkernel/contest-images/sdcard-rv-pub.img` | `d74e436522f5946ca17280a7a25f17dbb6604b71fe675bb8a021ce8e849b334c` |
| LoongArch | `/mnt/h/OSkernel/contest-images/sdcard-la-pub.img` | `d1410544e677e11efb1c240be6ffb201c89d6de58c9675e73314a696e4cefdc5` |

原始 raw 镜像未被写入；每轮测试均创建独立 qcow2 覆盖层。

### 3.2 最终内核哈希

| 文件 | SHA-256 |
|---|---|
| `kernel-rv` | `6a0271b515baccfcd4a39a8c7a488d78840d26ef5e162ee064fa022bf07e03d5` |
| `kernel-la` | `f27a21699b7ece416a74dfa72611239a0e5d692bf392700e87c9c6fffcf0c62e` |

根盘会因重建 UUID 和不同测试阶段而改变。每个证据目录中的 `metadata.json` 才是该轮根盘、内核和官方测试盘的权威哈希。

## 4. 初赛与并发稳定性结果

### 4.1 最终完整矩阵

| 架构 | 拓扑 | 结果 | 证据 |
|---|---:|---|---|
| RISC-V | 8 核、2 GiB | `online mask 0xff`；5 个正式组；musl libc-test 217/217；零 FAIL/EPERM | `artifacts/finalsubmission-merge-futexfast-rv8-pre2025/round-01/` |
| LoongArch | 12 核、2 GiB | `online mask 0xfff`；5 个正式组；musl libc-test 217/217；零 FAIL/EPERM | `artifacts/finalsubmission-merge-futexfast-la12-pre2025/20260818T151917Z/` |

两架构的根盘和测试盘覆盖层均通过 `qemu-img check`。

### 4.2 LoongArch 高风险信号/TLS/futex 重复测试

以下三轮均运行 `pthread_cancel_sem_wait`、`pthread_cancel_points` 和 `tls_get_new_dtv`，每轮 3/3 `Pass!`，在线掩码 `0xfff`，无 panic、fault 或 FAIL：

```text
artifacts/finalsubmission-merge-futexfast-la12-signal-round1/20260818T152112Z/
artifacts/finalsubmission-merge-futexfast-la12-signal-round2/20260818T152120Z/
artifacts/finalsubmission-merge-futexfast-la12-signal-round3/20260818T152128Z/
```

复现命令：

```bash
python3 scripts/validate_smp4.py \
  --kernel kernel-rv --root-disk disk.img \
  --test-disk /path/to/sdcard-rv-pre2025.img \
  --suite matrix --rounds 1 --smp 8 --memory 2G \
  --output-dir artifacts/pre2025-rv8

sh scripts/validate_loongarch12_boot_docker.sh \
  --kernel kernel-la --root-image disk-la.img \
  --test-image /path/to/sdcard-la-pre2025.img \
  --smp 12 --memory 2G --suite matrix \
  --artifact-root artifacts/pre2025-la12

sh scripts/validate_loongarch12_boot_docker.sh \
  --kernel kernel-la --root-image disk-la.img \
  --test-image /path/to/sdcard-la-pre2025.img \
  --smp 12 --memory 2G --suite signal \
  --artifact-root artifacts/signal-la12
```

## 5. 决赛正式结果

六项均使用最终 `c54ba3d` 内核、官方决赛镜像、独立冷启动和独立覆盖层。

| 架构/题目 | 结果 | 证据 |
|---|---|---|
| RISC-V 8 核 CAgent | 10/10 case；`FINAL2026_CAGENT_RC=0` | `artifacts/finalsubmission-merge-futexfast-final-rv/20260818T161353Z-riscv64-cagent/` |
| RISC-V 8 核 Q2.1 | toolchain 与 minibuild 成功；`FINAL2026_Q21_RC=0` | `artifacts/finalsubmission-merge-futexfast-final-rv/20260818T161650Z-riscv64-q21/` |
| RISC-V 8 核 BuildStorm | 1905.41 秒；8 核；1,681,000 字节；`rc=0`；评分器通过 | `artifacts/finalsubmission-merge-futexfast-final-rv/20260818T153834Z-riscv64-buildstorm/` |
| LoongArch 12 核 CAgent | 10/10 case；`FINAL2026_CAGENT_RC=0` | `artifacts/finalsubmission-merge-futexfast-final-la/20260818T162219Z-loongarch64-cagent/` |
| LoongArch 12 核 Q2.1 | toolchain 与 minibuild 成功；`FINAL2026_Q21_RC=0` | `artifacts/finalsubmission-merge-futexfast-final-la/20260818T162436Z-loongarch64-q21/` |
| LoongArch 12 核 BuildStorm | 686.64 秒；12 核；1,714,568 字节；`rc=0` | `artifacts/finalsubmission-merge-futexfast-final-la/20260818T152417Z-loongarch64-buildstorm/` |

每个目录都包含：

- `metadata.json`：架构、核心数、内存及所有输入哈希；
- `qemu-command.txt`：真实 QEMU 命令；
- `serial.log`：未剪辑串口日志；
- `root-overlay.qcow2` 与 `test-overlay.qcow2`；
- 两份 `*-overlay-check.txt`，结果均为 `No errors were found on the image.`。

复现命令：

```bash
python3 scripts/run_final_2026.py \
  --arch riscv64 --mode cagent \
  --test-image /mnt/h/OSkernel/contest-images/sdcard-rv-pub.img \
  --root-image disk.img --kernel kernel-rv \
  --smp 8 --memory 8G --timeout 1200 \
  --artifact-root artifacts/final-rv

# 把 mode 分别改为 q21 和 buildstorm；BuildStorm timeout 建议 3600。

FINAL_2026_IMAGE_DIR_HOST=/mnt/h/OSkernel/contest-images \
sh scripts/run_final_2026_docker.sh \
  --arch loongarch64 --mode cagent \
  --test-image /mnt/h/OSkernel/contest-images/sdcard-la-pub.img \
  --root-image disk-la.img --kernel kernel-la \
  --smp 12 --memory 8G --timeout 1200 \
  --artifact-root artifacts/final-la

# 同样分别运行 q21 和 buildstorm。
```

## 6. 2025 现场赛最终回归

2025 官方现场赛只提供 RISC-V Alpine 镜像，因此该阶段只声明 RISC-V 8 核动态通过。

最终源码上真实完成：

- Git、Vim、GCC、Rustc 帮助命令返回 0；
- Git `init/add/commit/log`，工作树干净；
- POSIX timer 单次、周期、绝对时间、删除、非法参数和信号投递；
- `/dev/tty`、termios、ICRNL、DEL/Ctrl-H、窗口尺寸、`ENOTTY`；
- `mkdir -p` 已存在目录前缀，标记 `MKDIR_P_OK`；
- Ext4 覆盖 rename，标记 `EXT4_RENAME_OK`；
- Vim 真实交互编辑、保存和退出；长文件截短后 `stat=wc=81`；
- GCC/Rustc 编译并运行本轮新生成的 Hello World；
- DHCP、DNS、连续临时端口 `49152,49153`、`getsockname` 和 TCP EOF；
- 2026 ABI 扩展：procfs、eventfd/epoll、timerfd、文件映射、glibc 线程与资源回收；
- 完整 HTTPS clone `oscomp/xv6-riscv`，HEAD 为 `f5dea58cc1057f2b076cdb90b446c2c21d91171e`，工作树干净；
- 每个临时覆盖层退出后 `qemu-img check` 无错误。

最终命令：

```bash
make verify-onsite-2025-regressions QEMU_SMP=8
make verify-onsite-2025-vim QEMU_SMP=8
make verify-onsite-2025-network QEMU_SMP=8
make verify-onsite-2026-abi QEMU_SMP=8

# 另一个终端启动临时代理；结束后 Ctrl-C。
python3 scripts/onsite_2025_proxy_relay.py \
  --listen-port 27898 --upstream-port 7897

make verify-onsite-2025-git-clone QEMU_SMP=8
```

这些验证器使用临时目录和临时 qcow2，结束时删除覆盖层；结果由终端中的明确通过标记和 `qemu-img check` 输出判定，不会保存 PAT。GitHub push/pull 仍需人工交互输入 fine-grained PAT 并从网页检查远端副作用，本轮没有把令牌写入命令、URL、Git 配置、文档或日志。

## 7. 2026 Python 与六工具准备负载

这两组是对可能现场题型的真实兼容准备，不是对未公布赛题的保证。

### 7.1 Python 3.13 模拟题

| 架构 | 结果 | 证据 |
|---|---|---|
| RISC-V 8 核、2 GiB | REPL、模块导入、stdlib、venv、原子文件、SQLite、procfs、subprocess、线程、多进程、asyncio、DNS、HTTPS、POST 副作用全部通过 | `artifacts/finalsubmission-merge-futexfast-python-rv/20260818T164000Z-riscv64-all/` |
| LoongArch 12 核、2 GiB | 同上，`PYTHON_ONSITE_ALL_OK`、驱动 `rc=0` | `artifacts/finalsubmission-merge-futexfast-python-la/20260818T164553Z-loongarch64-all/` |

### 7.2 Make/CMake/Ninja/Cargo/Python/curl 六工具模拟题

| 架构 | 结果 | 证据 |
|---|---|---|
| RISC-V 8 核、2 GiB | 版本、Make 并行构建、CMake 两种生成器、Ninja、Cargo build/test/metadata、venv、组合工程、curl HTTPS/POST 全部通过 | `artifacts/finalsubmission-merge-futexfast-toolchain-rv/20260818T165019Z-riscv64-all/` |
| LoongArch 12 核、2 GiB | 同上，`TOOLCHAIN_ONSITE_ALL_OK`、驱动 `rc=0` | `artifacts/finalsubmission-merge-futexfast-toolchain-la/20260818T165649Z-loongarch64-all/` |

复现时必须显式指定实际镜像路径。Makefile 默认的 `testsuits-for-oskernel-final/image/...` 在当前机器不存在；遗漏变量会在客体启动前报 `missing required file`，这不是内核测试失败。

```bash
python3 scripts/onsite_2025_proxy_relay.py \
  --listen-port 27898 --upstream-port 7897

make verify-onsite-python-rv \
  PYTHON_ONSITE_MODE=all PYTHON_ONSITE_SMP=8 \
  FINAL_2026_RV_IMAGE=/mnt/h/OSkernel/contest-images/sdcard-rv-pub.img

FINAL_2026_IMAGE_DIR_HOST=/mnt/h/OSkernel/contest-images \
make verify-onsite-python-la-docker \
  PYTHON_ONSITE_MODE=all \
  FINAL_2026_LA_IMAGE=/mnt/h/OSkernel/contest-images/sdcard-la-pub.img

make verify-onsite-toolchain-rv \
  TOOLCHAIN_ONSITE_MODE=all TOOLCHAIN_ONSITE_SMP=8 \
  FINAL_2026_RV_IMAGE=/mnt/h/OSkernel/contest-images/sdcard-rv-pub.img

FINAL_2026_IMAGE_DIR_HOST=/mnt/h/OSkernel/contest-images \
make verify-onsite-toolchain-la-docker \
  TOOLCHAIN_ONSITE_MODE=all \
  FINAL_2026_LA_IMAGE=/mnt/h/OSkernel/contest-images/sdcard-la-pub.img
```

## 8. 2026 预测盘与 StarryOS 边界

预测盘由 `scripts/build_onsite26_predict_disk.py` 从官方 RISC-V 镜像生成，加入 6 类预测用例。最终干净运行位于：

```text
artifacts/onsite26-predict-runs/20260818T134914Z/
```

该轮在线掩码为 `0xff`，`MKDIR_P_RC=0`，6 类已配置用例通过，覆盖层检查正常。因为当轮没有设置外部 URL，预测盘网络项按设计跳过；网络能力由第 6、7 节的独立 DNS/HTTPS/Git/Python/curl 测试覆盖。

预测盘中的“StarryOS”用例是兼容性探针和准备流程，不等同于“本内核已经启动 StarryOS”。仓库目前没有把 StarryOS 作为客体/用户态负载完整启动的官方镜像和评分脚本，因此不得声称 StarryOS 已通过。

## 9. 构建与最终静态检查

最终源码已完成：

```bash
git diff --check
cargo check -p os --target riscv64gc-unknown-none-elf
make kernel-rv

docker run --rm -e HOME=/root \
  -v "$PWD:/work" -w /work \
  zhouzhouyi/os-contest:20260510 make kernel-la
```

构建会重写受跟踪的 `lwext4_rust/c/lwext4/liblwext4-riscv64.a`。收尾时已用与 HEAD blob 哈希完全一致的备份恢复；该生成差异没有进入提交。

## 10. 结论与不能声称的内容

可以由本轮证据支持：

- `finalsubmission` 已以普通 merge commit 合入，无代码冲突和历史改写；
- RISC-V 8 核与 LoongArch 12 核初赛公开矩阵均在最终源码上通过；
- 两架构决赛 CAgent、Q2.1、BuildStorm 均在最终源码、官方镜像和赛事核心数下逐题冷启动通过；
- RISC-V 2025 现场赛 Git/Vim/GCC/Rustc、网络和完整 clone 在最终源码上通过；
- 2026 Python 与六工具模拟负载在两架构最终源码上通过；
- futex 修复同时通过并发正确性和 BuildStorm 性能验证。

不能据此声称：

- 2025 现场赛 LoongArch 通过，因为官方只提供 RISC-V Alpine 现场镜像；
- 实体开发板通过，因为本轮只运行 QEMU；
- 未来隐藏测试和未公布 2026 赛题必然通过；
- StarryOS 已经在本内核上完整启动；
- 任意低内存跨题串联都通过。历史 `mode=all --memory 2G` 的 LoongArch 决赛串联曾在 CAgent 后进入 BuildStorm 时耗尽内核堆，正式赛事证据采用每题独立冷启动和 8 GiB，不能用分题结果掩盖这个非正式压力边界。

最终工程结论是：在本文列出的公开赛事镜像、QEMU 拓扑、命令和判据下，从初赛到决赛、2025 现场赛及当前 2026 准备负载均无可见回退；结论严格止于这些已执行输入。
