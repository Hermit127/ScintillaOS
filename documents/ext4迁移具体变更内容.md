# ext4rs → lwext4 迁移详细变更记录

记录日期：2026-05-19  
记录人：Codex  
目标：将 OS 内核 ext4 后端从纯 Rust 的 `ext4_rs` 迁移到 C 库绑定的 `lwext4_rust`，保持所有现有功能正常工作

---

## 1. 架构变更概览

### 迁移前 (ext4_rs)

```
BlockDevice ──► Ext4BlockDeviceAdapter ──► BlockCache ──► ext4_rs::Ext4
               (ext4_rs::BlockDevice)      (LRU 64-entry)
```

### 迁移后 (lwext4)

```
BlockDevice ──► BlockDeviceStream ──► Ext4BlockWrapper ──► Ext4Wrapper
               (KernelDevOp trait)     (封装 lwext4 C 库)    (Send+Sync)
```

### 关键差异

| 维度 | ext4_rs | lwext4 |
|------|---------|--------|
| 操作粒度 | inode 级 (`read_at(inode_num, offset, buf)`) | 路径级 (`fopen(path) → fread → fclose`) |
| 语言 | 纯 Rust | Rust FFI + C 静态库 |
| 块缓存 | 自实现 64-entry LRU | lwext4 内置 `ext4_bcache` |
| 扇区适配 | `Ext4BlockDeviceAdapter` (扇区→4K块) | `BlockDeviceStream + KernelDevOp` (流式抽象) |
| 挂载 | `Ext4::open(cache)` 永远成功 | `Ext4BlockWrapper::new(stream)` 返回 `Result` |

---

## 2. 文件变更清单

### 2.1 删除的文件

**`os/src/fs/block_cache.rs`** ── 已删除

- **原因**: 死代码，未被 `fs/mod.rs` 引入模块树，且依赖已废弃的 `ext4_rs::BlockDevice`
- **旧功能**: 64-entry LRU 缓存，包装 `ext4_rs::BlockDevice`
- **新方案**: lwext4 C 库自带 `ext4_bcache` 块缓存，无需内核层额外缓存

### 2.2 新增的文件

**`os/src/fs/block_device_stream.rs`** ── 新增

桥接内核 `BlockDevice` trait 和 lwext4 的 `KernelDevOp` trait：

| 类型 | 作用 |
|------|------|
| `BlockDeviceStream` | 流式块设备封装 (`inner`, `pos`, `total_size`) |
| `BlockDeviceStreamOp` | 实现 `KernelDevOp` trait (`write/read/seek/flush`) |
| `Ext4Wrapper` | 包装 `Ext4BlockWrapper`，`unsafe impl Send + Sync` |

512 字节扇区与 lwext4 块之间的转换在 `KernelDevOp` 实现中完成。

**`lwext4_rust/`** ── 新增 (git submodule)

将 C 语言 lwext4 库封装为 Rust `no_std` crate：

| 文件 | 作用 |
|------|------|
| `build.rs` | bindgen 生成 FFI 绑定 + 编译 C 静态库 |
| `src/bindings.rs` | 自动生成的 C → Rust FFI 绑定 |
| `src/blockdev.rs` | 块设备抽象 `Ext4BlockWrapper<K: KernelDevOp>` |
| `src/file.rs` | `Ext4File` 文件操作封装 |
| `c/lwext4/` | 上游 C 库 (git submodule) |

### 2.3 修改的文件

#### 2.3.1 `os/Cargo.toml`

| 操作 | 内容 |
|------|------|
| **移除** | `ext4_rs` 依赖 |
| **新增** | `lwext4_rust = { path = "../lwext4_rust" }` |

#### 2.3.2 `os/src/fs/vfs.rs` ── 核心重构

**导入变更**:
- 移除 `use ext4_rs::{Ext4, InodeFileType};`
- 新增 `use lwext4_rust::Ext4BlockWrapper;`
- 新增 `use lwext4_rust::bindings::*;`
- 新增 `use super::block_device_stream::{BlockDeviceStream, BlockDeviceStreamOp, Ext4Wrapper};`

**数据结构变更**:
- `FsInner::Ext4(Ext4)` → `FsInner::Ext4(Ext4Wrapper)`

**`VfsSuperBlock::new_ext4()`**:

```
旧: Arc<dyn BlockDevice> → Ext4BlockDeviceAdapter → BlockCache → Ext4::open → Arc<Self>
新: Arc<dyn BlockDevice> → BlockDeviceStream → Ext4BlockWrapper::new → Arc<Self>
返回类型: Arc<Self> → Option<Arc<Self>> (不再 panic)
```

**`with_ext4(f)` → `with_fs(f)`**: 闭包参数从 `&Ext4` 变为 `&Ext4Wrapper`

**新增路径级 ext4 操作方法**:

| 方法 | 底层 lwext4 API 调用链 |
|------|------------------------|
| `dir_get_entries(path)` | `ext4_dir_open` → `ext4_dir_entry_next` → `ext4_dir_close` |
| `get_inode_info(path)` | `ext4_fopen` → `ext4_fsize` / `ext4_mode_get` / `ext4_owner_get` → `ext4_fclose` |
| `read_at(path, off, buf)` | `ext4_fopen` → `ext4_fseek` → `ext4_fread` → `ext4_fclose` |
| `write_at(path, off, buf)` | `ext4_fopen` → `ext4_fseek` → `ext4_fwrite` → `ext4_fclose` |
| `create_file(parent, name, is_dir)` | `ext4_dir_mk` (目录) / `ext4_fopen("wb")` (文件) |
| `truncate_file(path, size)` | `ext4_fopen` → `ext4_ftruncate` → `ext4_fclose` |
| `unlink_file(path)` | `ext4_fremove` |
| `set_atime/mtime/ctime(path, t)` | `ext4_atime_set` / `ext4_mtime_set` / `ext4_ctime_set` |

**移除的函数**:
- `vfs_symlink()` → 改为直接调用 `bindings::ext4_fsymlink`
- `vfs_rename()` → 改为直接调用 `bindings::ext4_frename`

**其他函数适配**:

| 函数 | 旧调用方式 | 新调用方式 |
|------|-----------|-----------|
| `vfs_lookup()` | `ext4.ext4_dir_get_entries(parent.inode_num)` | `parent.super_block.dir_get_entries(&parent.fs_path)` |
| `vfs_create()` | `ext4.create_with_attr(ino, name, mode, uid, gid)` | `parent.super_block.create_file(&fs_path, name, is_dir)` |
| `list_apps()` | `ext4.ext4_dir_open("/bin")` | `ROOT_SB.dir_get_entries("/bin")` |

**`vfs_create()` 签名变更**: 移除 `uid/gid` 参数（直接使用 lwext4 默认值）

**关键点变更**:

```
ROOT_SB:     new_ext4(...)  →  new_ext4(...).expect("Failed to mount root ext4 filesystem")
mount_test:  new_ext4(...)  →  match new_ext4(...) { Some(sb) => sb, None => return false }
```

#### 2.3.3 `os/src/fs/inode.rs` ── 核心重构

**移除**:
- `Ext4BlockDeviceAdapter` 结构体及其 `ext4_rs::BlockDevice` impl
- `check_file_permission()` 辅助函数
- `use ext4_rs` 导入
- `use crate::drivers::BlockDevice` 导入
- `use crate::task::current_task` 导入

**所有 inode 级操作改为路径级**:

| 操作 | 旧 (inode 级) | 新 (路径级) |
|------|--------------|------------|
| `read_all()` | `ext4.read_at(inode_num, ...)` | `super_block.read_at(&fs_path, ...)` |
| `stat_inner()` | `ext4.get_inode_ref(inode_num)` | `super_block.get_inode_info(&fs_path)` |
| truncate | `ext4.truncate_inode(...)` | `super_block.truncate_file(&fs_path, 0)` |
| File::read() | `ext4.read_at(inode_num, ...)` | `super_block.read_at(&fs_path, ...)` |
| File::write() | `ext4.write_at(inode_num, ...)` | `super_block.write_at(&fs_path, ...)` |

**新增函数**:
- `rename_at()` ── 直接调用 `lwext4_rust::bindings::ext4_frename`
- `symlink_at()` ── 直接调用 `lwext4_rust::bindings::ext4_fsymlink`

#### 2.3.4 `os/src/syscall/fs.rs` ── 适配路径级 API

**`sys_utimensat()`**:
```
旧: ext4.get_inode_ref(dentry.inode_num) → inode.uid() / inode.mode()
新: dentry.super_block.get_inode_info(&dentry.fs_path) → info.uid / info.mode
```

**`sys_faccessat()`**:
```
旧: ext4.get_inode_ref(dentry.inode_num) → inode.mode()
新: dentry.super_block.get_inode_info(&dentry.fs_path) → info.mode
```

**`sys_readlinkat()`**:
```
旧: ext4.get_inode_ref(dentry.inode_num) → inode.is_link()
新: dentry.super_block.get_inode_info(&dentry.fs_path) → info.is_link
```

**`sys_mount()`** ── mount 失败不再内核 panic:
```
旧: let sb = VfsSuperBlock::new_ext4(...);
新: match VfsSuperBlock::new_ext4(...) { Some(sb) => sb, None => return -17 }
```

#### 2.3.5 `os/src/fs/mod.rs`

| 操作 | 内容 |
|------|------|
| **移除** | `mod block_cache;` |
| **保留** | `pub mod block_device_stream;` |

#### 2.3.6 `lwext4_rust/src/blockdev.rs` ── 多次挂载支持

**新增**:
```rust
use core::sync::atomic::{AtomicU32, Ordering};
static DEV_COUNTER: AtomicU32 = AtomicU32::new(0);
```

**`Ext4BlockWrapper::new()` 变更**:

| 维度 | 旧 | 新 |
|------|----|----|
| 设备名 | 硬编码 `"ext4_fs"` | `"ext4_fs{dev_id}"` (原子递增) |
| 挂载点 | 硬编码 `"/"` | `dev_id == 0 ? "/" : "/mp/"` |
| 并发能力 | 仅支持单设备 | 支持多设备同时挂载 |

#### 2.3.7 `user/src/lib.rs` / `user/src/syscall.rs`

适配 syscall 编号调整，新增 `unlinkat` 等系统调用常量。

---

## 3. 迁移前后 API 对照表

| 操作 | ext4_rs (旧) | lwext4 (新) |
|------|-------------|------------|
| 打开文件 | `ext4.get_inode_ref(inode_num)` | `ext4_fopen(path, flags)` |
| 读取 | `ext4.read_at(inode_num, offset, buf)` | `ext4_fopen` + `ext4_fread` + `ext4_fclose` |
| 写入 | `ext4.write_at(inode_num, offset, buf)` | `ext4_fopen` + `ext4_fwrite` + `ext4_fclose` |
| 创建文件 | `ext4.create_with_attr(ino, name, mode, ...)` | `ext4_fopen(path, "wb")` |
| 创建目录 | `ext4.create_with_attr(ino, name, S_IFDIR, ...)` | `ext4_dir_mk(path)` |
| 删除文件 | `ext4.unlink(parent_ino, name)` | `ext4_fremove(path)` |
| 截断文件 | `ext4.truncate_inode(inode_ref, size)` | `ext4_fopen` + `ext4_ftruncate` + `ext4_fclose` |
| 读取目录 | `ext4.ext4_dir_get_entries(dir_ino)` | `ext4_dir_open` + `ext4_dir_entry_next` + `ext4_dir_close` |
| 重命名 | `ext4.dir_add_entry` / `dir_remove_entry` | `ext4_frename(old, new)` |
| 符号链接 | `ext4.create_with_attr(..., S_IFLNK)` + `write_at` | `ext4_fsymlink(linkpath, target)` |
| 获取属性 | `inode_ref.inode.mode()` `.uid()` `.gid()` ... | `ext4_fopen` + `ext4_mode_get` + `ext4_owner_get` |
| 设置时间戳 | `inode.inode.set_atime()` + `write_back_inode()` | `ext4_atime_set(path, time)` |
| 块设备层 | `Ext4BlockDeviceAdapter` (扇区→块转换) | `BlockDeviceStream` + `KernelDevOp` (I/O 流抽象) |
| 块缓存 | `BlockCache` (64-entry LRU 自实现) | lwext4 内置 `ext4_bcache` |
| 挂载 | `Ext4::open(cache)` 永远成功 | `Ext4BlockWrapper::new(stream)` → `Result` |

---

## 4. 构建验证

```bash
$ cargo check -p os --target riscv64gc-unknown-none-elf
  → 零错误，仅 44 个预存警告

$ cargo build --release --target riscv64gc-unknown-none-elf
  → 零错误，编译成功

$ make fs-img ARCH=riscv64
  → 128MB ext4 文件系统镜像创建成功
```

---

## 5. QEMU 运行测试

### 启动验证

```
[kernel] Hello, world!
[net] VirtIO-Net initialized
/**** APPS ****
basic_runner, brk, cat, chdir, clone, close, ...
(共 51 个应用正确列出)
**************/
Rust user shell
>>
```

### basic_runner 测试结果 (32/32 PASS)

```
[PASS]          brk (exit: 0)
[PASS]        chdir (exit: 0)
[PASS]        clone (exit: 0)
[PASS]        close (exit: 0)
[PASS]          dup (exit: 0)
[PASS]         dup2 (exit: 0)
[PASS]       execve (exit: 0)
[PASS]         exit (exit: 0)
[PASS]         fork (exit: 0)
[PASS]        fstat (exit: 0)
[PASS]       getcwd (exit: 0)
[PASS]     getdents (exit: 0)
[PASS]       getpid (exit: 0)
[PASS]      getppid (exit: 0)
[PASS] gettimeofday (exit: 0)
[PASS]       mkdir_ (exit: 0)
[PASS]         mmap (exit: 0)
[PASS]        mount (exit: 0)
[PASS]       munmap (exit: 0)
[PASS]       openat (exit: 0)
[PASS]         open (exit: 0)
[PASS]         pipe (exit: 0)
[PASS]         read (exit: 0)
[PASS]        sleep (exit: 0)
[PASS]        times (exit: 0)
[PASS]       umount (exit: 0)
[PASS]        uname (exit: 0)
[PASS]       unlink (exit: 0)
[PASS]         wait (exit: 0)
[PASS]      waitpid (exit: 0)
[PASS]        write (exit: 0)
[PASS]        yield (exit: 0)

Passed: 32/32
Failed: 0/32
ALL TESTS PASSED!
```

---

## 6. 已知问题

无。所有 32 项测试全部通过。

---

## 7. lwext4 C 库修改

### 7.1 同盘双挂载死锁修复

**`lwext4_rust/c/lwext4/src/ext4_fs.c:248`** — 删掉 `*read_only = false;`

```diff
-    *read_only = false;
+    /* Keep caller's read_only preference when all features are supported */
```

**根因**: `ext4_fs_check_features()` 在检查通过后会强制设置 `*read_only = false`，覆盖调用方传入的 `read_only=true`。这导致 `ext4_fs_init()` 尝试写 superblock，与第一个实例的缓存冲突，触发无限块读取循环。

**修复**: 删掉该行后，调用方的 `read_only` 参数被尊重。非 root 挂载（dev_id > 0）传入 `read_only=true` → 跳过 superblock 写 → 不会死锁。

### 7.2 GCC 工具链适配

原来 lwext4 使用 `riscv64-linux-musl-gcc` 编译，环境迁移后无该编译器。改用 `riscv64-linux-gnu-gcc` 并适配：

| 文件 | 改动 |
|------|------|
| `lwext4_rust/c/lwext4/toolchain/musl-generic.cmake` | `linux-musl` → `linux-gnu` |
| `lwext4_rust/c/ulibc.c` | 移除与 glibc 冲突的 `stdout`/`fflush` 定义，添加 `_FORTIFY_SOURCE` 桩函数 (`__strcpy_chk`, `__memcpy_chk`, `__memset_chk`, `__printf_chk`) |
| `lwext4_rust/c/lwext4/src/CMakeLists.txt` | 修复 ulibc.c 路径引用 |

### 7.3 lwext4_rust 改进

**`lwext4_rust/src/blockdev.rs`** — `lwext4_mount(skip_recovery)` 函数新增 `skip_recovery: bool` 参数：

```rust
pub unsafe fn lwext4_mount(&mut self, skip_recovery: bool) -> Result<usize, i32>
```

- `skip_recovery=true`（非 root 挂载）时：跳过 `ext4_recover`、`ext4_journal_start`、`ext4_cache_write_back`
- `lwext4_umount` 中通过 `has_journal` 标志决定是否调用 `ext4_journal_stop`
- `ext4_mount` 调用传入 `read_only=skip_recovery`，配合 C 代码修复实现安全同盘挂载

**`lwext4_rust/Cargo.toml`** — `log` 依赖添加 `max_level_error` feature，使 `error!` 宏在 no_std 下生效。

---

## 8. 建议的后续检查清单

1. 对 `open/create/trunc/write/readback` 做单独回归（含边界偏移、空文件、覆盖写）
2. 增加 fs 层日志开关，提升排错效率
3. 补齐完整目录/权限/时间戳语义与 POSIX 错误码一致性
4. 性能优化：考虑 lwext4 缓存配置调优
