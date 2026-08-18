# 决赛公版 SD 卡镜像测试与 initproc 操作手册

本文适用于本仓库的本地冻结分支 `finalsubmission`。它说明如何用官方
`sdcard-rv-pub.img`、`sdcard-la-pub.img` 跑 2026 决赛测例，以及现场版
`initproc` 应保持什么行为。

## 结论与磁盘分工

不要把官方决赛测试盘做进内核，也不要把 CAgent、BuildStorm 写死在
`initproc` 中。正确的 QEMU 双盘布局是：

| QEMU 块设备 | 内容 | 用途 |
| --- | --- | --- |
| `vda` | 官方 `sdcard-*-pub.img` 的 QCOW2 临时覆盖层 | `/glibc/cagent_testcode.sh`、`/glibc/buildstorm_testcode.sh` 及官方运行环境 |
| `vdb` | 本仓库重新构建的 `disk.img` / `disk-la.img` | StarryOS 的根文件系统和 `/bin/user_shell` |

`scripts/run_final_2026.py` 已按此布局启动 QEMU：测试盘总是第一个
virtio 块设备，根盘总是第二个。它为两个原始镜像各创建 QCOW2 overlay，
所以测试产生的文件、ext4 日志回放和异常关机都不会污染官方原始镜像。

## 0. 先确认拿到的是官方公版镜像

默认路径是：

```text
testsuits-for-oskernel-final/image/sdcard-rv-pub.img
testsuits-for-oskernel-final/image/sdcard-la-pub.img
```

在仓库根目录检查：

```bash
test -f testsuits-for-oskernel-final/image/sdcard-rv-pub.img
test -f testsuits-for-oskernel-final/image/sdcard-la-pub.img
```

仅有 `sdcard-rv.img.xz`、`sdcard-la.img.xz` 或同名但不带 `-pub` 的文件时，
不能仅凭文件名认定它是公版决赛测试盘。先向赛方/队友确认来源、版本和
SHA-256；若确认它就是公版压缩包，校验并保留压缩包地解压：

```bash
xz -t sdcard-rv-pub.img.xz
xz -dk sdcard-rv-pub.img.xz
```

`xz -t` 成功时没有输出是正常现象；`xz -dk` 解压期间会占用终端，使用
`-k` 会保留 `.xz` 原文件。

可用以下命令确认镜像包含决赛脚本（命令成功后才继续）：

```bash
debugfs -R 'ls -l /' testsuits-for-oskernel-final/image/sdcard-rv-pub.img | grep glibc
debugfs -R 'ls -l /glibc' testsuits-for-oskernel-final/image/sdcard-rv-pub.img \
  | grep -E 'cagent_testcode.sh|buildstorm_testcode.sh'
```

## 1. `initproc` 的现场正确写法

冻结分支上的 `user/src/bin/initproc.rs` 有且只有两个职责：启动
`user_shell`，并持续回收退出的子进程。它**不**自动挂载 `vda`、不
`chroot`、不自动执行 CAgent/BuildStorm，也不在测试尚未结束时关机。

```rust
#[no_mangle]
fn main() -> i32 {
    if fork() == 0 {
        let argv = [b"user_shell\0".as_ptr(), core::ptr::null::<u8>()];
        let ret = exec("user_shell\0", &argv);
        write(2, b"initproc: exec user_shell failed\n");
        ret as i32
    } else {
        loop {
            let mut exit_code = 0;
            if wait(&mut exit_code) == -1 {
                yield_();
            }
        }
    }
}
```

这样做是为了让同一个内核同时适用于 QEMU、VisionFive V2、LoongArch
开发板和不同版本的测试盘。测试入口由宿主自动化脚本，或现场评审人员在
`>>` 提示符下明确输入的命令决定；不要为某一张卡硬编码路径或脚本名。

若临时把 `initproc` 改成自动跑脚本，必须放在新的测试分支中，完整验证后
恢复上述 shell 入口；不要污染 `finalsubmission`。

## 2. 推荐：一键运行 RISC-V 决赛测例

在 WSL 或 Linux 宿主机、仓库根目录执行：

```bash
git switch finalsubmission
make run-final-2026-rv \
  FINAL_2026_RV_IMAGE="$PWD/testsuits-for-oskernel-final/image/sdcard-rv-pub.img" \
  FINAL_2026_MODE=all \
  FINAL_2026_MEMORY=8G
```

该目标会依次：

1. 构建 `kernel-rv` 和 `disk.img`；
2. 为官方测试盘和根盘建立本轮独立的 QCOW2 overlay；
3. 以 RISC-V `-cpu max,v=true,vlen=128`、8 核启动 QEMU；
4. 等待 `Rust user shell >>`；
5. 自动输入挂载、将一个临时驱动脚本写进 `/mnt/tmp/`、`chroot` 到官方盘，
   运行 CAgent 与 BuildStorm；
6. 在 `artifacts/final2026/<时间>-riscv64-all/` 留下串口日志、QEMU 命令、
   输入哈希和两个 overlay 的一致性检查结果。

只跑某一部分时，替换模式：

```bash
# 官方 CAgent
make run-final-2026-rv FINAL_2026_RV_IMAGE="$PWD/testsuits-for-oskernel-final/image/sdcard-rv-pub.img" FINAL_2026_MODE=cagent

# BuildStorm 第 2.1 部分（工具链与最小构建）
make run-final-2026-rv FINAL_2026_RV_IMAGE="$PWD/testsuits-for-oskernel-final/image/sdcard-rv-pub.img" FINAL_2026_MODE=q21

# BuildStorm 完整脚本
make run-final-2026-rv FINAL_2026_RV_IMAGE="$PWD/testsuits-for-oskernel-final/image/sdcard-rv-pub.img" FINAL_2026_MODE=buildstorm
```

成功判据不是只看到 QEMU 启动，而是日志中至少有：

```text
FINAL2026_DRIVER_DONE rc=0
```

`all` 模式还必须有 10 个 `testcase cagent ... pass` 标记、
`BUILDSTORM_TOOLCHAIN ok`、`BUILDSTORM_MINIBUILD ok` 和
`BUILDSTORM_COMPILE mode=multi ok=true`。

## 3. 手动运行（现场演示或定位失败）

若需要手动操作，仍使用同样的双盘启动方式。可先构建：

```bash
make kernel-rv
make disk.img
```

随后按现有 Makefile 的双盘目标启动，`TEST_DISK` 必须是官方公版盘，
`ROOT_DISK` 必须是本项目根盘：

```bash
make run-dual-smp4-prebuilt ARCH=riscv64 \
  QEMU_CPU=max,v=true,vlen=128 \
  TEST_DISK="$PWD/testsuits-for-oskernel-final/image/sdcard-rv-pub.img" \
  ROOT_DISK="$PWD/disk.img"
```

在 `Rust user shell` 的 `>>` 后输入：

```bash
/busybox mount -t ext4 vda /mnt
/busybox chroot /mnt /bin/bash -c 'cd /glibc && ./cagent_testcode.sh'
```

运行 BuildStorm：

```bash
/busybox chroot /mnt /bin/bash -c 'cd /glibc && ./buildstorm_testcode.sh'
```

结束后输入：

```bash
/shutdown
```

不要在同一轮手动测试中直接写原始 `sdcard-*-pub.img`。正式复测应使用第 2
节的一键目标；它的 QCOW2 overlay 是可重复测试的必要条件。

## 4. LoongArch64

LoongArch 的磁盘语义相同：`vda` 是 `sdcard-la-pub.img`，`vdb` 是
`disk-la.img`；区别仅在 QEMU 使用 PCI virtio 设备。宿主环境具备
LoongArch QEMU 时：

```bash
make run-final-2026-la \
  FINAL_2026_LA_IMAGE="$PWD/testsuits-for-oskernel-final/image/sdcard-la-pub.img" \
  FINAL_2026_MODE=all
```

如只在官方容器内具备 LoongArch QEMU，先用官方容器构建 `kernel-la` 与
`disk-la.img`，再运行：

```bash
make run-final-2026-la-docker \
  FINAL_2026_LA_IMAGE="$PWD/testsuits-for-oskernel-final/image/sdcard-la-pub.img" \
  FINAL_2026_MODE=all
```

## 5. VisionFive V2 现场板说明

QEMU 中的 `vda`/`vdb` 是模拟器的两块 virtio 盘；VisionFive V2 使用的是
物理 SD 控制器和实际分区，二者不能把设备名直接照搬。板上仍应保持本节的
`initproc` shell 入口：启动后由评审或操作员挂载**已确认承载官方决赛目录
的物理测试分区**到 `/mnt`，检查 `/mnt/glibc` 后再 `chroot` 执行相同脚本。

在未确认 VF2 驱动将哪一个物理分区暴露为测试盘前，不要把
`/busybox mount -t ext4 vda /mnt` 硬编码到板上 initproc。应先用当前板卡
的分区枚举/挂载命令确认含有 `glibc/cagent_testcode.sh` 的分区；这保证现场
操作不会因 SD 卡布局变化而错误运行或误写别的分区。

## 6. 结果留存与判分

自动运行结束后，保留本轮目录而不要删除：

```bash
find artifacts/final2026 -maxdepth 2 -name serial.log -print
```

若本地有官方判分仓库，可对日志执行：

```bash
python3 testsuits-for-oskernel-final/judge/judge_cagent-glibc.py \
  < artifacts/final2026/<本轮目录>/serial.log | python3 -m json.tool
```

日志、提交哈希、原始测试镜像哈希、根盘哈希、QEMU 命令和 overlay 检查结果
共同构成可复查证据；只保存截图不足以证明使用了正确的公版镜像。
