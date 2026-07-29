# CAgent 双架构终端测试指南

本文记录在本项目中使用 RISC-V64 和 LoongArch64 运行 2026 年内核赛道决赛 CAgent 测试的完整终端步骤。

CAgent 使用官方 glibc 环境，每个架构均采用双盘：

- 第一块盘 `vda`：官方决赛测试镜像。
- 第二块盘 `vdb`：本项目构建的根文件系统。
- 启动后由 Rust user shell 手动挂载 `vda`，再通过 `chroot` 运行官方脚本。

官方 CAgent 共 10 项，满分 200 分。建议使用 QCOW2 写时复制层运行，避免测试或异常关机修改 14 GiB 的原始镜像。

## 1. 公共准备

进入项目目录：

```bash
cd /home/dyh/os26/oskernel2026test
git switch main
```

确认测试仓库、判分器和两个官方镜像存在：

```bash
test -f testsuits-for-oskernel-final/scripts/cagent_testcode.sh
test -f testsuits-for-oskernel-final/judge/judge_cagent-glibc.py
test -f testsuits-for-oskernel-final/image/sdcard-rv-pub.img
test -f testsuits-for-oskernel-final/image/sdcard-la-pub.img
```

确认宿主机工具：

```bash
command -v qemu-img
command -v qemu-system-riscv64
command -v qemu-system-loongarch64
command -v python3
```

查看版本：

```bash
qemu-system-riscv64 --version
qemu-system-loongarch64 --version
```

本项目已经验证过 QEMU 8.2.2。RISC-V 官方 glibc 程序需要支持 RISC-V Vector 1.0，因此下面固定使用：

```text
-cpu max,v=true,vlen=128
```

## 2. RISC-V64 CAgent

### 2.1 构建内核和根盘

强制从当前代码重新构建，避免误用旧产物：

```bash
make -B kernel-rv ARCH=riscv64
make -B disk.img ARCH=riscv64
```

检查产物：

```bash
file kernel-rv
file disk.img
ls -lh kernel-rv disk.img
```

预期：

- `kernel-rv` 是 RISC-V 64 位 ELF。
- `disk.img` 是 ext4 文件系统镜像。

### 2.2 创建本轮可写层

每次冷启动测试使用一个新的目录名，不要复用旧的 QCOW2：

```bash
CAGENT_RV_RUN=target/cagent-rv-run-01
mkdir -p "$CAGENT_RV_RUN"
```

为官方测试盘创建写时复制层：

```bash
qemu-img create -q -f qcow2 -F raw \
  -b "$PWD/testsuits-for-oskernel-final/image/sdcard-rv-pub.img" \
  "$CAGENT_RV_RUN/test.qcow2"
```

为本项目根盘创建写时复制层：

```bash
qemu-img create -q -f qcow2 -F raw \
  -b "$PWD/disk.img" \
  "$CAGENT_RV_RUN/root.qcow2"
```

检查写时复制关系：

```bash
qemu-img info "$CAGENT_RV_RUN/test.qcow2"
qemu-img info "$CAGENT_RV_RUN/root.qcow2"
```

### 2.3 启动 RISC-V 四核 QEMU

```bash
qemu-system-riscv64 \
  -machine virt \
  -cpu max,v=true,vlen=128 \
  -kernel "$PWD/kernel-rv" \
  -display none \
  -chardev stdio,id=serial0,signal=off,logfile="$PWD/$CAGENT_RV_RUN/serial.log" \
  -serial chardev:serial0 \
  -monitor none \
  -m 256M \
  -smp 4 \
  -drive file="$PWD/$CAGENT_RV_RUN/test.qcow2",if=none,format=qcow2,id=x0 \
  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
  -drive file="$PWD/$CAGENT_RV_RUN/root.qcow2",if=none,format=qcow2,id=x1 \
  -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1 \
  -netdev user,id=net0 \
  -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2
```

正常启动时应看到：

```text
Boot HART Count           : 4
[smp] online mask 0xf
Rust user shell
>>
```

### 2.4 在 guest 中运行官方 CAgent

出现 `>>` 后，先挂载第一块盘：

```bash
/busybox mount -t ext4 vda /mnt
```

可选：先验证 glibc Bash：

```bash
/busybox chroot /mnt /bin/bash --version
```

运行未修改的官方 CAgent 脚本：

```bash
/busybox chroot /mnt /bin/bash -c 'cd /glibc; ./cagent_testcode.sh'
```

成功进入测试后应出现：

```text
#### OS COMP TEST GROUP START cagent ####
```

完整结束时应出现：

```text
#### OS COMP TEST GROUP END cagent ####
>>
```

关闭系统：

```bash
/shutdown
```

### 2.5 RISC-V 判分

查看逐项 JSON：

```bash
python3 testsuits-for-oskernel-final/judge/judge_cagent-glibc.py \
  < "$CAGENT_RV_RUN/serial.log" \
  | python3 -m json.tool
```

查看通过数量和总分：

```bash
python3 testsuits-for-oskernel-final/judge/judge_cagent-glibc.py \
  < "$CAGENT_RV_RUN/serial.log" \
  | python3 -c 'import json,sys; r=json.load(sys.stdin); print("通过：{}/{}".format(sum(x["pass"] for x in r),len(r))); print("得分：{:.2f}/200".format(sum(x["score"] for x in r)))'
```

快速查看串口中的测试记录：

```bash
grep -a 'testcase cagent' "$CAGENT_RV_RUN/serial.log"
```

当前 `main` 工作树（包含中间路径符号链接修复）在 2026-07-29 的实测结果为 9/10，通过分数 177.10/200；当时仅 `fs-directory` 被判定为 reject。

## 3. LoongArch64 CAgent

### 3.1 构建内核和根盘

只重建 LoongArch 内核：

```bash
make kernel ARCH=loongarch64
cp target/loongarch64-unknown-none/release/os kernel-la
```

如果已经有可用的 `disk-la.img`，无需每次随内核一起重建。首次生成或用户程序发生变化时执行：

```bash
make fs-img ARCH=loongarch64
cp target/loongarch64-unknown-none/release/fs.img disk-la.img
```

检查产物：

```bash
file kernel-la
file disk-la.img
ls -lh kernel-la disk-la.img
```

预期：

- `kernel-la` 是 LoongArch 64 位 ELF。
- `disk-la.img` 是 ext4 文件系统镜像。

如果构建阶段提示缺少目标或工具链，先执行：

```bash
rustup target add loongarch64-unknown-none
rustup component add rust-src llvm-tools-preview
```

`basic/user` 中的 C 测试还可能需要 LoongArch 交叉编译器；已有预编译目录时，Makefile 会直接使用：

```bash
ls basic/user/build/loongarch64
```

### 3.2 创建本轮可写层

每次测试改用新的目录名：

```bash
CAGENT_LA_RUN=target/cagent-la-run-01
mkdir -p "$CAGENT_LA_RUN"
```

为官方测试盘创建写时复制层：

```bash
qemu-img create -q -f qcow2 -F raw \
  -b "$PWD/testsuits-for-oskernel-final/image/sdcard-la-pub.img" \
  "$CAGENT_LA_RUN/test.qcow2"
```

为本项目根盘创建写时复制层：

```bash
qemu-img create -q -f qcow2 -F raw \
  -b "$PWD/disk-la.img" \
  "$CAGENT_LA_RUN/root.qcow2"
```

检查写时复制关系：

```bash
qemu-img info "$CAGENT_LA_RUN/test.qcow2"
qemu-img info "$CAGENT_LA_RUN/root.qcow2"
```

### 3.3 启动 LoongArch QEMU

LoongArch 使用 PCI virtio-blk。CAgent 的 server 与 agent 通过 `127.0.0.1` 通信，内核使用软件本地 TCP 路径，因此此处不需要添加外部虚拟网卡。下面的 1 GiB 单核配置已经完整跑通官方脚本。

```bash
qemu-system-loongarch64 \
  -machine virt \
  -kernel "$PWD/kernel-la" \
  -display none \
  -chardev stdio,id=serial0,signal=off,logfile="$PWD/$CAGENT_LA_RUN/serial.log" \
  -serial chardev:serial0 \
  -monitor none \
  -m 1G \
  -smp 1 \
  -drive file="$PWD/$CAGENT_LA_RUN/test.qcow2",if=none,format=qcow2,id=x0 \
  -device virtio-blk-pci,drive=x0 \
  -drive file="$PWD/$CAGENT_LA_RUN/root.qcow2",if=none,format=qcow2,id=x1 \
  -device virtio-blk-pci,drive=x1
```

等待出现：

```text
Rust user shell
>>
```

LoongArch 当前使用稳定的单核配置。CAgent 本身不要求固定核数；如果后续已经完成 LoongArch SMP 验证，可将 `-smp 1` 改为相应核数。

### 3.4 在 guest 中运行官方 CAgent

guest 内命令与 RISC-V 相同：

```bash
/busybox mount -t ext4 vda /mnt
```

验证 glibc Bash：

```bash
/busybox chroot /mnt /bin/bash --version
```

运行官方测试：

```bash
/busybox chroot /mnt /bin/bash -c 'cd /glibc; ./cagent_testcode.sh'
```

成功启动和完整结束时分别会出现：

```text
#### OS COMP TEST GROUP START cagent ####
#### OS COMP TEST GROUP END cagent ####
>>
```

测试完成后关闭：

```bash
/shutdown
```

### 3.5 LoongArch 判分

查看逐项 JSON：

```bash
python3 testsuits-for-oskernel-final/judge/judge_cagent-glibc.py \
  < "$CAGENT_LA_RUN/serial.log" \
  | python3 -m json.tool
```

查看通过数量和总分：

```bash
python3 testsuits-for-oskernel-final/judge/judge_cagent-glibc.py \
  < "$CAGENT_LA_RUN/serial.log" \
  | python3 -c 'import json,sys; r=json.load(sys.stdin); print("通过：{}/{}".format(sum(x["pass"] for x in r),len(r))); print("得分：{:.2f}/200".format(sum(x["score"] for x in r)))'
```

查看测试记录：

```bash
grep -a 'testcase cagent' "$CAGENT_LA_RUN/serial.log"
```

当前 `main` 工作树在 2026-07-29 使用普通发布构建和 QEMU 8.2.2 实测完整跑完 10 项，结果为 10/10 全部通过。仓库内 `judge_cagent-glibc.py` 的 10 项权重之和为 181，因此本地判分器全通过时的原始总分是 181.00。复核文件位于：

```text
target/cagent-la-main-r19/serial.log
target/cagent-la-main-r19/judge.json
```

## 4. 不创建 QCOW2，直接测试

只有在允许原始镜像被写入时才使用本节。CAgent 会创建和删除文件，ext4 挂载、日志回放或异常关机也可能改变镜像。重复测试时，结果可能受到上一次运行残留的影响。

### 4.1 RISC-V 直接运行

```bash
script -q -c \
  'make run-dual-smp4-prebuilt ARCH=riscv64 QEMU_CPU=max,v=true,vlen=128 TEST_DISK=testsuits-for-oskernel-final/image/sdcard-rv-pub.img ROOT_DISK=disk.img' \
  target/cagent-rv-direct.log
```

guest 内执行：

```bash
/busybox mount -t ext4 vda /mnt
/busybox chroot /mnt /bin/bash -c 'cd /glibc; ./cagent_testcode.sh'
/shutdown
```

判分：

```bash
python3 testsuits-for-oskernel-final/judge/judge_cagent-glibc.py \
  < target/cagent-rv-direct.log \
  | python3 -c 'import json,sys; r=json.load(sys.stdin); print("{:.2f}/200".format(sum(x["score"] for x in r)))'
```

### 4.2 LoongArch 直接运行

```bash
script -q -c \
  'make run-dual-prebuilt ARCH=loongarch64 QEMU_SMP=1 TEST_DISK=testsuits-for-oskernel-final/image/sdcard-la-pub.img ROOT_DISK=disk-la.img' \
  target/cagent-la-direct.log
```

guest 内执行：

```bash
/busybox mount -t ext4 vda /mnt
/busybox chroot /mnt /bin/bash -c 'cd /glibc; ./cagent_testcode.sh'
/shutdown
```

判分：

```bash
python3 testsuits-for-oskernel-final/judge/judge_cagent-glibc.py \
  < target/cagent-la-direct.log \
  | python3 -c 'import json,sys; r=json.load(sys.stdin); print("{:.2f}/200".format(sum(x["score"] for x in r)))'
```

## 5. 常见问题

### 5.1 `/bin/bash` 报 `Operation not permitted`

典型输出：

```text
chroot: can't execute '/bin/bash': Operation not permitted
```

本项目曾因 VFS 不跟随 `/bin -> usr/bin` 这种中间路径符号链接而出现该问题。确认 `os/src/fs/vfs.rs` 中的 `vfs_resolve_path` 会跟随非最终分量的符号链接，然后强制重建对应架构内核：

```bash
make -B kernel-rv ARCH=riscv64
```

或：

```bash
make -B kernel-la ARCH=loongarch64
```

### 5.2 挂载后找不到 `/glibc`

确认挂载的是第一块官方测试盘：

```bash
/busybox mount -t ext4 vda /mnt
/busybox ls /mnt
```

正常应看到 `glibc`。如果挂载的是根盘或双盘顺序错误，需要检查 QEMU 中 `x0`、`x1` 以及两个 `-device` 的顺序。

### 5.3 判分全部为 0，执行时间也是 0

这表示日志中没有：

```text
testcase cagent NAME pass|reject TIME
```

先检查测试是否真正进入开始和结束标记：

```bash
grep -a 'OS COMP TEST GROUP' "$CAGENT_RV_RUN/serial.log"
```

LoongArch 对应：

```bash
grep -a 'OS COMP TEST GROUP' "$CAGENT_LA_RUN/serial.log"
```

### 5.4 QEMU 报镜像被占用

确认没有另一个 QEMU 进程正在使用同一 QCOW2 文件。每个并行或重复测试都应使用不同的运行目录和写时复制层。

### 5.5 再跑一轮

不要覆盖已有证据。改用新的目录名：

```bash
CAGENT_RV_RUN=target/cagent-rv-run-02
```

或：

```bash
CAGENT_LA_RUN=target/cagent-la-run-02
```

然后重新执行对应架构的 QCOW2 创建、QEMU 启动和判分步骤。

### 5.6 全量构建提示 CMake 不存在

如果 `make -B kernel-la` 或 `make fs-img ARCH=loongarch64` 报错类似：

```text
/home/dyh/.local/bin/cmake: ... /tmp/cmake_local/usr/bin/cmake: No such file or directory
```

说明宿主机的 `cmake` 包装器已经失效，不是内核编译错误。仅修改内核时直接使用：

```bash
make kernel ARCH=loongarch64
cp target/loongarch64-unknown-none/release/os kernel-la
```

确实需要重建 `disk-la.img` 时，应先安装或修复宿主机 CMake，再执行 3.1 节的根盘构建命令。

### 5.7 LoongArch 启动后在 glibc/CAgent 中异常退出

当前代码已经补齐 CAgent 所需的 LoongArch 支持，包括：

- 兼容 QEMU 8.2 的 FDT 内存区编码。
- 从页表项正确还原读、执行权限。
- 启用并在任务切换时保存、恢复全部 LSX 寄存器。
- 按 LoongArch glibc 启动 ABI 初始化入口寄存器。
- 提供 LoongArch `rt_sigreturn` 用户态跳板。
- 隔离根文件系统和 `vda` 测试盘在 lwext4 中的挂载路径。
- 根据文件描述符指向的实际对象判断终端，确保 pipe 上的 `TCGETS` 和 `TIOCGWINSZ` 返回 `ENOTTY`。

修改这些位置后必须重新执行 `make kernel ARCH=loongarch64` 并更新 `kernel-la`，否则 QEMU 仍会加载旧内核。

## 6. 测试完成后的检查

确认写时复制层没有损坏：

```bash
qemu-img check "$CAGENT_RV_RUN/test.qcow2"
qemu-img check "$CAGENT_RV_RUN/root.qcow2"
```

或：

```bash
qemu-img check "$CAGENT_LA_RUN/test.qcow2"
qemu-img check "$CAGENT_LA_RUN/root.qcow2"
```

保留以下文件即可复核本轮结果：

- `serial.log`：完整串口输出和官方 judge 的输入。
- `test.qcow2`：本轮测试盘写入内容。
- `root.qcow2`：本轮根盘写入内容。

原始 `sdcard-rv-pub.img`、`sdcard-la-pub.img`、`disk.img` 和 `disk-la.img` 不应因 QCOW2 模式下的测试而改变。
