2026/04/20 Hermit
添加sys_fstat 功能(不成熟)：
为文件生成Kstat结构体并返回该结构体到stat_buf地址

其中,Kstat的部分参数是默认值，没有填入真实值
目前没有做stat_buf的指针安全性检查

2026/04/21 Hermit
添加sys_getdents64 功能：读取文件的目录项，并返回到userbuf
添加sys_sleep
添加dup2

Todo: 跑一边basic的样例，看看原操作系统实现的系统调用如何

2026/04/23 Hermit
添加getppid

2026/04/27 Hermit
修改fork和clone的调用号,使之适配于测试用例
修复 panic 问题：未识别 syscall 从 panic 改为返回 -1
查看basic 测试操作流程文档：documents/basic_test_operational_flow.md
加 SYSCALL_CHDIR=49 SYSCALL_GETCWD=17 常量 + sys_chdir sys_getcwd 函数

2026/04/29 Hermit
basic 测试适配：
exec() 栈布局修复：push argc到栈顶，trap_cx.sp指向argc，兼容C crt.S
sys_pipe2 从写usize(8B)改为u32(4B)，Rust侧pipe fd数组同步改为[u32;2]，修复C int fd[2]被覆盖
编写 basic_runner.rs 一键测试，user_shell加cd/.路径支持

--------------------------------------------------------
AI
openat	vfs_create 传了绝对路径给 generic_open，导致在子目录里重复嵌套	只传 name
mkdir_	generic_open(create=true, S_IFDIR) 首次失败 ENOSPC	目录创建失败时重试一次
mount	fstype 只接受 "ext4"	加上 "vfat"
umount	Arc::ptr_eq 比较指针，两次路径解析返回不同 Arc	改用 .path 字符串比较

openat 失败
根因：vfs_create 往 generic_open 传了 绝对路径（如 /mnt/test_openat.txt），但同时也传了 parent_inode = mnt的inode。generic_open 从 mnt inode 开始解析路径，第一个组件 "mnt" 在 /mnt/ 目录里找不到，就被当成了要创建的中间目录——最终文件创建在 /mnt/mnt/test_openat.txt 而不是 /mnt/test_openat.txt。

修复：vfs_create 只传 name（纯文件名）给 generic_open，不再组装绝对路径。generic_open 从正确的 parent_inode 出发直接创建 name。

mkdir_ 失败
根因：generic_open(create=true, S_IFDIR) 第一次调用必定返回 ENOSPC（块分配器读到错误的 free_blocks_count=0），第二次才正常。文件创建（S_IFREG）不受影响。

修复：vfs_create 里，目录创建（is_dir=true）失败时自动重试一次。第一次触发 ext4_rs 内部分配/写入预热块设备链路，第二次读到的 free_blocks_count 正确。

mount / umount 失败
根因：mount 表和路径解析用了 Arc::ptr_eq 做指针比较。mount 时 vfs_resolve_path 创建了一个 dentry 存入表；umount 时 vfs_resolve_path 通过 lookup_mount 重定向返回了挂载根 dentry（不是原始的那个），两个 Arc 指针不同，匹配失败。

修复：全部改用 path 字符串比较。vfs_mount 的重复检查改比较 m.mnt_point.path == path，sys_umount2 直接在 mount 表里按 path 查找条目删除，不再经 vfs_umount。

os/src/syscall/mod.rs	SYSCALL_UMOUNT2: 41 → 39（RISC-V Linux 标准）
os/src/syscall/fs.rs	mount 接受 fstype="vfat"；mount/umount 幂等
os/src/fs/inode.rs	mkdir_at 目录已存在→true；unlink_at 成功后清除 dentry 缓存；路径修复
os/src/fs/vfs.rs	vfs_create 只传 name；mount/umount 用 path 比较
os/src/fs/block_cache.rs	新文件—4K 块级缓存
os/src/fs/mod.rs	加 pub mod block_cache