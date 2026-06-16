2026/6/16 hermit

把 MachineTest 上已经被官网确认 Accepted 的修改迁移到 main。

官网评测信息：
最后一次提交时间：2026-06-15 23:05:23
评测开始时间：2026-06-15 23:05:55.202686+08:00
评测结束时间：2026-06-16 00:26:59.089374+08:00
总分：1244.8785790593415
迁移来源提交：005cb597 Use real block capacity for contest disks

得分：
basic：glibc-la 102，glibc-rv 102，musl-la 102，musl-rv 102，总分 408
busybox：四种环境各 54，总分 216
iozone：四种环境各 20，总分 80
lmbench：glibc-la 45.913640293863466，glibc-rv 43.86551695923608，musl-la 46.2083403456853，musl-rv 43.8910814605567，总分 179.87857905934155
lua：四种环境各 9，总分 36
libctest：musl-la 161，musl-rv 164，总分 325
iperf、ltp、netperf、cyclictest、libcbench：本次仍为 0

这次迁移原则：
1. main 分支仍然保留 user/src/bin/initproc.rs 的 shell 入口，不迁移 MachineTest 上自动跑评测的 initproc。
2. 不迁移磁盘镜像、QEMU 日志、临时测试盘等产物。
3. 不迁移 vendor/polyhal-trap 里的改动，避免继续污染 vendor。
4. 迁移 MachineTest 已经被官网验证过的内核、文件系统、执行器、用户库和评测脚本改动。
5. 2026-06-16 凌晨在 MachineTest 上做过的一批 iperf 实验没有进入本次迁移，已经保存在 stash：
   stash@{2026-06-16 01:10:30 +0800}: On MachineTest: pre-main-merge local experiments 2026-06-16


2026/6/16
官方 sdcard / 测试盘排查结论

之前官网日志里可以看到：
qemu-system-riscv64 ... -drive file=sdcard-rv.img ... id=x0 ... -drive file=disk-rv.img ... id=x1
qemu-system-loongarch64 ... -drive file=sdcard-la.img ... id=x0 ... -drive file=disk-la.img ... id=x1

这里真正的评测用例不是我们自己塞进 rootfs 的固定输出，而是在官方提供的 sdcard 里。
内核启动后有两块 virtio-blk。按官网命令顺序，第一块 `sdcard-rv.img` / `sdcard-la.img` 会被内核识别为 `vda`，它才是官方测试盘；第二块 `disk-rv.img` / `disk-la.img` 会被识别为 `vdb`，它是我们构建出的 rootfs。
我们在本地找到并保存过官方镜像：`tmp/official-20250615/sdcard-rv.img` 和 `tmp/official-20250615/sdcard-la.img`。用 Docker 里的 `debugfs` 读取 `sdcard-rv.img` 根目录，可以看到 `/musl`、`/glibc`、`/lost+found`。在 QEMU 里用 busybox 手动挂载也能复现：`/busybox mount -t ext4 vda /mnt` 后 `/mnt` 下是 `glibc`、`musl`、`lost+found`；而挂载 `vdb` 后看到的是 rootfs 内容，例如 `bin`、`busybox`、`contest.sh`、`basic_testcode.sh` 等。
因此这次修复里提到的官方 sdcard / 测试盘，精确地说就是第一块 virtio-blk，也就是 `vda`。

这件事后来通过 initproc 的 mount 日志和 getdents 日志确认：
mount vda -> /mnt 成功后，/mnt/musl/ 和 /mnt/glibc/ 能看到官方脚本。
官网卡在 “scan known /mnt/glibc/” 或 “/mnt/musl/ has N known test scripts” 时，本质是我们已经找到官方 sdcard，但执行脚本、动态链接、文件系统读写或进程调度中某一步卡住。

因此这段时间的正确修复方向是：
1. 让内核能稳定挂载官方第一块盘，也就是 `vda`。
2. 让 exec 能从 /mnt/musl 和 /mnt/glibc 执行官方二进制。
3. 让 busybox sh、动态链接器、libc、读写文件、pipe、fork/wait/signal 能支撑官方脚本。
4. 不把测试结果写死在内核或用户程序里。


2026/6/16
构建和双盘相关修改

Makefile：
1. kernel-rv / kernel-la 目标用于生成官网提交需要的 kernel-rv、kernel-la。
2. run-dual 目标用于本地模拟官网双盘运行方式，把 rootfs 和官方测试盘同时挂到 QEMU。
3. fs-img 目标里增加了 busybox、busybox-glibc、lmbench、iozone、lua、LTP 脚本的复制逻辑。
4. LTP_BIN_DIR、LTP_MODE 只负责把已有 LTP 二进制和脚本放进 rootfs，main 上没有把 initproc 改成自动跑 LTP。
5. 生成 busybox_glibc_testcode.sh 时，把 busybox_cmd.txt 里的 ./busybox 替换成 ./busybox-glibc，避免 glibc 版 busybox 调到 musl 版二进制。

os/src/drivers/block/virtio_blk.rs：
1. VirtIOBlock::new / probe 相关逻辑增加了对多块 virtio-blk 的识别。
2. 在 RISC-V 上扫描多个 MMIO virtio 地址，官网命令里第一个 virtio-blk 是官方 sdcard/test disk，第二个 virtio-blk 是我们构建出的 disk/rootfs。
3. 在 LoongArch 上通过 PCI virtio-blk probe 获取两块盘。
4. 修复容量获取逻辑，后续 005cb597 使用真实 block capacity，避免把官方测试盘容量识别错导致 ext4 读写越界或目录扫描异常。

os/src/drivers/block/mod.rs：
1. BLOCK_DEVICE 和 ROOT_BLOCK_DEVICE 的初始化支持多盘。
2. rootfs 和官方测试盘不再假设同一块设备。
3. mount("vda", "/mnt", "ext4", ...) 这类路径能落到正确的 block device。

os/src/drivers/block/device.rs：
1. BlockDevice trait 暴露真实容量查询。
2. 文件系统层可以基于块设备真实大小限制读写范围。

os/src/fs/block_device_stream.rs：
1. BlockDeviceStream::read_offset / write_offset 适配真实块设备容量。
2. 对非块对齐读写做扇区级读改写，避免 ext4 访问官方测试盘时破坏邻近数据。
3. Ext4Wrapper 仍然通过 lwext4 访问 ext4，不在这里理解评测目录结构。


2026/6/16
lwext4 和 VFS 相关修改

lwext4_rust/src/blockdev.rs：
1. 每次注册 lwext4 block device 使用唯一设备名，避免多次 mount 时 lwext4 因设备名重复 panic。
2. 这是支持 rootfs + 官方 sdcard/test disk 同时挂载的基础。

lwext4_rust/c/lwext4/src/ext4.c：
1. 修复 mount path 匹配问题，避免 /mnt 和 /mnt/musl 等路径在 lwext4 内部匹配到错误挂载点。
2. 这影响 open、stat、readlink、symlink 和目录扫描。

os/src/fs/vfs.rs：
1. vfs_resolve_path 负责从 cwd/root 和传入路径解析 Dentry。
2. Dentry::children 缓存目录项，配合 getdents64 减少重复读目录。
3. ROOT_DENTRY / ROOT_SB 初始化 rootfs。
4. mount 相关函数把官方测试盘挂载到 /mnt，使 /mnt/musl、/mnt/glibc 成为真实目录而不是 rootfs 里的假目录。
5. follow_symlink 在执行动态链接器、busybox applet 和 libc symlink 时会被调用。

os/src/fs/inode.rs：
1. open_file_at 支持从当前 cwd、dirfd 和绝对路径打开文件。
2. mkdir_at、unlink_at、rename_at、symlink_at、link_at 适配 busybox/lua/lmbench/iozone 对文件系统的调用。
3. OSInode::read 不直接依赖一次大 ext4_fread，而是经过 read_at/offset 逻辑，避免大块读污染用户缓冲。
4. OSInode::write 更新文件内容并维护 offset，iozone 的连续写、随机写、pwrite/pread 路径依赖这里。
5. OSInode::stat_inner / stat 返回 uid/gid、mode、size、blocks 等信息，basic、busybox、lmbench 都会检查。
6. special_device_stat 给 /dev/null、/dev/zero、/dev/random、/dev/urandom、/dev/rtc 等路径提供 stat 结果。

os/src/fs/mod.rs：
1. Kstat、S_IFDIR、S_IFREG、S_IFCHR、S_IFIFO 等常量用于 fstat/statx/getdents。
2. File trait 增加 path/is_dir/poll/get_flags/set_flags 等接口，让普通文件、pipe、socket 能统一放入 fd_table。

os/src/fs/pipe.rs：
1. PipeRingBuffer::read_buf / write_buf 实现 pipe 环形缓冲。
2. Pipe::read 在空管道时阻塞并释放 stdout owner，避免多个测试进程输出时互相卡死。
3. Pipe::write 在满管道时阻塞，读端关闭时返回 EPIPE 语义。
4. wake_pipe_read_tasks / wake_pipe_write_tasks 用于解除等待 pipe 的任务。
5. make_pipe 创建读端和写端，sys_pipe2 做 flags 校验和 cloexec 标记。

os/src/fs/stdio.rs：
1. 增加 stdout owner 机制，减少多进程同时写控制台导致输出交错。
2. stdout_release_if_owner 在阻塞、yield、等待 pipe/socket 时释放 owner，避免卡死。


2026/6/16
exec、动态链接和内存相关修改

os/src/mm/memory_set.rs：
1. MemorySet::from_elf 解析 ELF LOAD 段，把代码、数据、bss 映射进用户地址空间。
2. MemorySet::from_elf 增加 PIE 支持，ET_DYN 主程序使用固定偏移加载，修复动态链接程序入口和段地址。
3. MemorySet::from_elf 解析 PT_TLS，返回 tls_base 和 tls_size，glibc/musl 启动依赖 TLS。
4. MemorySet::from_elf 解析 PT_INTERP，返回 interp_path，用于加载 /lib/ld-musl-*.so.1 或 glibc loader。
5. MemorySet::load_interp_elf 把动态链接器加载到固定区域，并返回解释器入口。
6. MemorySet::from_existed_user 用于 fork，复制/共享用户地址空间。
7. MemorySet::handle_page_fault 处理 demand paging、COW 写缺页和 mmap 文件缺页，lmbench 的 page fault、mmap、fork 测试依赖这里。
8. MemorySet::insert_file_area 用于 mmap 文件区域，iozone/lmbench 会走到。
9. valid_user_buf 用于 syscall 参数检查，避免错误用户指针把内核写崩。

os/src/task/task.rs：
1. TaskControlBlock::exec 从文件系统读取 ELF，调用 MemorySet::from_elf 创建新地址空间。
2. TaskControlBlock::exec 在存在 PT_INTERP 时读取解释器 ELF，并调用 MemorySet::load_interp_elf。
3. TaskControlBlock::exec 构造 argv/envp/auxv，增加 AT_PHDR、AT_PHENT、AT_PHNUM、AT_ENTRY、AT_EXECFN 等字段，glibc/musl 初始化依赖这些 auxv。
4. TaskControlBlock::exec 设置 TLS 指针，支持动态链接程序。
5. TaskControlBlock::fork / fork_with_options 复制 trap frame、cwd/root、fd_table、信号状态、uid/gid、pgid/sid 等。
6. TaskControlBlockInner::clear_fd_table / close_on_exec 处理 exec 时的 FD_CLOEXEC。
7. TaskControlBlockInner::alloc_fd / close_fd 支持 open、pipe、socket、dup、accept 等分配 fd。

os/src/task/fp.rs：
1. 新增 UserFloatState，保存/恢复用户浮点寄存器。
2. lmbench、lua、glibc 里的浮点或 memcpy 优化路径需要不破坏用户浮点状态。

os/src/task/mod.rs：
1. add_initproc 添加初始用户进程。
2. exit_current_and_run_next 处理进程退出、子进程交给 init、唤醒 waitpid。
3. wake_blocked_task、wake_pipe_read_tasks、wake_pipe_write_tasks、wake_poll_wait_tasks 处理阻塞任务唤醒。
4. SignalFlags 改为 64 位 bitflags，能容纳 Linux signal 范围。
5. check_signals / handle_signals 处理 SIGSEGV、SIGCHLD、SIGTERM、SIGKILL 等。

os/src/task/action.rs：
1. SignalAction / SignalActions 保存 sigaction 表。
2. sigaction、sigreturn、lmbench 信号测试依赖这里的 handler/restorer 信息。

os/src/task/manager.rs：
1. PID2TCB 管理 pid 到 TaskControlBlock 的映射。
2. pid2task 用于 kill、wait、信号投递和调试。


2026/6/16
系统调用相关修改

os/src/syscall/mod.rs：
1. syscall 统一分发 Linux ABI syscall number。
2. LAST_SYSCALL_ID、LAST_SYSCALL_ARG0/1/2、LAST_SYSCALL_PID 用于 page fault 时定位最后一次 syscall。
3. 增加/接通 readv、writev、sendfile、statx、ppoll、pselect6、socket 系列、clock_gettime、futex、mremap、madvise 等入口。
4. 未实现 syscall 返回 -38，不在这里硬编码测试成功。

os/src/syscall/fs.rs：
1. sys_openat 处理 O_CREAT、O_TRUNC、O_DIRECTORY、O_CLOEXEC、AT_FDCWD。
2. sys_read / sys_write 对 fd_table 中的 File trait 调用 read/write，socket 和 pipe 也在这里统一处理。
3. sys_close 调用 TaskControlBlockInner::close_fd。
4. sys_pipe2 校验 O_NONBLOCK/O_CLOEXEC，创建 pipe 并写回两个 fd。
5. sys_dup、sys_dup3、sys_fcntl 支持 busybox/lmbench 常用 fd 操作。
6. sys_getdents64 读取目录项，官方 sdcard 的 /mnt/musl、/mnt/glibc 扫描依赖这里。
7. sys_fstat、sys_fstatat、sys_statx 生成 Linux 风格 stat 结构，basic/busybox/lua/lmbench 都会调用。
8. sys_readlinkat 支持动态链接器 symlink 和 busybox applet symlink。
9. sys_mkdirat、sys_unlinkat、sys_renameat2、sys_symlinkat、sys_linkat 支持 busybox 文件操作。
10. sys_lseek、sys_pread64、sys_pwrite64、sys_readv、sys_writev 支持 iozone 和 libcbench/lmbench 常见 IO。
11. sys_mount / sys_umount2 支持把官方测试盘挂载到 /mnt。
12. sys_ioctl 提供 termios、winsize、RTC 等最小兼容。
13. sys_statfs / sys_fstatfs 返回文件系统容量信息，busybox df/stat 等依赖这里。

os/src/syscall/process.rs：
1. sys_clone / sys_fork 创建新任务，basic clone/fork、busybox sh、lmbench 多进程测试依赖这里。
2. sys_execve 调用 TaskControlBlock::exec，支持从官方 sdcard 执行 ELF。
3. sys_waitpid / sys_waitid 等待子进程，并按 Linux wait status 写回 exit code。
4. sys_exit / sys_exit_group 结束进程并触发父进程唤醒。
5. sys_getpid、sys_getppid、sys_gettid、sys_getuid、sys_geteuid、sys_getgid、sys_getegid、sys_setuid、sys_setgid 支持 basic/libctest。
6. sys_brk、sys_mmap、sys_munmap、sys_mprotect、sys_madvise、sys_mremap 支持 libc 堆、mmap、lmbench、iozone。
7. sys_gettimeofday、sys_clock_gettime、sys_times、sys_nanosleep 提供时间接口，lmbench/lua/busybox 依赖。
8. sys_rt_sigaction、sys_rt_sigprocmask、sys_rt_sigreturn、sys_kill 处理信号。
9. sys_ppoll、sys_pselect6 处理 pipe/socket/stdio 的等待，iperf 后续也会依赖这里。
10. sys_futex 提供最小 futex wait/wake，glibc/pthread/动态链接程序可能会走到。
11. sys_getrusage、sys_uname、sys_syslog、sys_umask、sys_getrandom 等提供 libc/busybox 兼容。


2026/6/16
网络相关现状

os/src/net/mod.rs：
1. NET_IFACE 初始化 smoltcp iface。
2. net_poll 驱动网卡收发。
3. RISC-V/LoongArch 没有可用网卡时需要优雅降级，不能影响 basic/busybox/lua/lmbench/iozone。

os/src/net/socket.rs：
1. SocketFile 把 socket 放进 fd_table。
2. SocketFile::read / write 对 TCP/UDP 调用 net/syscall.rs 中的 tcp_recv、tcp_send、udp_recv、udp_write。
3. SocketFile::poll 给 ppoll/pselect6 判断 socket 是否可读/可写。

os/src/net/syscall.rs：
1. sys_socket 创建 TCP/UDP socket。
2. sys_bind、sys_listen、sys_accept、sys_connect 支持服务端/客户端连接。
3. sys_sendto、sys_recvfrom、sys_sendmsg、sys_recvmsg 支持 UDP/TCP 数据收发。
4. sys_setsockopt、sys_getsockopt 处理 SO_REUSEADDR、SO_RCVBUF、SO_SNDBUF、TCP_NODELAY 等兼容选项。
5. sys_getsockname、sys_getpeername、sys_sock_shutdown 给 iperf/busybox 网络测试提供基本接口。
6. 目前官网 iperf 仍为 0，说明这里已有框架但存在实际协议、阻塞、线程或 loopback 语义 bug。

后续修 iperf 时不能硬编码 iperf 输出。
应该查：
1. iperf3 -s -D 后 daemon/父进程退出和 SIGCHLD/wait 是否正确。
2. clone flags，尤其 CLONE_FILES、CLONE_VM、CLONE_SETTLS、CLONE_CHILD_CLEARTID 对 pthread 的影响。
3. UDP connected socket 的 read/write、sendto/recvfrom、sendmsg/recvmsg 是否共享同一个 local endpoint。
4. SocketFile::poll 是否能看到本地 UDP/TCP 队列就绪。
5. ppoll/pselect6 是否会在 socket 可读/可写后唤醒。
6. VirtIO-net 探测和本地 loopback 不是一回事，先确认 127.0.0.1 环路能闭环，再看真实 virtio-net。


2026/6/16
用户态和评测脚本相关修改

user/src/lib.rs：
1. syscall 封装统一使用 6 参数形式。
2. 增加 socket、sendto、recvfrom、sendmsg、recvmsg、setsockopt、getsockopt、clock_gettime、futex 等封装。
3. 用户态测试程序可以通过这些封装直接调用内核。

user/src/syscall.rs：
1. 维护用户态 syscall number。
2. contest_runner、tcp_echo_test、ltp_runner 依赖这里的编号。

user/src/bin/contest_runner_musl.rs：
1. 负责按官方 marker 格式运行 musl 版测试。
2. 输出 OS COMP TEST GROUP START/END，方便官网评分脚本识别。

user/src/bin/contest_runner_glibc.rs：
1. 负责按官方 marker 格式运行 glibc 版测试。
2. basic 的 musl/glibc 测试内容相同，但 marker 不同，官网按 marker 计分。

user/src/bin/tcp_echo_test.rs：
1. 保留网络基础回环测试程序。
2. 后续 iperf 修复时可以作为比 iperf 更小的定位用例。

user/src/bin/ltp_runner.rs：
1. main 中保留该程序，但 initproc 不自动进入它。
2. LTP 只用于评估现有实现，不新增功能、不为了拿 LTP 分数扩功能。

scripts/educg_eval.py：
1. 记录了通过官网页面提交并抓取评测结果的辅助脚本。
2. 当前不依赖它自动刷分，手动官网评测仍是主流程。

lua-tests/*.lua：
1. 提供 lua 评分需要的脚本内容。
2. date.lua、file_io.lua、max_min.lua、random.lua、remove.lua、round_num.lua、sin30.lua、sort.lua、strings.lua 覆盖官方 lua 子项。


2026/6/16
main 上继续工作的边界

1. 当前 main 迁移目标是保留官网已验的 1244.8785790593415 分基线。
2. 后续只在 main 上修 iperf，先找根因，再修真实 socket/clone/poll 语义。
3. iperf 修完后可以用官方 sdcard 试跑 LTP，记录现有功能能拿多少分。
4. LTP 阶段只评测已有实现，不添加新的 LTP 功能，不绕过 LTP 用例，不为了 LTP 分数改测试选择。


2026/6/16
main 上 iperf 修复记录

本轮 iperf 修复仍然使用官方 sdcard：
1. RISC-V 使用 `tmp/official-20250615/sdcard-rv.img`，QEMU 命令中的第一块盘挂载为 `vda`。
2. LoongArch 使用 `tmp/official-20250615/sdcard-la.img`，QEMU 命令中的第一块 PCI virtio-blk 盘同样挂载为 `vda`。
3. 手动测试命令均先执行 `/busybox mount -t ext4 vda /mnt`，再分别进入 `/mnt/musl` 或 `/mnt/glibc` 执行官方 `iperf_testcode.sh`。
4. 没有修改官方 `iperf_testcode.sh`，没有在内核或用户态硬编码 iperf 输出，也没有跳过官方六组用例。

最终验证结果：
1. `musl-rv`：`BASIC_UDP`、`BASIC_TCP`、`PARALLEL_UDP`、`PARALLEL_TCP`、`REVERSE_UDP`、`REVERSE_TCP` 全部 success，日志为 `tmp/final-iperf-musl-rv.log`。
2. `glibc-rv`：同六组全部 success，日志为 `tmp/final-iperf-glibc-rv.log`。
3. `musl-la`：同六组全部 success，日志为 `tmp/final-iperf-musl-la.log`。
4. `glibc-la`：同六组全部 success，日志为 `tmp/main-iperf-glibc-la-statx.log`。

os/src/drivers/net/virtio_net.rs：
1. `VirtIONetAdapter::new` 不再固定使用 `0x10003000`。官方 RISC-V QEMU 命令里有两块 virtio-blk 和一块 virtio-net，实际 net 设备可能落在后面的 MMIO slot；固定地址会探测到空设备或块设备，从而导致网络未初始化。
2. 新增 `find_net_mmio`，按 `VIRTIO_NET_BASES` 扫描 `0x10001000` 到 `0x10008000`，先校验 virtio magic，再读取 device id，只接受 device id 为 1 的 virtio-net。
3. 新增 `has_mmio_net_device`，让上层能在不创建 `VirtIONetAdapter` 的情况下判断是否存在 RISC-V/AArch64 MMIO 网卡。
4. `VirtIONetAdapter::new` 通过 `find_net_mmio` 得到真实 base 和 MMIO 虚拟地址，再创建 `MmioTransport` 和 `VirtIONet`。这样 RISC-V 官方环境中 net 设备位于 `0x10008000` 时也能初始化。

os/src/net/mod.rs：
1. `imp` 模块的条件编译范围加入 `loongarch64`，让 LoongArch 也使用同一套 socket syscall 和 smoltcp socket set。
2. 增加 `NetDevice` 类型别名。RISC-V/AArch64 使用 `VirtIONetAdapter`，LoongArch 使用本文件内的 `LoopbackOnlyAdapter`。
3. 新增 `loopback::LoopbackOnlyAdapter`。它实现 smoltcp `Device` trait，只提供本地 socket 所需的设备抽象，不做 PCI 网卡寄存器级实现。官方 iperf 用的是 `127.0.0.1`，本轮先保证本机 TCP/UDP socket 语义正确。
4. `LoopbackOnlyAdapter::new` 返回固定 MAC 的本地设备；`mac_address` 提供给 `NetworkState::new` 构造 smoltcp `Interface`。
5. `LoopbackRxToken::consume` 和 `LoopbackTxToken::consume` 满足 smoltcp `Device` 的收发 token 接口。LoongArch 本地 iperf 的真实数据不走这里，而是走 `net/syscall.rs` 中的本地 TCP/UDP 队列。
6. `NetworkState::new` 改为调用 `NetDevice::new`，同一段初始化逻辑可以服务 RISC-V 的真实 virtio-net 和 LoongArch 的本地-only 设备。
7. `net_device_present` 在 RISC-V/AArch64 上继续调用 `has_mmio_net_device`，在 LoongArch 上返回 true，使本地 socket 层可用。
8. `net_available`、`net_available_fast`、`with_net_iface`、`with_net_iface_tcp_connect`、`wake_net_blocked_tasks` 的条件编译同步加入 LoongArch。

os/src/net/socket.rs：
1. `SocketFile` 增加 `ref_count`，让 `dup`、`fork`、`accept` 后的 socket fd 能区分“fd 引用减少”和“最后一个引用关闭”。
2. `SocketFile` 增加 `local_tcp`，记录本地 TCP 连接 id 和当前 fd 属于服务端还是客户端。
3. `SocketFile::read` 对 TCP 先检查 `local_tcp`，本地连接走 `local_tcp_recv`；否则走原来的 smoltcp `tcp_recv`。UDP 读改为调用 `udp_recv_with_endpoints`，把本 fd 的 local endpoint 和 peer endpoint 一起传下去。
4. `SocketFile::write` 对 TCP 先走 `local_tcp_send`；UDP 写改为调用 `udp_write_with_endpoint`，支持 connected UDP 的 `write()` 和 `sendmsg()`。
5. `SocketFile::close_ref` 在最后一个 fd 引用关闭时，分别清理 `local_tcp_close`、`local_udp_close` 或 `local_tcp_unlisten`。这个修复避免前一组 UDP/TCP 测试残留队列污染后一组测试。
6. `SocketFile::poll` 对本地 TCP 连接调用 `local_tcp_poll`，监听 socket 调用 `local_tcp_pending_on` 判断 accept 是否可读。UDP poll 同时检查 smoltcp socket 和 `local_udp_pending_on`。
7. `SocketFile::poll` 对监听 TCP socket 不再把 `!may_recv()` 直接当成可读，否则 `ppoll/pselect6` 会把 listen fd 误判成普通连接可读。

os/src/net/syscall.rs：
1. 新增 `LocalUdpPacket`，记录 UDP packet 的目标端口、源 endpoint 和 payload。之前只按端口取包，多路并行 UDP 会互相取错包。
2. 新增 `LOCAL_UDP_QUEUE` 和 `LOCAL_UDP_QUEUE_LIMIT`，用于 127.0.0.1 UDP 的内核内投递。
3. `local_udp_packet_matches` 用目标端口和可选 peer endpoint 过滤 packet。connected UDP 只接收对应 peer 的包，修复 `PARALLEL_UDP` 的竞态。
4. `enqueue_local_udp` 入队本地 UDP packet，队列满时丢最旧包并主动让出 CPU，避免发送端长时间独占执行。
5. `try_send_local_udp` 在目标地址是 `127.x.x.x`、`0.0.0.0` 或 `10.0.2.15` 时走本地投递，不依赖真实网卡。
6. `local_udp_pending_on` 给 `SocketFile::poll` 判断 UDP fd 是否可读。
7. `local_udp_close` 在 UDP socket 最后一个 fd 引用关闭时清理匹配 local/peer 的残留包，避免 `BASIC_UDP` 的旧包影响 `PARALLEL_UDP` 和 `REVERSE_UDP`。
8. `try_recv_local_udp` 用 socket fd 找到 local endpoint，再按 peer 过滤队列，同时在 `recvfrom` 需要时写回源地址。
9. `try_recv_local_udp_by_endpoint` 给 `SocketFile::read` 和 connected UDP 使用，不依赖当前 syscall 的 fd 参数。
10. 新增 `LocalTcpConn`，用 `c2s` 和 `s2c` 两个 `VecDeque<u8>` 保存本地 TCP 两个方向的数据流，同时记录 client/server endpoint 和关闭状态。
11. `create_local_tcp` 在连接目标端口已经 listen 时创建本地连接，分配 ephemeral port，写入客户端 socket 的 local/peer endpoint，并把连接 id 放入 `LOCAL_TCP_PENDING`。
12. `accept_local_tcp` 从 `LOCAL_TCP_PENDING` 取出连接，创建新的 `SocketFile`，设置服务端 local/peer endpoint 和 `local_tcp` 标记，并写回 `accept/accept4` 的 peer 地址。
13. `local_tcp_send` 写入对应方向队列，队列满时阻塞/yield；peer 已关闭时返回 EPIPE 语义。
14. `local_tcp_recv` 从对应方向队列读取，队列空且 peer 未关闭时按阻塞/非阻塞语义处理。
15. `local_tcp_close` 标记某一端关闭，并在两端都关闭且队列清空后移除连接。
16. `local_tcp_unlisten` 在监听 socket 最后关闭时移除监听端口和 pending 队列。
17. `local_tcp_poll` 返回本地连接的可读、可写、错误状态，支撑 iperf3 的 `select/poll` 等待。
18. `local_tcp_pending_on` 给监听 socket 判断 accept 队列是否非空。
19. `local_tcp_peer_endpoint` 给 `getpeername` 返回本地 TCP 对端地址。
20. `sys_socket` 仍创建真实 `SocketFile` 和 smoltcp handle，没有硬编码 iperf 结果。
21. `sys_bind` 对 UDP 总是记录 local endpoint。即使 smoltcp bind 因端口复用限制失败，本地 UDP 队列仍能按 local/peer 语义工作，支撑 iperf 并行 UDP。
22. `sys_listen` 标记 TCP 监听端口，`sys_connect` 对本地 TCP 优先调用 `create_local_tcp`。
23. `sys_accept` 和新增 `sys_accept4` 都先尝试 `accept_local_tcp`，没有 pending 连接时再走原 smoltcp 路径。
24. `sys_sendto`、`sys_recvfrom`、`sys_sendmsg`、`sys_recvmsg` 对本地 TCP/UDP 分别走本地队列；非本地地址保留原 smoltcp 路径。
25. `udp_write_with_endpoint` 修复 connected UDP 的 `write()` 路径，`REVERSE_UDP` 中 server 端通过 connected UDP 回发数据依赖这个函数。
26. `udp_recv_with_endpoints` 修复 connected UDP 的 `read()` 路径，保证只读当前 peer 的包。
27. `sys_getsockname`、`sys_getpeername` 能从 `SocketFile` 记录的 local/peer endpoint 返回本地 TCP/UDP 地址。
28. `sys_getsockopt` 增加 `SO_ERROR`、`TCP_MAXSEG`、`TCP_CONGESTION`、`TCP_INFO` 的兼容返回。glibc/musl 版 iperf3 会读取这些选项决定连接状态和输出信息。
29. `sys_sock_shutdown` 对本地 TCP 调用 `local_tcp_close`，使 reverse TCP 结束时读端能看到 EOF。

os/src/syscall/mod.rs：
1. 新增 `SYSCALL_ACCEPT4 = 242`。
2. syscall 分发中把 `accept4` 接到 `sys_accept4`，LTP 和 glibc 网络程序会直接调用该入口。

os/src/syscall/process.rs：
1. `sys_futex` 增加最小 WAIT/WAKE 支持。WAIT 时检查用户态 futex word 是否仍等于期望值，匹配才把当前任务放入 futex 阻塞状态；WAKE 时按地址唤醒最多指定数量任务。
2. 新增 `wake_futex_tasks`，只唤醒 `BlockReason::Futex(addr)` 匹配的任务，避免把 socket/pipe 阻塞任务误唤醒。
3. `sys_pselect6` 保持已有 fd poll 语义，清理调试输出后用于 iperf3 等待 socket 可读/可写。

os/src/task/task.rs：
1. `BlockReason` 增加 `Futex(usize)`，记录 futex wait 的用户地址。

os/src/task/mod.rs：
1. `wake_net_blocked_tasks` 明确不唤醒 `BlockReason::Futex(_)`，避免网络 poll 轮询误扰 futex wait。
2. `check_pending_signals` 遇到未知 signal bit 时跳过，不再对 `SignalFlags::from_bits` 结果直接 unwrap，避免 LTP/复杂程序触发异常 signal bit 时 panic。

os/src/fs/stdio.rs：
1. 新增 `char_device_stat`，统一构造字符设备 `Kstat`。
2. `Urandom::stat` 返回 `S_IFCHR | 0444` 和 `st_rdev = 0x0109`。
3. `DevNull::stat` 返回 `S_IFCHR | 0666` 和 `st_rdev = 0x0103`。
4. `DevZero::stat` 返回 `S_IFCHR | 0666` 和 `st_rdev = 0x0105`。
5. 这个修复解决 glibc 版 iperf3 `daemon(0, 0)` 对 `/dev/null` fd 做 `fstat/statx` 后判定设备号为 0，从而报 “unable to become a daemon: No such device” 的问题。

os/src/syscall/fs.rs：
1. `write_statx` 新增 `dev_major` 和 `dev_minor`，按 Linux dev_t 编码把 `Kstat.st_rdev` 和 `Kstat.st_dev` 写成 `statx.stx_rdev_major/minor` 与 `statx.stx_dev_major/minor`。
2. RISC-V glibc daemon 主要走 `fstatat(AT_EMPTY_PATH)`，修 `DevNull::stat` 后已通过；LoongArch glibc daemon 会走 `statx(AT_EMPTY_PATH)`，还需要 `write_statx` 写出 rdev major/minor 才能通过。

本轮 iperf 根因总结：
1. RISC-V 一开始的问题是 virtio-net 探测固定地址错误，官方 QEMU 下 net 设备不一定在 `0x10003000`。
2. LoongArch 一开始的问题是网络模块没有启用，且不应该为了 iperf 去做难维护的 PCI 网卡寄存器级修补；官方 iperf 使用 `127.0.0.1`，因此先实现本地 socket/loopback 语义。
3. UDP 并行和 reverse 的问题是 connected UDP 没有按 local/peer endpoint 过滤队列，导致不同 stream 互相消费 packet。
4. TCP 并行和 reverse 的问题是没有本地 TCP accept 队列和双向字节流，listen fd 的 poll 状态也会被误判。
5. glibc 版 iperf 的问题是 `daemon()` 对 `/dev/null` fd 做设备校验，fd/statx 返回的字符设备号不完整。


2026/6/16
LTP 官方 sdcard 探测记录

本轮 LTP 只做探测，不新增 LTP 功能：
1. 没有修改 `user/src/bin/ltp_runner.rs` 的测试列表。
2. 没有向内核添加新的 LTP 专用 syscall。
3. 没有删减官方 sdcard 里的 LTP 用例。
4. 没有为了让 LTP 分数变高而更改官方脚本或用例输出。

官方 sdcard 中 LTP 内容：
1. `tmp/official-20250615/sdcard-rv.img` 的 `/musl/ltp` 和 `/glibc/ltp` 都存在。
2. `tmp/official-20250615/sdcard-la.img` 的 `/musl/ltp` 和 `/glibc/ltp` 也都存在。
3. `/musl/ltp/testcases/bin` 和 `/glibc/ltp/testcases/bin` 下是大量 LTP 二进制和脚本。
4. 官方 sdcard 根目录没有 `/ltp`。main 分支 rootfs 中的 `ltp_runner` 默认路径是 `/ltp/testcases/bin/`，因此直接运行 rootfs 的 `ltp_runner` 不能代表官方 sdcard 上的 LTP。

本次探测方式：
1. 使用最终 RISC-V 内核和官方 `sdcard-rv.img`。
2. QEMU 启动后执行 `/busybox mount -t ext4 vda /mnt`。
3. 进入 `/mnt/musl` 后按 `testsuits-for-oskernel/scripts/ltp/ltp_testcode.sh` 的逻辑遍历 `ltp/testcases/bin/*`。
4. 外层 QEMU 使用 240 秒总超时，因为官方全量 LTP 没有单用例超时保护，某个用例卡住会阻塞后续用例。

探测结果：
1. 日志文件：`tmp/probe-ltp-musl-rv.log`。
2. 外层状态码：124，表示 240 秒超时终止。
3. 已看到 `#### OS COMP TEST GROUP START ltp ####`，没有看到 `#### OS COMP TEST GROUP END ltp ####`，因此这不是完整官网评分结果。
4. 已开始运行的 case 数：4。
5. 统计到的 `TPASS` 行数：4。
6. 统计到的 `TFAIL` 行数：2。
7. 统计到的 `TBROK` 行数：0。
8. 统计到的 `TCONF` 行数：0。
9. `abort01` 中触发 `SIGSEGV`，LTP 输出 `abort() failed to dump core` 和 `abort() raised SIGSEGV` 两个 `TFAIL`。
10. `abs01` 输出 3 个 `TPASS`。
11. `accept01` 至少输出 1 个 `TPASS`：`bad file descriptor : EBADF (9)`，随后没有在 240 秒窗口内继续完成，外层 timeout 杀掉 QEMU。

LTP 结论：
1. 当前 main 的 LTP 不能给出完整分数；本次只确认官方 sdcard 的 LTP 目录可见、部分 LTP 二进制能从 `/mnt/musl` 执行。
2. 现有实现至少能支撑 `abs01` 的简单 libc 测试和 `accept01` 的第一段 EBADF 检查。
3. `abort01` 暴露了 core dump / abort signal 语义不足，但本轮按要求不新增 LTP 功能。
4. `accept01` 后续卡住需要进一步分析 socket accept 的阻塞/超时/信号交互；本轮不为 LTP 分数新增功能。
