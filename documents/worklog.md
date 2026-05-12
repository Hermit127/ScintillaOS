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