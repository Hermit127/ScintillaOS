# VFS 与 Dentry 实现原理文档

## 1. 背景

### 1.1 原有架构的问题

在引入 VFS 之前，内核的文件系统层是**扁平化**的：

```
所有文件操作 → with_ext4() → EXT4_FS (单一全局 ext4 实例)
```

具体问题：
- **单一文件系统**：只有一个全局 `EXT4_FS: UPSafeCell<Ext4>` 实例，无法支持多个挂载的文件系统
- **字符串路径解析**：`normalize_path()` 做纯字符串拼接，`lookup_existing()` 直接调 `ext4_rs::generic_open()`。路径解析与文件系统实现紧耦合，无法感知挂载点
- **无挂载点概念**：进程的 `cwd: String` 只是一个字符串，切换工作目录只是改字符串，没有 vnode/dentry 语义
- **无命名空间隔离**：所有进程共享同一个根目录视角，无法实现 `chroot` 或 per-process mount namespace

### 1.2 目标

实现 Linux VFS 的子集，支持：
- 多文件系统实例（多个 ext4 superblock）
- Dentry 缓存（目录项缓存，加速路径解析）
- 挂载点（mount/umount，路径解析时透明穿越挂载点）
- 进程级文件系统上下文（cwd、root 均为 dentry）

---

## 2. 核心数据结构

### 2.1 VfsSuperBlock（超级块）

代表一个**已挂载的文件系统实例**。每个文件系统（如 ext4）挂载时创建一个 `VfsSuperBlock`。

```
os/src/fs/vfs.rs

pub struct VfsSuperBlock {
    pub fs_type: &'static str,           // 文件系统类型名 ("ext4")
    pub fs_data: UPSafeCell<FsInner>,    // 文件系统私有数据
}

pub enum FsInner {
    Ext4(Ext4),                          // ext4 实例
}
```

**关键设计**：
- `fs_data` 封装了文件系统特定的句柄（如 `ext4_rs::Ext4`），通过 `UPSafeCell` 提供内部可变性
- 通过 `with_ext4()` 方法安全地访问底层 ext4 实例
- 不存储 block device 引用——ext4 实例内部已持有 `Arc<dyn BlockDevice>`

**生命周期**：由 `Arc<VfsSuperBlock>` 管理，被所有属于该文件系统的 dentry 共享引用。

### 2.2 Dentry（目录项）

代表路径中的一个**名字**。Dentry 构成一棵树，树的边表示父子目录关系。

```
os/src/fs/vfs.rs

pub struct Dentry {
    pub name: String,                    // 本组件名 (如 "mnt")
    pub path: String,                    // 统一命名空间路径 (如 "/mnt/foo")
    pub fs_path: String,                 // 文件系统内部路径 (如 "/foo")
    pub inode_num: u32,                  // 在本文件系统中的 inode 号
    pub is_dir: bool,                    // 是否为目录
    pub parent: Option<Arc<Dentry>>,     // 父 dentry
    pub super_block: Arc<VfsSuperBlock>, // 所属文件系统
    pub children: UPSafeCell<Option<Vec<Arc<Dentry>>>>, // 缓存子节点
}
```

**关键字段说明**：

| 字段 | 根 FS 中 /mnt 的值 | 挂载 FS 的 root | 挂载 FS 中 /foo |
|------|-------------------|-----------------|----------------|
| `name` | `"mnt"` | `"/"` | `"foo"` |
| `path` | `"/mnt"` | `"/mnt"` | `"/mnt/foo"` |
| `fs_path` | `"/mnt"` | `"/"` | `"/foo"` |
| `parent` | 指向根 dentry | `None` | 指向挂载 root |
| `super_block` | ROOT_SB | 挂载 SB | 挂载 SB |

- **`path`**：统一命名空间中的完整路径。所有 syscall 对外展示的都是这个路径。
- **`fs_path`**：在所属文件系统内部的路径。传给 `ext4_rs::generic_open()` 等 API 时使用。对根文件系统两者相同；对挂载文件系统，`fs_path` 去掉了挂载点前缀。
- **`children`**：惰性缓存。`None` = 未加载，`Some(Vec)` = 已加载的子节点列表。

**生命周期**：由 `Arc<Dentry>` 管理。被 `OSInode`、进程的 `cwd`/`root`、挂载表等共同持有引用。

### 2.3 Mount（挂载关系）

记录挂载点 dentry 与被挂载文件系统 root dentry 之间的对应关系。

```
pub struct Mount {
    pub mnt_point: Arc<Dentry>,   // 挂载点（位于父文件系统）
    pub mnt_root: Arc<Dentry>,    // 被挂载文件系统的根 dentry
}
```

全局挂载表：
```
lazy_static! {
    pub static ref MOUNT_TABLE: UPSafeCell<Vec<Mount>> = ...;
}
```

**例子**：在 `/mnt` 上挂载一个 ext4 文件系统后：
```
MOUNT_TABLE = [
    Mount { mnt_point: ROOT_DENTRY, mnt_root: ROOT_DENTRY },  // 根自挂载
    Mount { mnt_point: /mnt dentry, mnt_root: 新 FS root },   // /mnt 挂载
]
```

### 2.4 OSInode（打开的文件句柄）

对应 Linux 中 `struct file` + `struct inode` 的合体，即一个打开的文件描述符。

```
pub struct OSInode {
    readable: bool,
    writable: bool,
    dentry: Arc<Dentry>,           // 指向对应的 dentry
    inner: UPSafeCell<OSInodeInner>,
}

pub struct OSInodeInner {
    offset: usize,                 // 文件读写偏移
}
```

所有文件操作（read/write/stat/getdents64）通过 `self.dentry.super_block.with_ext4()` 调度到正确的 ext4 实例。

### 2.5 进程文件系统上下文

```
pub struct TaskControlBlockInner {
    pub cwd: Arc<Dentry>,    // 当前工作目录
    pub root: Arc<Dentry>,   // 根目录 (支持 chroot)
    pub fd_table: Vec<Option<Arc<dyn File + Send + Sync>>>,
    ...
}
```

- `cwd` 和 `root` 均初始化为 `ROOT_DENTRY`
- `fork()`: `Arc::clone()` 浅拷贝 —— 父子进程共享 dentry
- `exec()`: 保留不变

---

## 3. 路径解析

### 3.1 整体流程

```
用户调用 open("/mnt/foo/bar")
  → sys_openat
  → open_file_at(base, "/mnt/foo/bar", flags)
  → vfs_resolve_path(&start, "/mnt/foo/bar")
     │
     ├── 1. 确定起点：绝对路径从 task.root 开始，相对路径从 start(cwd) 开始
     ├── 2. path.split('/') 拆分路径组件
     ├── 3. 逐级调用 vfs_lookup(parent, component)
     └── 4. 返回最终 dentry
  → OSInode::new(dentry)
```

### 3.2 vfs_lookup — 单级查找

```
vfs_lookup(parent, name):
  1. 特殊处理 "." → 返回 parent
  2. 特殊处理 ".." → 调用 vfs_parent(parent)
     - 如果 parent 是某个挂载的 root，则返回挂载点的父目录
     - 否则返回 parent.parent
  3. 查 dentry.children 缓存
     - 命中 → 检查是否为挂载点 → 如果是则返回挂载 root
     - 命中且非挂载点 → 直接返回缓存 dentry
  4. 缓存未命中 → 调用文件系统查找
     - parent.super_block.with_ext4(|ext4| ext4.ext4_dir_get_entries(parent.inode_num))
     - 在返回的条目列表中查找匹配 name 的条目
     - 构建新 Dentry 对象
  5. 加入 parent.children 缓存
  6. 检查 MOUNT_TABLE，如果是挂载点则返回挂载 root
  7. 返回 dentry
```

**关键：挂载点穿越**

假设 `/mnt` 被挂载：
```
vfs_lookup(root, "mnt"):
  → 查找到 /mnt dentry
  → lookup_mount(/mnt dentry) → 发现是挂载点
  → 返回 mounted_root dentry (path="/mnt", fs_path="/")
  
vfs_lookup(mounted_root, "foo"):
  → mounted_root.children 缓存未命中
  → 查询挂载 FS 的 ext4.ext4_dir_get_entries(root_inode=2)
  → 找到 "foo" → 构建 dentry (path="/mnt/foo", fs_path="/foo")
  → 返回
```

### 3.3 vfs_parent — 感知挂载的 ".."

```
vfs_parent(dentry):
  1. 检查 dentry 是否是某个挂载的 root (is_mount_root)
  2. 如果是 → 返回挂载点 dentry 的父目录 (mnt_point.parent)
  3. 否则 → 返回 dentry.parent
```

**例子**：cwd = `/mnt/foo` (在挂载 FS 中)，执行 `chdir("..")`：
```
vfs_lookup(mounted_root, ".."):
  → vfs_parent(mounted_root)
  → is_mount_root(mounted_root) → true → 返回 /mnt dentry 的 parent
  → /mnt dentry 的 parent 是 root dentry
  → cwd 变为 "/"
```

### 3.4 文件创建路径

```
vfs_create(parent, name, is_dir):
  1. 构造文件系统内部路径: parent.fs_path + "/" + name
  2. 调用 parent.super_block.with_ext4(|ext4|
       ext4.generic_open(fs_path, &mut parent_inode, true, file_type, &mut off))
  3. 构建新 Dentry
  4. 加入 parent.children 缓存
```

注意必须使用 `fs_path` 而非 `path`，因为 ext4 只知道自身文件系统内的路径。

---

## 4. 文件系统挂载与卸载

### 4.1 初始化

内核启动时：
```
main() {
    ...
    fs::mount_root();    // ← 将 ROOT_DENTRY 自挂载到 MOUNT_TABLE
    fs::list_apps();     // 列出根文件系统 /bin 内容
    task::add_initproc(); // 创建 init 进程
    ...
}
```

`mount_root()` 在挂载表中注册一个自引用条目，使得根目录也是"合法的挂载点"：
```
MOUNT_TABLE.push(Mount {
    mnt_point: ROOT_DENTRY.clone(),
    mnt_root:  ROOT_DENTRY.clone(),
});
```

### 4.2 sys_mount 流程

```
mount(source, target, fstype, flags, data)
  1. 从用户空间拷贝字符串: source, target, fstype
  2. 验证 fstype == "ext4"
  3. 解析 target 路径 → 得到挂载点 dentry
  4. 验证 target 是目录
  5. 验证 target 未被占用
  6. VfsSuperBlock::new_ext4(BLOCK_DEVICE, "ext4")
     - 内部创建 Ext4BlockDeviceAdapter 适配块设备
     - 调用 Ext4::open(adapter) 打开文件系统
  7. Dentry::new_mount_root(sb, mnt_path)
     - 创建挂载 FS 的根 dentry: name="/", path=挂载点路径, fs_path="/"
  8. MOUNT_TABLE.push(Mount { mnt_point, mnt_root })
```

### 4.3 sys_umount2 流程

```
umount2(target, flags)
  1. 解析 target 路径 → 得到挂载点 dentry
  2. 在 MOUNT_TABLE 中查找 m.mnt_point == dentry
  3. 移除该 Mount 条目
```

---

## 5. 与 ext4 的整合方式

### 5.1 Ext4BlockDeviceAdapter

```
os/src/fs/inode.rs

pub struct Ext4BlockDeviceAdapter {
    pub inner: Arc<dyn BlockDevice>,  // 内核块设备 (VirtIO or RAM disk)
}

impl ext4_rs::BlockDevice for Ext4BlockDeviceAdapter {
    // 将 ext4_rs 的 read_offset/write_offset 调用
    // 转换为内核 BlockDevice trait 的 read_block/write_block 调用
    // 处理 512B 扇区到 4KB ext4 block 的转换
}
```

这是一个适配器模式：内核的 `BlockDevice` trait 和 ext4_rs 的 `BlockDevice` trait 是不同的 trait，适配器负责桥接。

### 5.2 VfsSuperBlock 与 Ext4 的绑定

```
VfsSuperBlock::new_ext4(blk_device, "ext4"):
  1. 创建 Ext4BlockDeviceAdapter { inner: blk_device }
  2. Ext4::open(adapter) → 获得 Ext4 实例
  3. 包装为 VfsSuperBlock { fs_data: FsInner::Ext4(ext4) }
```

每个 `VfsSuperBlock` 封装一个独立的 `Ext4` 实例，有独立的 block cache、inode cache 等。

### 5.3 文件操作调度

所有文件操作通过 `dentry.super_block.with_ext4()` 路由到正确的 ext4 实例：

```
OSInode::read(&self, buf):
  let ext4 = self.dentry.super_block.with_ext4(|e| &e);
  ext4.read_at(self.dentry.inode_num, offset, buf)
```

- 根文件系统的文件 → 路由到 ROOT_SB 的 ext4
- 挂载文件系统的文件 → 路由到挂载 SB 的 ext4

### 5.4 路径解析中的 ext4 调用

| 操作 | ext4 API | 说明 |
|------|---------|------|
| 查找目录项 | `ext4_dir_get_entries(parent_inode)` | 列出目录所有条目，按名查找 |
| 创建文件/目录 | `generic_open(fs_path, parent, create=true, type, nameoff)` | 使用 fs_path |
| 读取文件 | `read_at(inode, offset, buf)` | |
| 写入文件 | `write_at(inode, offset, buf)` | |
| 获取属性 | `get_inode_ref(inode).inode.*` | mode, size, atime, mtime 等 |
| 截断 | `truncate_inode(inode_ref, 0)` | |
| 删除 | `unlink(parent_ref, child_ref, name)` | |

---

## 6. 系统调用层的变更

### 6.1 syscall 适配

所有路径相关 syscall 中，`cwd` 从 `String` 改为 `Arc<Dentry>`：

| syscall | 原实现 | 新实现 |
|---------|--------|--------|
| `sys_openat` | `normalize_path(cwd_string, path)` | `vfs_resolve_path(cwd_dentry, path)` |
| `sys_chdir` | `open_file_at(cwd_string, path)` | `vfs_resolve_path(cwd_dentry, path)` → 设置 `cwd = dentry` |
| `sys_getcwd` | 直接复制 `cwd: String` | 复制 `cwd.path: String` |
| `sys_mkdirat` | `normalize_path` + `generic_open` | `vfs_resolve_parent_and_name` + `vfs_create` |
| `sys_unlinkat` | `normalize_path` + `unlink` | `vfs_resolve_path` + ext4 `unlink` |

### 6.2 新增 syscall

| syscall | 编号 | 功能 |
|---------|------|------|
| `sys_mount` | 40 | 挂载文件系统 |
| `sys_umount2` | 41 | 卸载文件系统 |

### 6.3 未修改的 syscall

以下 syscall 使用 fd 表（不涉及路径），无需修改：
- `sys_read`, `sys_write`, `sys_close`
- `sys_dup`, `sys_dup2`, `sys_dup3`
- `sys_pipe2`, `sys_fstat`, `sys_getdents64`

---

## 7. 文件结构

```
os/src/fs/
├── mod.rs          # File trait, Kstat, 类型常量, re-export
├── vfs.rs          # [NEW] VFS 核心: Dentry, VfsSuperBlock, Mount, 路径解析
├── inode.rs        # [REWRITE] OSInode (持有 Dentry), open_file_at, mkdir_at, unlink_at
├── pipe.rs         # Pipe (未修改)
└── stdio.rs        # Stdin/Stdout (未修改)

os/src/task/
├── task.rs         # [MODIFY] cwd/root 改为 Arc<Dentry>
└── mod.rs          # INITPROC 创建

os/src/syscall/
├── mod.rs          # [MODIFY] 新增 SYSCALL_MOUNT/SYSCALL_UMOUNT2
└── fs.rs           # [REWRITE] 适配 dentry，新增 sys_mount/sys_umount2
```

---

## 8. 内存与并发模型

- **单处理器**：使用 `UPSafeCell`（`RefCell` 包装），`exclusive_access()` 在借用冲突时 panic
- **无锁设计**：所有数据结构通过 `Arc` 共享引用，`UPSafeCell` 提供内部可变性
- **惰性缓存**：Dentry 的 `children` 字段为 `Option<Vec>`，`None` = 未加载，按需填充
- **无回收**：dentry 缓存不回收（简化实现），在长期运行中可能增长
- **lazy_static**：全局数据（ROOT_SB, ROOT_DENTRY, MOUNT_TABLE）使用 `lazy_static!` + `spin_no_std` 初始化

---

## 9. 架构图

```
┌─────────────────────────────────────────────────────────────┐
│                     System Calls (syscall/)                  │
│  openat  chdir  getcwd  mkdirat  unlinkat  mount  umount2  │
└──────────┬──────────────────────────────────────────────────┘
           │
┌──────────▼──────────────────────────────────────────────────┐
│                    File Operations (inode.rs)                 │
│  open_file_at  mkdir_at  unlink_at                          │
│  OSInode: read / write / stat / getdents64                  │
└──────────┬──────────────────────────────────────────────────┘
           │
┌──────────▼──────────────────────────────────────────────────┐
│                      VFS Layer (vfs.rs)                      │
│  ┌──────────────┐  ┌──────────────┐  ┌───────────────────┐  │
│  │    Dentry    │  │    Mount     │  │  VfsSuperBlock    │  │
│  │  name, path  │  │ mnt_point    │  │  fs_type          │  │
│  │  fs_path     │  │ mnt_root     │  │  fs_data(FsInner) │  │
│  │  inode_num   │  └──────────────┘  │  with_ext4()      │  │
│  │  is_dir      │                    └───────────────────┘  │
│  │  parent      │                                           │
│  │  super_block │  ┌──────────────────────────────────────┐ │
│  │  children    │  │  Path Resolution                     │ │
│  └──────────────┘  │  vfs_lookup / vfs_resolve_path       │ │
│                    │  vfs_parent / vfs_create              │ │
│                    └──────────────────────────────────────┘ │
│                    ┌──────────────────────────────────────┐ │
│                    │  Global State                        │ │
│                    │  MOUNT_TABLE / ROOT_SB / ROOT_DENTRY │ │
│                    └──────────────────────────────────────┘ │
└──────────┬──────────────────────────────────────────────────┘
           │
┌──────────▼──────────────────────────────────────────────────┐
│              ext4 Backend (ext4_rs crate)                    │
│  Ext4::open  generic_open  read_at  write_at                │
│  get_inode_ref  ext4_dir_get_entries  truncate_inode        │
└──────────┬──────────────────────────────────────────────────┘
           │
┌──────────▼──────────────────────────────────────────────────┐
│         Ext4BlockDeviceAdapter (inode.rs)                    │
│  ext4_rs::BlockDevice ←→ drivers::BlockDevice               │
│  512B sectors ←→ 4KB ext4 blocks                            │
└──────────┬──────────────────────────────────────────────────┘
           │
┌──────────▼──────────────────────────────────────────────────┐
│              Block Device (drivers/block/)                   │
│  VirtIOBlock (riscv64/aarch64)  /  RamDiskBlock (x86_64)    │
└─────────────────────────────────────────────────────────────┘
```

---

## 10. 使用示例

### 创建目录并挂载

```rust
// 用户态测试程序
sys_mkdir("/mnt\0".as_ptr(), 0);          // 创建挂载点目录
let ret = sys_mount("", "/mnt", "ext4", 0, &[]);  // 挂载 ext4
```

### 挂载后路径透明访问

```
open("/mnt/foo")  → vfs_resolve_path → 自动穿越挂载点 → 访问挂载 FS 中的 /foo
chdir("/mnt")     → vfs_resolve_path → 返回挂载 root dentry → cwd = 挂载 root
getcwd()          → 返回 "/mnt" (挂载 root 的 path 字段)
```

### 卸载

```rust
let ret = sys_umount2("/", 0);  // 卸载根目录（通常用于测试）
```
