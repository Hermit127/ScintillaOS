# 大赛提交适配 — 完整修改说明

## 项目目标

将 `oskernel2026test` 项目适配为符合 OS 大赛提交规范的格式。评测机会执行 `make all`，生成 `kernel-rv`（RISC-V）、`kernel-la`（LoongArch）两个 ELF 内核文件，以及可选的 `disk.img` 磁盘镜像。

## QEMU 磁盘架构

```
总线 0 (0x10001000):  评测机提供的测试磁盘（含 *_testcode.sh + 测试二进制）
总线 1 (0x10002000):  你的 disk.img（含操作系统文件 + contest_runner）
```

---

## 修改清单（共 11 个文件）

### 1. Makefile

**路径**: `Makefile`

**改动**: 
- 将默认 `ARCH` 从硬编码 `riscv64` 改为 `?= riscv64`（可被命令行覆盖）
- 添加 `VIRTIO_BLK_BUS` 和 `VIRTIO_BLK_DEVICE` 变量，统一处理不同架构的 QEMU 参数
- 添加大赛提交目标：
  - `all`: 构建 kernel-rv、kernel-la、disk.img
  - `kernel-rv`: 编译 riscv64 内核 → 输出 `kernel-rv`（ELF）
  - `kernel-la`: 编译 loongarch64 内核 → 输出 `kernel-la`（ELF）
  - `disk.img`: 拷贝 fs.img 为 disk.img
- 添加 `run-dual` 本地测试目标（双磁盘：bus0=测试盘, bus1=disk.img）
- 将原有的磁盘参数抽出为 `QEMU_ARGS_SINGLE`（原 run）和 `QEMU_ARGS_DUAL`（新增）
- `TEST_DISK` 变量可指定测试磁盘路径

### 2. os/src/drivers/block/virtio_blk.rs

**改动**:
- 添加总线 1 地址常量:
  ```rust
  const VIRTIO1: PhysAddr = polyhal::pa!(0x10002000);
  ```
- 将 `VirtIOBlock::new()` 拆分为默认构造函数（使用 VIRTIO0）和 `new_at(addr)`（使用指定地址），使块设备可配置

### 3. os/src/drivers/block/mod.rs

**改动**:
- 添加 `ROOT_BLOCK_DEVICE` 静态变量（总线 1，作为根文件系统）:
  ```rust
  pub static ref ROOT_BLOCK_DEVICE: Arc<dyn BlockDevice> = Arc::new(VirtIOBlock::new_at(0x10002000));
  ```
- 保留 `BLOCK_DEVICE`（总线 0，作为测试磁盘）
- loongarch64 也添加对应的 `ROOT_BLOCK_DEVICE`（改用 VirtIOBlock）

### 4. os/src/drivers/mod.rs

**改动**: 导出 `ROOT_BLOCK_DEVICE`:
```rust
pub use block::{BlockDevice, BLOCK_DEVICE, ROOT_BLOCK_DEVICE};
```

### 5. os/src/fs/vfs.rs

**改动**:
- `ROOT_SB` 改用 `ROOT_BLOCK_DEVICE`（总线 1）创建根文件系统超级块
- 添加 `mount_test_disk_at(mnt_point)` 函数，将总线 0 的测试磁盘挂载到指定挂载点:
  ```rust
  pub fn mount_test_disk_at(mnt_point: Arc<Dentry>) -> bool {
      let test_sb = VfsSuperBlock::new_ext4(BLOCK_DEVICE.clone(), "ext4");
      vfs_mount(mnt_point, test_sb)
  }
  ```

### 6. os/src/syscall/mod.rs

**改动**:
- 添加 `SYSCALL_SHUTDOWN = 48`
- 在 syscall 调度中添加该调用号的处理: `SYSCALL_SHUTDOWN => sys_shutdown()`

### 7. os/src/syscall/process.rs

**改动**: 添加 `sys_shutdown()` 函数:
```rust
pub fn sys_shutdown() -> isize {
    polyhal::instruction::shutdown();
    0
}
```

### 8. os/src/task/mod.rs

**改动**: INITPROC 的 init 二进制从 `user_shell` 改为 `contest_runner`:
```rust
let inode = open_file("/bin/contest_runner", OpenFlags::RDONLY).unwrap();
```

### 9. user/src/syscall.rs

**改动**:
- 添加 `SYSCALL_SHUTDOWN = 48` 常量
- 添加 `sys_shutdown()` 封装函数

### 10. user/src/lib.rs

**改动**: 添加 `pub fn shutdown()` 公共接口:
```rust
pub fn shutdown() -> isize {
    sys_shutdown()
}
```

### 11. user/src/bin/contest_runner.rs（新建）

**功能**: 大赛测试脚本运行器，替代原 `user_shell` 作为 init 进程。

**执行流程**:
1. 调用 `sys_mount` 将总线 0 的测试磁盘挂载到 `/mnt`
2. 切换当前目录到 `/mnt`
3. 遍历预定义的 `*_testcode.sh` 脚本列表
4. 对每个脚本逐行解析执行：
   - `./busybox echo "TEXT"` → 拦截并直接用 `println!` 输出（无需真实 busybox）
   - `cd <DIR>` → 调用 `chdir` 改变当前工作目录
   - `./<NAME>.sh` → 递归读取该 shell 脚本（如 run-all.sh），解析测试列表并逐个执行
   - `./<二进制名>` → `fork()` + `exec()` + `waitpid()` 执行测试二进制
5. 所有脚本执行完毕后调用 `shutdown()` 关机

**解析 run-all.sh 的特殊逻辑**:
```sh
tests="          # 检测到 tests= 进入列表收集模式
brk
chdir
clone
...
"
for i in $tests   # 遇到 for 循环标记退出收集
```
将收集到的测试名逐行 `fork+exec` 执行。

---

## 构建与使用

```bash
# 一键构建大赛所需文件
make all

# 产物
#   - kernel-rv     (RISC-V 内核 ELF，约 772K)
#   - kernel-la     (LoongArch 内核 ELF)
#   - disk.img      (根文件系统镜像，128M，含 /bin/contest_runner 和 /mnt/)

# 本地双磁盘测试
make run-dual TEST_DISK=/path/to/test_disk.img
```

## 待完成

- **busybox** 用例可能在更复杂的测试脚本中需要，当前通过拦截 `busybox echo` 规避。

## 测试注意事项

- 当 `TEST=1` 传递给 `make` 时，`user/Makefile` 会将 initproc 替换为 `usertests`（用于本地调试）。大赛场景不应使用此选项。
- `run-dual` 需要提供 `TEST_DISK` 变量指向评测磁盘镜像，默认为 `fs-img.img`。
- 内核在启动时尝试读取总线 1 的 `disk.img` 作为根文件系统。确保 `disk.img` 包含完整的 ext4 文件系统（含 `/bin/contest_runner`）。
