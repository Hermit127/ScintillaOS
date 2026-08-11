# BuildStorm 官网输出 10 问题定位与终端非阻塞修复

## 1. 官网结果并非漏判编译成功

`Riscv输出-10.txt` 中，正式构建在 34 分 34 秒后完成，ELF 与 BIN 产物均已生成，
构建命令返回值也是 `rc=0`。但评测脚本随后还会启动生成的
`arceos-helloworld`，验证产物能否实际运行。该阶段输出：

```text
----- boot arceos-helloworld in qemu (untimed, arch=riscv64) -----
BUILDSTORM_RESULT mode=multi status=FAIL rc=0 cores=8 elapsed_s=2123.18 run=FAIL
qemu-system-riscv64: Failed to set FD nonblocking: Invalid argument
qemu-system-riscv64: could not connect serial device to character backend 'mon:stdio'
```

因此 RISC-V 的情况是“编译成功、产物启动验证失败”。评分器以最终
`BUILDSTORM_RESULT` 为准，`status=FAIL` 不能获得题 2.2 的编译成功门槛分与时间分。

`LoongArch输出-10.txt` 则停在 `Compiling ax-api v0.7.4`，没有输出构建完成、产物
验证或最终结果标记。LoongArch 的零分来自构建链路没有走到可结算位置，与
RISC-V 的终端错误不是同一个失败阶段。

## 2. 根因

QEMU 的 `mon:stdio` 字符后端会通过 `fcntl(F_SETFL)` 给继承的标准文件描述符设置
`O_NONBLOCK`。内核的 `sys_fcntl` 已经实现 `F_GETFL/F_SETFL`，但 `Stdin` 和
`Stdout` 没有覆盖 `File::set_flags`，因此落入默认实现并返回失败，用户态看到
`EINVAL`。这使运行在 ScintillaOS 用户态中的内层 QEMU 无法建立串口字符后端。

## 3. 修复设计

`Stdin` 与 `Stdout` 分别新增原子非阻塞状态。初始 fd 0、fd 1 和 fd 2 各自创建
独立的文件对象；fork 或 dup 仍通过现有 `Arc<dyn File>` 共享同一对象，因而符合
Linux 中 open file description 状态由复制后的描述符共享的语义。

`F_SETFL` 与 `FIONBIO` 现在可以更新标准终端对象的 `O_NONBLOCK`，`F_GETFL` 会返回
该状态。非阻塞 stdin 在调试串口没有可读字符时返回 `-EAGAIN`，阻塞 stdin 仍沿用
原有的睡眠等待路径；stdout/stderr 保存并报告标志，但输出仍可立即完成。

## 4. 验证

双架构内核检查均通过：

```text
cargo check -p os --target riscv64gc-unknown-none-elf
cargo check -p os --target loongarch64-unknown-none
```

随后构建带临时探针的正式 RISC-V release 内核，并以 `-smp 8` 启动。探针依次验证
fd 0、fd 1、fd 2 的 `F_SETFL(O_NONBLOCK)` 返回 0，fd 0 的 `F_GETFL` 包含
`O_NONBLOCK`，以及无输入时 read 返回 `-EAGAIN`。串口结果为：

```text
STDIO_FCNTL_NONBLOCK PASS
[kernel] shutdown requested by tid=1
```

临时探针在验证后已从 `initproc` 删除，正式评测顺序和脚本均未修改。本次修复只补齐
通用终端与 `fcntl` 语义，不识别 BuildStorm 进程、产物路径或评分输出。

## 5. 嵌套启动继续暴露的 Unix socket 用户指针问题

终端非阻塞修复后，使用已经完成正式构建的 RISC-V 测试盘，从交互诊断内核执行
`tg-xtask arceos qemu`。原来的 `Failed to set FD nonblocking` 已经消失，但
`cargo metadata` 初始化期间外层内核出现读缺页：

```text
[fault-diag] invalid page-fault dispatch cpu=4 tid=75 user=false
sepc=0xffffffc0802d2ece bad=0x98c1df
[kernel] panic: read page fault outside a valid user task
```

使用与该次运行完全一致的未剥离 ELF 反汇编，`sepc` 位于 `memcpy` 的源字节读取，返回地址
位于 `UnixSocketFile::send_with_flags` 调用 `copy_from_slice` 之后。寄存器同时显示目标地址是
内核堆，源地址 `0x98c1df` 是当前用户进程地址，长度为 1。继续检查 syscall 边界后确认，
`sys_sendto` 直接用 `slice::from_raw_parts` 把用户指针构造成内核切片；用户页尚未驻留或缓冲区
跨页时，内核态 `memcpy` 不会进入正常的用户缺页处理路径，而会直接 panic。

## 6. 安全拷贝与消息边界修复

`sendto` 现在先在不持有 socket 队列锁的状态下，通过当前任务的地址空间完成需求分页和
`copy_from_task_user`，再把内核暂存区交给 Unix、TCP 或 UDP 实现。`recvfrom` 先验证并准备
用户可写页，数据先接收到内核暂存区，成功后再通过 `copy_to_task_user` 回写。无效地址按
Linux 约定返回 `EFAULT`，不再把用户地址交给内核 `memcpy`。

暂存区上限与现有 socket 缓冲区一致，为 64 KiB。TCP 和 Unix stream 保留短写语义；UDP
超过单个数据报上限返回 `EMSGSIZE`。Unix seqpacket 由 socket 对象根据类型检查原始请求长度，
超过记录上限同样返回 `EMSGSIZE`，不会因暂存区截断而错误成功。空缓冲区继续允许空指针并
返回 0。用户页准备和复制都发生在获取 `UnixSocketState`、本地 TCP/UDP 队列或 smoltcp
设备锁之前，因此没有引入“地址空间锁嵌套设备锁”的新锁序。

## 7. 构建产物启动验证

修改后 RISC-V 与 LoongArch 的 `cargo check` 均通过。独立临时 worktree 以正式提交
`09642b80` 为基线，只叠加 socket 安全拷贝和临时 shell 入口，重新生成 8 核 RISC-V
release 内核。正式 `initproc` 没有修改。

在 `-smp 8 -m 8G` 外层系统中再次运行同一 `tg-xtask arceos qemu` 路径，工具可以完成
`cargo metadata`、10.60 秒增量构建并真正启动内层 `qemu-system-riscv64`，原 Unix socket
缺页 panic 没有再出现。镜像内精简版 QEMU 缺少 `efi-virtio.rom`，因此本地诊断为块设备和
网卡显式使用空 `romfile`，并显式指定镜像已有的 OpenSBI；VirtIO 网卡本身仍然保留。Mac
上的双层 TCG 在 300 秒内稳定到达 ArceOS 主入口和启动横幅，但因双层模拟速度过慢，在应用
输出前被诊断超时终止，外层内核始终保持运行。

为排除构建产物本身的问题，从同一成功测试盘提取正式 ELF，得到：

```text
size=1681000
sha256=74e580679b57fa61db8640f615240af322162000444f00c31c34bf2ee50409e7
```

随后在宿主机使用同样的 512 MiB、单核、VirtIO 块设备、VirtIO 网卡和 `mon:stdio` 参数进行
单层启动。产物完整输出 ArceOS 架构信息与 `Hello, world!`，QEMU 返回 0。该对照证明正式
构建产物可运行；本地双层未在时限内到达应用输出是嵌套 TCG 性能限制，而不是 ELF、根文件
系统或虚拟网卡被删除。修复仍是通用用户内存语义，不检查 QEMU、Cargo 或测试名称。
