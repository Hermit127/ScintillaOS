2026/04/20 Hermit
添加fstat功能(不成熟)：
为文件生成Kstat结构体并返回该结构体到stat_buf地址

其中,Kstat的部分参数是默认值，没有填入真实值
目前没有做stat_buf的指针安全性检查

2026/04/22 LYZ
ext4 文件系统迁移（两阶段方案一次实现到读写可用）：
1. 块设备层解耦：新增内核自有 `BlockDevice` trait，`virtio_blk` 与 `ram_blk` 改为实现内核 trait，不再依赖 easyfs。
2. 文件系统后端切换：`os/src/fs/inode.rs` 重写为 ext4 后端，接入 `ext4_rs`，实现 `open/read/write/create/trunc/fstat`，并通过统一 `OSInode` 接口对接 syscall。
3. 路径语义调整：支持绝对路径，兼容无前缀命令名解析；`INITPROC` 改为从 `/bin/user_shell` 启动。
4. 镜像构建链路替换：`Makefile` 从 `easyfs-packer` 切换到 `mke2fs` 生成 ext4 镜像，自动将用户程序拷贝到 `/bin`，并关闭 journal。
5. 额外修复：修复 `os/src/syscall/fs.rs` 末尾缺失右花括号导致的编译失败。

功能效果：
- 系统可成功加载 ext4 镜像并读取 `/bin` 下应用；
- 启动时可列出应用列表；
- 内核可正常拉起 `/bin/user_shell`，`make run` 启动链路通过。

已知问题：
- 当前工程仍有若干历史 warning（未在本次改动中处理）；
- ext4 适配层为扇区级读改写实现，功能可用但性能未做优化。

详细排错记录见：
`documents/ext4_migration_detailed_log_2026-04-22.md`

2026/04/22 Duyuheng
添加brk功能与测试文件，调用负责动态调整进程的堆顶指针（program_brk），通过映射或解除映射物理页来扩展或收缩进程的数据段。

2026/04/22 LYZ
合并与运行问题修复：
1. 处理 `git pull` 后的冲突文件，清理 `documents/worklog.md`、`os/src/fs/inode.rs`、`os/src/syscall/fs.rs` 中的合并标记。
2. 修复 ext4 后端目录遍历实现，去掉 `OSInodeInner` 中不存在的 `inode` 字段访问，改为基于 `inode_num` 读取目录项。
3. 简化 `sys_sleep` 实现，移除当前 `TaskControlBlockInner` 中不存在的 `sleeping` 状态记录，保留等待循环。
4. 修正 `getdents64` 目录项名称类型转换问题，确保目录项写入缓冲区时使用字节切片。

验证结果：
- `cargo check -p os --target riscv64gc-unknown-none-elf` 已通过，仅保留既有 warning。


2026/04/23 LYZ
ext4 体系下文件调用适配（openat/read/write/close/fd目录语义）
- syscall 参数通道从 3 参数扩展到 6 参数，支持 `openat(dirfd, path, flags, mode)` 标准调用形态。
- `SYS_openat(56)` 作为统一入口，`open` 实现为 `openat(AT_FDCWD, ...)` 语法糖。
- 新增 `dirfd` 目录 fd 处理：支持 `AT_FDCWD` 与“目录 fd + 相对路径”解析。
- `OpenFlags` 对齐 Linux 关键位：`O_CREATE(0x40) / O_TRUNC(0x200) / O_DIRECTORY(0x10000)`，并兼容旧 `TRUNC` 位。
- ext4 inode 封装补充目录属性与路径元信息，供 openat 相对解析与 getdents 使用。
- 用户态 syscall 封装同步升级为 6 参数寄存器调用，新增 `openat` 封装。

效果
- 现有 ext4 启动链路正常，系统可进入 shell；
- 文件 fd 与管道 fd 仍统一走 fd 表；
- close/read/write 行为保持最小语义（成功返回字节数/0，错误返回 -1）。

兼容性提醒
- 当前按 Linux 语义解释 `0x200` 为 `O_TRUNC`；若存在旧二进制将 `0x200` 当 CREATE，行为会不一致，需重编译用户程序。

已知兼容性提醒（保留）：
- 当前仍以 `-1` 作为通用错误返回，未细分完整 errno；
- `AT_FDCWD` 目前按根目录语义处理，未实现独立 cwd 状态；
- 如存在旧二进制将 `0x200` 当 CREATE，需要重编译用户程序以匹配当前语义。
