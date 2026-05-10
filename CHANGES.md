# 大赛提交修改记录

完整技术文档见 [ADAPTATION.md](./ADAPTATION.md)

## 修改文件

| 文件 | 操作 | 内容 |
|------|------|------|
| `Makefile` | 修改 | all/kernel-rv/kernel-la/disk.img/run-dual 目标 |
| `os/src/drivers/block/virtio_blk.rs` | 修改 | VIRTIO0/VIRTIO1 覆盖 riscv64+loongarch64，`new_at()` 方法 |
| `os/src/drivers/block/mod.rs` | 修改 | loongarch64 改用 VirtIOBlock（替代 RamDiskBlock）+ ROOT_BLOCK_DEVICE |
| `os/src/drivers/mod.rs` | 修改 | 导出 ROOT_BLOCK_DEVICE |
| `os/src/fs/vfs.rs` | 修改 | ROOT_SB 改用 bus1 + mount_test_disk_at() |
| `os/src/syscall/mod.rs` | 修改 | SYSCALL_SHUTDOWN = 48 |
| `os/src/syscall/process.rs` | 修改 | sys_shutdown() |
| `os/src/task/mod.rs` | 修改 | INITPROC → contest_runner |
| `user/src/syscall.rs` | 修改 | sys_shutdown() |
| `user/src/lib.rs` | 修改 | pub fn shutdown() |
| `user/src/bin/contest_runner.rs` | 新建 | 测试脚本运行器 |
