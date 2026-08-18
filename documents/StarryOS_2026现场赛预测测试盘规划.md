# StarryOS 2026 现场赛预测测试盘规划

## 1. 定位

2025 现场赛的本质不是在内核中实现 Git、Vim、GCC 和 Rustc，而是让官方 Ext4
用户态中的原版 Linux 软件通过参赛内核真实运行。2026 预测盘继续沿用这个边界：
测试盘提供程序、动态库、输入和驱动脚本；StarryOS 提供 Linux ABI、进程、内存、
Ext4、TTY、时间、信号和网络语义。

启动拓扑保持与赛事一致：

```text
vda / bus0：starryos-onsite26-riscv64.img（测试盘）
vdb / bus1：disk.img（StarryOS 根盘，含 /busybox 和 shell）
网卡：virtio-net
CPU：先 4 核排错，再 8 核压力验证
内存：先 1 GiB，工具链长测提高到 2–8 GiB
```

## 2. 从 2025 规划继承的原则

2025 的四题分别深入文件系统、交互终端、编译器进程链和网络，而不只检查版本号。
预测盘因此坚持以下判定：

1. `--version` 只算加载检查，不算功能通过；
2. 编译器必须完成编译、动态链接并运行产物；
3. Git 必须产生并校验真实对象库；
4. 文件系统必须覆盖跨页写回、fsync、rename、目录增删和并发访问；
5. 网络题必须出现真实远端副作用或可独立校验的响应；
6. Vim 必须人工操作，串口脚本不能替代 TTY 交互验收；
7. 每轮从干净镜像或 qcow2 覆盖层开始，保留内核、镜像和日志哈希。

## 3. 首版题目矩阵

| 阶段 | 负载 | 主要内核路径 | 通过标记 |
| --- | --- | --- | --- |
| 环境 | uname 与四工具版本 | ELF、动态加载、proc/utsname | `versions` case pass |
| Git | init/add/commit/branch/merge/fsck | fork/exec/wait、目录项、锁文件、rename | `git_workflow` |
| C ABI | pthread、fork、pipe、poll、mmap、fsync | futex、COW、fd、poll、共享映射、写回 | `STARRY26_C_ABI_OK` |
| Rust | 8 线程、文件同步、原子 rename | glibc TLS、clone/futex、Ext4 | `STARRY26_RUST_OK` |
| FS 压力 | 8 进程并发创建/rename/删除 512 文件 | 调度、Dentry、Ext4 锁与缓存回收 | `fs_concurrency` |
| 网络 | `git ls-remote` HTTPS | DNS、TCP、TLS、poll、时间 | `network_remote` |
| 人工 | Vim 编辑保存后 GCC 编译 | TTY、信号、timer、文件覆盖 | 人工记录 |

首版主要用于快速发现 ABI 和组合语义回退，不能替代数小时的大型构建、实体板卡和未知
官方镜像。下一层压力盘应加入 Python/SQLite、Cargo 多 crate 构建、本地并发服务、
大目录依赖扫描、内存压力和故障注入；仓库现有 `onsite_python`、`onsite_toolchain`、
CAgent 与 BuildStorm 运行器可以作为这层验证。

## 4. 制作与校验

```bash
make onsite-2026-predict-disk
sha256sum artifacts/onsite26-predict/starryos-onsite26-riscv64.img
e2fsck -fn artifacts/onsite26-predict/starryos-onsite26-riscv64.img
debugfs -R 'ls -l /starry26' artifacts/onsite26-predict/starryos-onsite26-riscv64.img
```

构建器先复制 2025 官方 Alpine raw 镜像，再向副本注入负载，最后执行修复检查和只读检查。
基盘不会被写入。输出镜像被 `.gitignore` 排除；同目录 JSON 保存基盘、输出和 payload
SHA-256，便于确认每次运行使用的确切输入。

## 5. 建议现场演练顺序

1. `make kernel-rv disk.img onsite-2026-predict-disk`；
2. 以 4 核、1–2 GiB 双盘启动，运行 `/starry26/run.sh`；
3. 单独完成 Vim 人工题；
4. 配好网络后用 `STARRY26_REMOTE_URL` 重跑；
5. 以 8 核连续跑 10 轮，检查随机死锁、资源泄漏和盘损坏；
6. 再运行仓库已有 Python、六工具和 BuildStorm 长测；
7. 每次只对 qcow2 覆盖层写入，保留串口日志和哈希，不把大镜像提交 Git。

LoongArch 版本不能直接复用 RISC-V Alpine 用户态。必须拿 LoongArch64 基盘，用同一构建器
注入架构无关脚本和源码，再在对应 QEMU/开发板上编译执行；在获得并核验该基盘之前，
不能声称 LoongArch 动态通过。
