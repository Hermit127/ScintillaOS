# StarryOS 2026 现场赛预测盘

这套负载以 2025 现场赛官方 RISC-V Alpine Ext4 镜像为基盘，在新镜像的
`/starry26` 中加入可重复执行的预测题。基盘中的 Git、Vim、GCC、Rustc、动态链接器
和共享库保持不变；生成过程不会修改原始镜像。

盘内自动测试覆盖：

- Git 初始化、暂存、提交、分支、合并和对象校验；
- GCC 动态链接、pthread、fork/pipe/poll、共享 mmap、fsync 和 rename；
- Rustc 多线程、文件创建及原子重命名；
- 并发进程和 Ext4 目录/小文件压力；
- 可选的真实远端 `git ls-remote` 网络检查。

生成镜像：

```bash
make onsite-2026-predict-disk
```

默认输出为 `artifacts/onsite26-predict/starryos-onsite26-riscv64.img`，同目录生成
`.manifest.json`。也可以显式指定输入和输出：

```bash
python3 scripts/build_onsite26_predict_disk.py \
  --base-image ../alpine-linux-riscv64-ext4fs.img \
  --output /path/to/starryos-onsite26-riscv64.img
```

构建并以赛事双盘拓扑启动：

```bash
make run-onsite-2026-predict-rv QEMU_SMP=4
```

在 StarryOS 中把测试盘作为第一块盘挂载到 `/mnt`，然后进入 Alpine：

```sh
/busybox mount -t ext4 vda /mnt
/busybox chroot /mnt /bin/sh
/starry26/run.sh
```

启用网络题时设置一个只读公开仓库地址：

```sh
STARRY26_REMOTE_URL=https://github.com/oscomp/xv6-riscv.git /starry26/run.sh
```

Vim 仍需人工验收，因为自动伪造交互不能证明 TTY、刷新、定时器和保存路径真实可用：

```sh
vim /tmp/starry26-vim.c
```

输入一段 C 程序、保存退出，再用 `gcc` 编译运行。完整规划和现场使用顺序见
`documents/StarryOS_2026现场赛预测测试盘规划.md`。
