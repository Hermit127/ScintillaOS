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

这里真正的评测用例不是我们自己塞进 rootfs 的固定输出，而是在官方提供的 sdcard/test disk 里。
内核启动后有两块 virtio-blk：
第一块是启动/rootfs 盘；
第二块是官方测试盘，里面包含 /musl、/glibc 目录以及 basic、busybox、lmbench、lua、iozone、iperf、ltp 等 testcode.sh 和对应二进制。

这件事后来通过 initproc 的 mount 日志和 getdents 日志确认：
mount vda -> /mnt 成功后，/mnt/musl/ 和 /mnt/glibc/ 能看到官方脚本。
官网卡在 “scan known /mnt/glibc/” 或 “/mnt/musl/ has N known test scripts” 时，本质是我们已经找到官方 sdcard，但执行脚本、动态链接、文件系统读写或进程调度中某一步卡住。

因此这段时间的正确修复方向是：
1. 让内核能稳定挂载官方第二块盘。
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
2. 在 RISC-V 上扫描多个 MMIO virtio 地址，官网命令里第一个 virtio-blk 是 sdcard，第二个 virtio-blk 是 disk/test disk。
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
