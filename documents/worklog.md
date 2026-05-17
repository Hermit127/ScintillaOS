2026/04/22 LYZ
把文件系统从easyfs迁移到ext4以适配basic

已知问题：
- ext4 适配层为扇区级读改写实现，功能可用但性能未做优化。
详细排错记录见：
documents/ext4_migration_detailed_log_2026-04-22.md



2026/04/22 Duyuheng
添加brk功能与测试文件，调用负责动态调整进程的堆顶指针（program_brk），通过映射或解除映射物理页来扩展或收缩进程的数据段。



2026/04/23 LYZ
ext4 体系下文件调用适配（openat/read/write/close/fd目录语义）
syscall封装从3参数改为6参数寄存器调用，新增openat调用

已知兼容性问题：
- 当前仍以 `-1` 作为通用错误返回，未细分完整 errno；
- `AT_FDCWD` 目前按根目录语义处理，未实现独立 cwd 状态；
- 当前按 Linux 语义解释 `0x200` 为 `O_TRUNC`；若存在旧二进制将 `0x200` 当 CREATE，行为会不一致，需重编译用户程序。



2026/04/23 LYZ
跑通cargo check -p os --target riscv64gc-unknown-none-elf



2026/04/23 Duyuheng
添加了mmap系统调用，实现了函数功能，但是没有添加到系统调用里。
mmap内部实现：
1.参数与权限处理：校验地址对齐，并将用户态权限转换为内核格式。
2，虚拟地址空间检查：计算目标虚拟页号范围，确保与现有内存段无重叠。
3.建立映射与加载：根据标志建立匿名或文件映射，分配物理页并拷贝数据。
syscall需要扩展参数数量



2026/04/23 Hermit
文档中实现什么功能能否尽量手动写，ai写的一堆废话
实现的功能细节可以交给ai

2026/04/24 LYZ
实现了dup与dup2系统调用

2026/04/24 LYZ
修复ext4的目录识别问题
修复getdents

2026/04/24 LYZ
完成getcwd unlink chdir调用
mkdir依然存在问题

2026/04/24 dyh
恢复了mmap的系统调用
实现了munmap的系统调用，解除地址映射，为mmap的逆过程。

2026/04/25 dyh
实现了times的系统调用，测试文件已添加。

2026/4/26 dyh
实现了uname的最小实现。

2026/4/28 dyh
实现了nanosleep，未测试
完善了nanosleep，写了测试文件，暂未接入到sleep调用

2026/4/28 stq
目前可以使用basic_runner进行测试了，brk和chdir通过了测试，但是clone卡死了
clone已经修复，mkdir与mount测试失败，pipe卡死，pipe之前的全部测试成功

MERGE不要用AI!!!  请手动检查每一行冲突!!!
MERGE不要用AI!!!  请手动检查每一行冲突!!!
MERGE不要用AI!!!  请手动检查每一行冲突!!!

2026/4/29 stq
pipe usize格式修改4字节格式(int)，以适配测试用例

2026/4/29
实现了vfs和dentry缓存块，完成mount和umount的实现，其余和文件有关的调用未测试，可能出现bug
目前仍旧缺少缓存回收机制。

2026/5/7
ext4_rs 库修改!
文件：ext4_rs-1.3.3/src/ext4_impls/balloc.rs
改动：bgid = 1 → bgid = 0
原因：balloc_alloc_block 中搜索空闲块的起始块组号硬编码为 1。单块组文件系统从组 1 开始读到不存在的块组描述符→ free_blocks_count = 0 → 直接 ENOSPC，从未检查组 0。

同步方式：fork ext4_rs 到你的 GitHub，改一行代码，把 os/Cargo.toml 中的 ext4_rs = "1.3.3" 改成：

ext4_rs = { git = "https://github.com/你的账号/ext4_rs", rev = "你的commit" }


2026/5/12 hermit
更新用户系统，新增getgid，setgid等多项指令(具体功能为查询用户)
修复mmap和getppid中的bug，getppid为调用号问题，需要重新编译测试，mmap为RISC-V 静态二进制代码段常从 0x0 加载，修复方式：遇 NULL 时遍历 areas 找第一段空闲虚地址区间，而非直接用 0

2026/5/12 hermit
新增lseek功能(查找文件指针)

修复了lseek和之前用户系统与文件的bug
原因：我之前设 uid/gid 的做法是"事后补刀"——ext4_rs 先把 inode 创建好写盘，然后我再单独读回来改 uid/gid 再写盘。这导致 inode 磁盘状态和 ext4_rs 内部缓存不一致。后续写文件内容时，write_at 读到不一致的 inode，计算出错误的块分配量，申请了一块巨大到不可能的内存 → 崩溃。

修复：改成"一步到位"——用 ext4_rs 原生的 create_with_attr，创建 inode 的同时就把 uid/gid 写进去。

以下的代码我都手动看过了，应该没有问题
新增clock_gettime功能，通过测试 (获取系统时间)
新增fstatat功能，通过测试 (根据路径 / 文件描述符，获取文件元数据（大小、类型、权限、时间等）)
新增fcntl功能，通过测试 (对文件fd进行一系列操作，具体为复制，获取/修改fd标志，获取状态标志) (目前仍是最小实现！)
新增ioctl功能，通过测试(ioctl功能是一个非常复杂的功能，我们只实现了其中的两个，读 termios和窗口大小,用于通过busybox,后续需要逐步实现ioctl)
修改vfs，新增symlinkat功能，通过测试(可以创建文件的快捷方式)
新增readlinkat功能，通过测试(可以读取快捷方式指向谁)
新增rename功能，通过测试(重命名，具体实现为调用ext4的接口，更改父目录的目录项列表)
新增faccessat功能，通过测试(检查文件是否存在、是否可读/可写/可执行)

2026/5/13 hermit
新增utimensat功能，通过测试(修改文件的时间，如上次修改时间等)
新增umask功能，通过测试(修改文件的权限)
新增最小化mprotect，通过测试(修改一段已存在内存区域的访问权限，后续需要添加更完整的mprotect)
把busybox需要stub的系统调用全部添加了stub形式
修改了网络架构，使得没有网卡也能通过编译


2026/5/12 dyh
建立iperf所需网络系统底层架构

2026/5/13 dyh
建立sockettable
新增socket系列系统调用

2026/5/14 dyh
完成Phase 2 数据收发系统调用：
- 新增 sendmsg(211)/recvmsg(212)：支持 iovec scatter/gather，iperf3 主 I/O 路径
- 新增 sendto(206)/recvfrom(207)：UDP 数据收发
- 新增 setsockopt(208)/getsockopt(209)：TCP_NODELAY、SO_KEEPALIVE、SO_REUSEADDR 等
- 新增 shutdown(210)/getpeername(205)/getsockname(204)
- 修改 sys_read/sys_write 支持 socket fd（先查 socket_table 回退 fd_table）
- 阻塞 I/O 使用 busy-poll + suspend_current_and_run_next
- 为非网络架构(loongarch64/x86_64)添加 stub 函数
- riscv64/loongarch64/x86_64 三架构 cargo check 通过
实现socket的I/O阻塞

2026/5/17 dyh
把socket的表写入fd_table统一
iperf3目前依旧无法运行