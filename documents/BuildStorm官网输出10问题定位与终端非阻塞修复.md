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
