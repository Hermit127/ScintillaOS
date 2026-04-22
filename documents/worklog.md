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

