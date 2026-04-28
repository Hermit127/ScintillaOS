cp target/riscv64gc-unknown-none-elf/release/fs.img fs-img.img
Platform: qemu
warning: profiles for the non root package will be ignored, specify profiles at the workspace root:
package:   /mnt/e/Learning_resources/OS/oskernel2026test/os/Cargo.toml
workspace: /mnt/e/Learning_resources/OS/oskernel2026test/Cargo.toml
warning: profiles for the non root package will be ignored, specify profiles at the workspace root:
package:   /mnt/e/Learning_resources/OS/oskernel2026test/user/Cargo.toml
workspace: /mnt/e/Learning_resources/OS/oskernel2026test/Cargo.toml
   Compiling os v0.1.0 (/mnt/e/Learning_resources/OS/oskernel2026test/os)
warning: creating a shared reference to mutable static is discouraged
  --> user/src/lib.rs:38:19
   |
38 |             .init(HEAP_SPACE.as_ptr() as usize, USER_HEAP_SIZE);
   |                   ^^^^^^^^^^^^^^^^^^^ shared reference to mutable static
   |
   = note: for more information, see <https://doc.rust-lang.org/nightly/edition-guide/rust-2024/static-mut-references.html>
   = note: shared references to mutable statics are dangerous; it's undefined behavior if the static is mutated or if a mutable reference is created for it while the shared reference lives
   = note: `#[warn(static_mut_refs)]` on by default

warning: `user_lib` (lib) generated 1 warning
warning: unused import: `irq::IRQ`
  --> os/src/main.rs:13:34
   |
13 | use polyhal::{common::PageAlloc, irq::IRQ, mem::get_mem_areas, PhysAddr};
   |                                  ^^^^^^^^
   |
   = note: `#[warn(unused_imports)]` on by default

warning: unused import: `frame_alloc`
 --> os/src/drivers/block/virtio_blk.rs:4:17
  |
4 | use crate::mm::{frame_alloc, frame_dealloc, frames_alloc, FrameTracker};
  |                 ^^^^^^^^^^^

warning: unused import: `log::debug`
 --> os/src/drivers/block/virtio_blk.rs:8:5
  |
8 | use log::debug;
  |     ^^^^^^^^^^

warning: unused import: `VirtIOHeader`
  --> os/src/drivers/block/virtio_blk.rs:13:54
   |
13 | use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};
   |                                                      ^^^^^^^^^^^^

warning: unused import: `ext4_rs::crc32`
  --> os/src/syscall/process.rs:11:5
   |
11 | use ext4_rs::crc32;
   |     ^^^^^^^^^^^^^^

warning: unused import: `core::cell::LazyCell`
 --> os/src/task/processor.rs:1:5
  |
1 | use core::cell::LazyCell;
  |     ^^^^^^^^^^^^^^^^^^^^

warning: unused variable: `flags`
  --> os/src/syscall/process.rs:85:18
   |
85 | pub fn sys_clone(flags: usize, _stack: usize, _ptid: usize, _tls: usize, _ctid: usize) -> isize {
   |                  ^^^^^ help: if this is intentional, prefix it with an underscore: `_flags`
   |
   = note: `#[warn(unused_variables)]` on by default

warning: value assigned to `user_sp` is never read
   --> os/src/task/task.rs:223:9
    |
223 |         user_sp -= user_sp % core::mem::size_of::<usize>();
    |         ^^^^^^^
    |
    = help: maybe it is overwritten before being read?
    = note: `#[warn(unused_assignments)]` on by default

warning: constant `S_IFREG` is never used
  --> os/src/fs/mod.rs:58:11
   |
58 | pub const S_IFREG: u32 = 0o100000;
   |           ^^^^^^^
   |
   = note: `#[warn(dead_code)]` on by default

warning: function `sys_open` is never used
  --> os/src/syscall/fs.rs:72:8
   |
72 | pub fn sys_open(path: *const u8, flags: u32) -> isize {
   |        ^^^^^^^^

warning: function `sys_mkdir` is never used
  --> os/src/syscall/fs.rs:98:8
   |
98 | pub fn sys_mkdir(path: *const u8, mode: u32) -> isize {
   |        ^^^^^^^^^

warning: function `sys_unlink` is never used
   --> os/src/syscall/fs.rs:119:8
    |
119 | pub fn sys_unlink(path: *const u8) -> isize {
    |        ^^^^^^^^^^

warning: function `sys_pipe` is never used
   --> os/src/syscall/fs.rs:196:8
    |
196 | pub fn sys_pipe(pipe: *mut usize) -> isize {
    |        ^^^^^^^^

warning: constant `PROT_NONE` is never used
  --> os/src/syscall/process.rs:16:7
   |
16 | const PROT_NONE: usize = 0;
   |       ^^^^^^^^^

warning: constant `MAP_SHARED` is never used
  --> os/src/syscall/process.rs:21:7
   |
21 | const MAP_SHARED: usize = 0x01;
   |       ^^^^^^^^^^

warning: constant `MAP_PRIVATE` is never used
  --> os/src/syscall/process.rs:22:7
   |
22 | const MAP_PRIVATE: usize = 0x02;
   |       ^^^^^^^^^^^

warning: constant `MAP_FIXED` is never used
  --> os/src/syscall/process.rs:23:7
   |
23 | const MAP_FIXED: usize = 0x10;
   |       ^^^^^^^^^

warning: fields `kernel_stack` and `sleeping` are never read
  --> os/src/task/task.rs:54:9
   |
48 | pub struct TaskControlBlockInner {
   |            --------------------- fields in this struct
...
54 |     pub kernel_stack: KernelStack,
   |         ^^^^^^^^^^^^
...
74 |     pub sleeping : Option<u64>,
   |         ^^^^^^^^

warning: creating a shared reference to mutable static is discouraged
  --> os/src/mm/heap_allocator.rs:18:19
   |
18 |             .init(HEAP_SPACE.as_ptr() as usize, KERNEL_HEAP_SIZE);
   |                   ^^^^^^^^^^^^^^^^^^^ shared reference to mutable static
   |
   = note: for more information, see <https://doc.rust-lang.org/nightly/edition-guide/rust-2024/static-mut-references.html>
   = note: shared references to mutable statics are dangerous; it's undefined behavior if the static is mutated or if a mutable reference is created for it while the shared reference lives
   = note: `#[warn(static_mut_refs)]` on by default

warning: `os` (bin "os") generated 19 warnings (run `cargo fix --bin "os"` to apply 6 suggestions)
    Finished `release` profile [optimized] target(s) in 11.17s
qemu-system-riscv64 -machine virt -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 -kernel target/riscv64gc-unknown-none-elf/release/os.bin -nographic -drive file=target/riscv64gc-unknown-none-elf/release/fs.img,if=none,format=raw,id=x0 -smp 1 -D qemu.log -d in_asm,int,pcall,cpu_reset,guest_errors

OpenSBI v1.0
   ____                    _____ ____ _____
  / __ \                  / ____|  _ \_   _|
 | |  | |_ __   ___ _ __ | (___ | |_) || |
 | |  | | '_ \ / _ \ '_ \ \___ \|  _ < | |
 | |__| | |_) |  __/ | | |____) | |_) || |_
  \____/| .__/ \___|_| |_|_____/|____/_____|
        | |
        |_|

Platform Name             : riscv-virtio,qemu
Platform Features         : medeleg
Platform HART Count       : 1
Platform IPI Device       : aclint-mswi
Platform Timer Device     : aclint-mtimer @ 10000000Hz
Platform Console Device   : uart8250
Platform HSM Device       : ---
Platform Reboot Device    : sifive_test
Platform Shutdown Device  : sifive_test
Firmware Base             : 0x80000000
Firmware Size             : 252 KB
Runtime SBI Version       : 0.3

Domain0 Name              : root
Domain0 Boot HART         : 0
Domain0 HARTs             : 0*
Domain0 Region00          : 0x0000000002000000-0x000000000200ffff (I)
Domain0 Region01          : 0x0000000080000000-0x000000008003ffff ()
Domain0 Region02          : 0x0000000000000000-0xffffffffffffffff (R,W,X)
Domain0 Next Address      : 0x0000000080200000
Domain0 Next Arg1         : 0x0000000087000000
Domain0 Next Mode         : S-mode
Domain0 SysReset          : yes

Boot HART ID              : 0
Boot HART Domain          : root
Boot HART ISA             : rv64imafdcsuh
Boot HART Features        : scounteren,mcounteren,time
Boot HART PMP Count       : 16
Boot HART PMP Granularity : 4
Boot HART PMP Address Bits: 54
Boot HART MHPM Count      : 0
Boot HART MIDELEG         : 0x0000000000001666
Boot HART MEDELEG         : 0x0000000000f0b509

  _____      _       _    _          _      
 |  __ \    | |     | |  | |   /\   | |     
 | |__) |__ | |_   _| |__| |  /  \  | |     
 |  ___/ _ \| | | | |  __  | / /\ \ | |     
 | |  | (_) | | |_| | |  | |/ ____ \| |____ 
 |_|   \___/|_|\__, |_|  |_/_/    \_\______|
                __/ |                       
               |___/                        

Platform Arch             : riscv64
Boot HART ID              : 0
Boot HART Count           : 1
Boot Args                 : 
Platform Memory Region    : 0x0000000080000000 - 0x0000000088000000
Platform Memory Available : 0x00000000822bac78 - 0x0000000087000000
Platform Memory Available : 0x0000000087001312 - 0x0000000088000000

[kernel] Hello, world!
init logging
init memory region 0x822bac78 - 0x87000000
init memory region 0x87001312 - 0x88000000
/**** APPS ****
basic_runner
brk
brktest
cat
chdir
clone
close
cmdline_args
count_lines
dup
dup2
execve
exit
fantastic_text
filetest_simple
fork
forktest
forktest2
forktest_simple
forktree
fstat
getchar
getcwd
getdents
getpid
getppid
gettimeofday
hello_world
huge_write
infloop
initproc
matrix
mkdir_
mmap
mount
munmap
nanosleep
open
openat
pipe
pipe_large_test
pipetest
read
run_pipe_test
sig_simple
sig_simple2
sig_tests
sleep
sleep_simple
stack_overflow
store_fault
test_echo
test_times
times
umount
uname
unlink
until_timeout
user_shell
usertests
usertests_simple
wait
waitpid
write
yield
**************/
Rust user shell
>> basic_runner
===== BASIC TEST RUNNER =====
Running 32 tests...

[kernel] Hello, world!
init logging
init memory region 0x822bac78 - 0x87000000
init memory region 0x87001312 - 0x88000000
/**** APPS ****
basic_runner
brk
brktest
cat
chdir
clone
close
cmdline_args
count_lines
dup
dup2
execve
exit
fantastic_text
filetest_simple
fork
forktest
forktest2
forktest_simple
forktree
fstat
getchar
getcwd
getdents
getpid
getppid
gettimeofday
hello_world
huge_write
infloop
initproc
matrix
mkdir_
mmap
mount
munmap
nanosleep
open
openat
pipe
pipe_large_test
pipetest
read
run_pipe_test
sig_simple
sig_simple2
sig_tests
sleep
sleep_simple
stack_overflow
store_fault
test_echo
test_times
times
umount
uname
unlink
until_timeout
user_shell
usertests
usertests_simple
wait
waitpid
write
yield
**************/
Rust user shell
>> basic_runner
===== BASIC TEST RUNNER =====
Running 32 tests...

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
[FAIL]       mkdir_ (exit: -100)
[PASS]         mmap (exit: 0)
[FAIL]        mount (exit: -100)
[PASS]       munmap (exit: 0)
[PASS]       openat (exit: 0)
[PASS]         open (exit: 0)
