# OS Kernel 2026 — Agent Context

## Reply

使用中文回复用户

## Branch Migration Rule

- 从 `MachineTest` 向 `main` 迁移修改时，默认迁移除 `user/src/bin/initproc.rs` 和磁盘镜像/磁盘产物之外的所有修改。
- 在 `main` 分支测试时，默认 `initproc` 保持 shell 入口；不要迁移或改成自动跑测试的入口，用户会自己输入命令测试。
- 除非用户明确要求，不要上传或 `push`；构建和测试只做本地操作。

## Build & Run

```bash
# Full contest submission
make all                    # → kernel-rv, kernel-la, disk.img

# Single arch build
make kernel ARCH=riscv64    # riscv64gc-unknown-none-elf
make kernel ARCH=loongarch64 # loongarch64-unknown-none

# FS image (ext4 with user binaries)
make fs-img ARCH=riscv64

# Run (single disk, local dev)
make run ARCH=riscv64

# Run (dual-disk, simulates contest)
make run-dual ARCH=riscv64 TEST_DISK=fs-img.img

# Cargo check (fast verify)
cargo check -p os --target riscv64gc-unknown-none-elf
```

Supported ARCH: `riscv64` | `loongarch64` | `x86_64` | `aarch64`

---

## Architecture

```
main.rs  →  kernel_interrupt (trap handler) → syscall dispatch → syscall/fs.rs, syscall/process.rs
                ↓ Timer trap → schedule()
```

### Module Tree

```
os/src/
├─ main.rs              kernel entry, trap dispatch loop
├─ config.rs            USER_STACK_SIZE, KERNEL_STACK_SIZE, PAGE_SIZE, KERNEL_HEAP_SIZE
├─ syscall/
│  ├─ mod.rs            syscall IDs + dispatch table (match id → handler)
│  ├─ fs.rs             file/dir/mount syscalls (openat, read, write, close, stat, getdents, mount, chdir...)
│  └─ process.rs        process syscalls (fork, exec, waitpid, mmap, munmap, brk, uid/gid, sleep, kill...)
├─ task/
│  ├─ mod.rs            re-exports: current_task, schedule, add_task, PidHandle, SignalFlags...
│  ├─ task.rs           TaskControlBlock + TaskControlBlockInner (uid/gid, fd_table, cwd, memory_set...)
│  ├─ processor.rs      current_task(), schedule(), run_tasks()
│  ├─ manager.rs        pid2task map, add_task, fetch_task
│  ├─ pid.rs            PidHandle (RAII pid alloc/dealloc)
│  ├─ signal.rs         SignalFlags, signal delivery
│  └─ action.rs         SignalAction, sigaction/sigreturn
├─ fs/
│  ├─ mod.rs            Kstat struct, File trait, S_IF* constants
│  ├─ vfs.rs            VfsSuperBlock, Dentry (dentry cache), vfs_create, mount/umount, root fs init
│  ├─ inode.rs          OSInode (File trait impl), open_file_at, mkdir_at, unlink_at, permission check, BlockDevice adapter
│  ├─ block_cache.rs    BlockCache (64-entry LRU cache wrapping ext4_rs BlockDevice)
│  ├─ pipe.rs           Pipe (pipe2 syscall)
│  └─ stdio.rs          Stdin/Stdout (File trait impls)
├─ mm/
│  ├─ mod.rs            re-exports
│  ├─ memory_set.rs     MemorySet, MapArea, mmap/munmap logic, insert_file_area
│  ├─ frame_allocator.rs phys frame alloc/free (FrameTracker)
│  ├─ heap_allocator.rs kernel heap init (linked_list_allocator)
│  ├─ page_table.rs     page table manipulation, translated_ref/refmut/str/byte_buffer
│  └─ vpn_range.rs      VPN range iterator
├─ drivers/
│  ├─ mod.rs            BLOCK_DEVICE, ROOT_BLOCK_DEVICE lazy_static
│  └─ block/mod.rs      VirtIOBlock (BlockDevice trait)
├─ sync/
│  ├─ mod.rs
│  └─ up.rs             UPSafeCell<T> — RefCell wrapper for uniprocessor safe interior mutability
├─ console.rs           println! via SBI uart
├─ logging.rs           log crate init
└─ lang_items.rs        panic/oom handlers
```

---

## Key Data Structures

### TaskControlBlockInner (`task/task.rs:49`)
Core per-process state including:
- `uid: u32, euid: u32, gid: u32, egid: u32` — credential fields
- `fd_table: Vec<Option<Arc<dyn File + Send + Sync>>>` — file descriptor table
- `cwd: Arc<Dentry>, root: Arc<Dentry>` — current working dir, root dir
- `memory_set: MemorySet` — virtual address space
- `parent: Option<Weak<TaskControlBlock>>, children: Vec<Arc<TaskControlBlock>>`
- `exit_code: i32` — encoded as `(exit_code << 8)` for WEXITSTATUS

### Kstat (`fs/mod.rs:11`)
stat/fstat return struct, C repr. Fields: `st_dev, st_ino, st_mode, st_nlink, st_uid, st_gid, st_rdev, st_size, st_blksize, st_blocks, st_atime_sec/nsec, st_mtime_sec/nsec, st_ctime_sec/nsec`

### VfsSuperBlock + Dentry (`fs/vfs.rs`)
- `VfsSuperBlock` wraps `FsInner::Ext4(Ext4Wrapper)` via `UPSafeCell`. Access via `with_fs(|fs| ...)`.
- `Ext4Wrapper` wraps `lwext4_rust::Ext4BlockWrapper`, which is backed by the `lwext4` C library.
- `Dentry` is a directory entry cache node: `name, path, inode_num, is_dir, parent: Weak<Dentry>, children: UPSafeCell<Option<Vec<Arc<Dentry>>>>, super_block: Arc<VfsSuperBlock>`
- `ROOT_DENTRY` + `ROOT_SB` are lazy_static globals

### OSInode (`fs/inode.rs:56`)
`File` trait impl for ext4 files. Fields: `readable, writable, dentry, offset`. All I/O goes through `dentry.super_block.with_ext4(...)`.

---

## Design Patterns

1. **UPSafeCell** — Uniprocessor-safe interior mutability. `unsafe impl Sync`, exclusive access via `borrow_mut()`. Used throughout for static globals and task state.

2. **File trait** — All file-like objects (OSInode, Pipe, Stdin, Stdout) implement `File: Send + Sync`. fd_table stores `Arc<dyn File + Send + Sync>`.

3. **with_fs closure pattern** — All ext4 access goes through `VfsSuperBlock::with_fs(|fs: &Ext4Wrapper| -> R)`. The closure runs while holding the UPSafeCell lock. Be careful not to hold outer locks across with_fs calls (potential deadlock).

4. **Credential flow**:
   - `open_file_at` → gets current task's `euid/egid` → passes to `vfs_create(parent, name, is_dir, uid, gid)` → writes uid/gid to new ext4 inode
   - `fstat` → `OSInode::stat_inner()` → reads `inode.uid()`/`inode.gid()` from ext4 inode
   - `setuid/setgid` → modifies `TaskControlBlockInner.{uid,euid,gid,egid}` directly
   - `fork` → child inherits parent's uid/euid/gid/egid in `TaskControlBlockInner::new()`

5. **BlockDevice stack**:
   ```
   VirtIOBlock (drivers) → BlockDeviceStream → Ext4BlockWrapper → lwext4 C library
   ```

---

## Syscall Dispatch

All syscalls in `syscall/mod.rs:61-123`. 6-arg calling convention. Syscall numbers are Linux RISC-V ABI values. Key notes:
- `SYSCALL_UMOUNT2 = 39` (was 41, moved to avoid conflict)
- `SYSCALL_SHUTDOWN = 48` (intentionally reserved, clashes with `faccessat` — kept for compatibility)
- `SYSCALL_FORK = 401` (clone is 220)
- `waitpid` exit code: `(exit_code << 8)` (Linux WEXITSTATUS format)

---

## lwext4_rust

`lwext4_rust/` — local embedded ext4 filesystem (C library `lwext4` with Rust bindings). Used via `lwext4_rust = { path = "../lwext4_rust", default-features = false }` in os/Cargo.toml.
The `VfsSuperBlock` wraps `FsInner::Lwext4(RwLock<lwext4_rust::Ext4>)`.

---

## Conventions

- Code comments: English
- Don't add doc comments (`///`) unless necessary
- Don't add inline comments (`//`) unless logic is non-obvious
- Keep changes minimal — don't refactor unrelated code

## Contest Test Suite Format

Official testsuits-for-oskernel busybox test expects:

### busybox_cmd.txt (one command per line)
```
echo "#### independent command test"
ash -c exit
sh -c exit
basename /aaa/bbb
cal
...
sh -c 'sleep 5' & ./busybox kill $!
sort test.txt | ./busybox uniq
```

### busybox_testcode.sh (musl version)
- `#!/bin/sh` or `#!/busybox sh`
- First: `./busybox --install -s /bin`
- Print start marker: `echo "#### OS COMP TEST GROUP START busybox-musl ####"`
- Loop: `while IFS= read -r line` from `./busybox_cmd.txt`
- Skip empty/`#` lines
- Execute: `eval "./busybox $line"` (prepends `./busybox` to each cmd line)
- If `$line` already contains `./busybox` (e.g. `sort | ./busybox uniq`), eval works correctly
- `false` is expected to fail (special case in the test)
- Print end marker: `echo "#### OS COMP TEST GROUP END busybox-musl ####"`

### busybox_glibc_testcode.sh (glibc version)
- Same as musl but:
  - Use `./busybox-glibc` instead of `./busybox`
  - `busybox_glibc_cmd.txt` generated by: `sed 's|\./busybox|./busybox-glibc|g' busybox_cmd.txt`
  - Start marker: `busybox-glibc`

### basic_testcode.sh
- Each test is a separate ELF binary: `test_brk`, `test_chdir`, `test_clone`, etc.
- Output format: `========== START test_xxx ==========` / `========== END test_xxx ==========`
- Use existing patterns: `with_ext4`, `UPSafeCell::exclusive_access()`, `Option::ok_or`, `?` operator
- Win32 environment: no `make`/`cargo` available for build verification here; verify on Linux dev machine
