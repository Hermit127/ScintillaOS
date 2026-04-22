# rCore Baseline -> ext4 迁移详细变更记录

记录日期：2026-04-22  
记录人：Codex  
目标：把当前 baseline 从 `easyfs` 迁移到 `ext4`，并完成可启动、可读写的基础能力。

---

## 1. 变更总览

本次变更涉及以下核心方向：
1. 块设备接口从 `easyfs::BlockDevice` 解耦到内核自有 `BlockDevice`。
2. 文件系统后端从 `easyfs` 切换为 `ext4_rs`。
3. 应用路径语义从平铺名称向绝对路径 (`/bin/...`) 迁移。
4. 镜像构建链路从 `easyfs-packer` 切换为 `mke2fs` 生成 ext4。
5. 修复历史遗留编译错误（`sys_fstat` 右花括号缺失）。

当前工作区相关变更（`git status --short`）为：
- `M Makefile`
- `M documents/worklog.md`
- `M os/Cargo.toml`
- `M os/src/drivers/block/mod.rs`
- `M os/src/drivers/block/ram_blk.rs`
- `M os/src/drivers/block/virtio_blk.rs`
- `M os/src/drivers/mod.rs`
- `M os/src/fs/inode.rs`
- `M os/src/syscall/fs.rs`
- `M os/src/task/mod.rs`
- `?? os/src/drivers/block/device.rs`

---

## 2. 文件级变更明细

### 2.1 `os/src/drivers/block/device.rs`（新增）

**改动内容**
- 新增内核自有块设备 trait：
  - `read_block(&self, block_id: usize, buf: &mut [u8])`
  - `write_block(&self, block_id: usize, buf: &[u8])`

**改动目的**
- 去除对 `easyfs` trait 的耦合，为 ext4 / 其他 FS 复用统一块设备接口。

---

### 2.2 `os/src/drivers/block/mod.rs`

**改动内容**
- 引入 `mod device;`
- 删除 `use easyfs::BlockDevice;`
- 改为 `pub use device::BlockDevice;`

**改动目的**
- 将块设备统一依赖切换到内核自有 trait。

---

### 2.3 `os/src/drivers/mod.rs`

**改动内容**
- 从仅导出 `BLOCK_DEVICE` 改为同时导出 `BlockDevice` 与 `BLOCK_DEVICE`。

**改动目的**
- 让 FS 层可直接通过 `crate::drivers::{BlockDevice, BLOCK_DEVICE}` 使用统一接口。

---

### 2.4 `os/src/drivers/block/virtio_blk.rs`

**改动内容**
- `use virtio_drivers::device::blk::{SECTOR_SIZE, VirtIOBlk};`
- 在 `read_block/write_block` 中增加缓冲区长度对齐断言：`buf.len() % SECTOR_SIZE == 0`。

**改动目的**
- 保证块设备读写调用满足扇区对齐约束，避免非法长度读写。

---

### 2.5 `os/src/drivers/block/ram_blk.rs`

**改动内容**
- 删除 `use easyfs::BlockDevice;`
- 改为 `use super::BlockDevice;`

**改动目的**
- 与新的内核块设备 trait 对齐。

---

### 2.6 `os/Cargo.toml`

**改动内容**
- 移除依赖：`easyfs = "0.1"`
- 新增依赖：`ext4_rs = "1.3.3"`

**改动目的**
- 正式切换 FS 后端实现。

---

### 2.7 `os/src/fs/inode.rs`（主要重构）

**改动内容（关键）**
1. 删除原 `easyfs::{EasyFileSystem, Inode}` 相关实现。
2. 新增 `Ext4BlockDeviceAdapter`：将内核 `BlockDevice` 适配成 `ext4_rs::BlockDevice`。
3. 新增 `EXT4_FS` 全局实例（`lazy_static` + `UPSafeCell<Ext4>`）。
4. 新增路径处理函数：
   - `trim_cstr_suffix`：去除用户态传入 `\0`。
   - `normalize_candidates`：支持相对路径兼容解析（`/<name>` 和 `/bin/<name>`）。
   - `normalize_create_path`：创建时规范化路径。
   - `lookup_inode`：统一查找逻辑。
5. `OSInode` 内部状态改为 `inode_num + offset`（不再持有 easyfs inode 对象）。
6. `read/read_all/write/stat` 全部改为调用 ext4 接口：
   - `fs.read_at(...)`
   - `fs.write_at(...)`
   - `fs.get_inode_ref(...)` + inode metadata 转 `Kstat`
7. `open_file` 行为：
   - 支持 `CREATE`。
   - 支持 `TRUNC`（通过 `truncate_inode`）。
   - 支持绝对路径与命令名兼容路径。
8. `list_apps` 改为遍历 `/bin` 目录条目输出应用名。

**重要实现细节（排错关键）**
- `ext4_rs` 并非只在 4K 边界读写，它会访问例如 `offset=1024`（superblock）与 inode 子块偏移。
- 适配层最终采用“扇区级读改写（read-modify-write）”处理任意偏移，而非 4K 强对齐。
- 这是从一次启动 panic 修复得出的关键结论。

---

### 2.8 `os/src/task/mod.rs`

**改动内容**
- `INITPROC` 从 `open_file("user_shell", ...)` 改为 `open_file("/bin/user_shell", ...)`。

**改动目的**
- 对齐 ext4 目录语义，避免根目录平铺假设。

---

### 2.9 `Makefile`

**改动内容**
1. 新增变量：
   - `ROOTFS_DIR := target/$(TARGET)/$(MODE)/rootfs`
   - `ROOTFS_BIN_DIR := $(ROOTFS_DIR)/bin`
2. `fs-img` 目标替换为 ext4 镜像流程：
   - 编译 user 程序
   - 清理并创建 rootfs/bin
   - 将 `user/src/bin/*.rs` 对应可执行拷贝到 `rootfs/bin`
   - `dd` 创建空镜像
   - `mke2fs -t ext4 -b 4096 -F -d $(ROOTFS_DIR) $(FS_IMG)`
   - `tune2fs -O ^has_journal $(FS_IMG)`（关闭 journal）

**改动目的**
- 替换 easyfs 打包链路，直接构建可挂载 ext4 镜像。

---

### 2.10 `os/src/syscall/fs.rs`

**改动内容**
- 补齐 `sys_fstat` 末尾缺失的 `}`。

**改动目的**
- 修复历史遗留编译错误（与 ext4 迁移无直接逻辑耦合，但阻塞编译验证）。

---

### 2.11 `documents/worklog.md`

**改动内容**
- 新增 2026/04/22 迁移摘要记录。

**改动目的**
- 满足项目“每次新增功能必须记录”的流程要求。

---

## 3. 迁移过程中的问题与修复轨迹

### 问题 A：`cargo check` 网络拉取失败
- 现象：crates index 拉取失败（受环境网络/沙箱限制影响）。
- 处理：使用允许网络访问的执行方式重跑检查。

### 问题 B：编译报错 `unclosed delimiter`（`syscall/fs.rs`）
- 现象：`sys_fstat` 函数缺失闭合右花括号。
- 处理：补全 `}` 并重新检查。

### 问题 C：启动时 ext4 访问 panic（偏移断言）
- 现象：早期适配层假设 4K 对齐，遇到 `offset=1024` 触发断言。
- 处理：将适配层改为“任意 offset + 扇区级拼接/读改写”逻辑。
- 结果：系统可成功启动并进入 shell。

---

## 4. 验证命令与结果（本次执行）

1. `cargo check -p os --target riscv64gc-unknown-none-elf`
- 结果：通过（存在历史 warning，不影响功能）。

2. `make fs-img ARCH=riscv64`
- 结果：通过，生成 ext4 镜像成功。

3. `timeout 25s make run ARCH=riscv64 TEST=1`
- 结果：通过关键启动验证：
  - 内核成功启动；
  - `/bin` 应用枚举正常；
  - 成功进入 `Rust user shell`；
  - 命令因 `timeout` 主动结束属预期。

---

## 5. 目前已知风险点（后续排错优先级）

1. `ext4_rs` 适配层当前为扇区级读改写，实现正确性优先，性能未优化。
2. 现有工程 warning 较多，排错时需注意把 warning 与功能性错误区分开。
3. 目录/路径目前按“满足现有用户程序运行”目标实现，完整 POSIX 语义未全面覆盖。

---

## 6. 建议的后续检查清单

1. 针对 `open/create/trunc/write/readback` 做单独回归（含边界偏移、空文件、覆盖写）。
2. 增加 fs 层日志开关，定位 ext4 inode/path 解析失败点。
3. 视性能需求增加 block cache 或减少读改写次数。
4. 如需严格规范，补齐更多目录/权限/时间戳语义与错误码一致性。

