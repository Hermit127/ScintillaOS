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

MERGE不要用AI!!!  请手动检查每一行冲突!!!
MERGE不要用AI!!!  请手动检查每一行冲突!!!
MERGE不要用AI!!!  请手动检查每一行冲突!!!



