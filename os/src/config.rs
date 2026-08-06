// 这里负责操作系统的基础配置：集中放内核和用户空间共用的基础配置常量。
#[allow(unused)]

// Linux userspace commonly starts with an 8 MiB main-thread stack limit.  A
// 1 MiB fixed mapping is too small for rustc and caused its first stack-growth
// access to collide with the signal restorer page below it.
pub const USER_STACK_SIZE: usize = 8 * 1024 * 1024;
pub const KERNEL_STACK_SIZE: usize = 4096 * 4;
// The heap is statically reserved from the physical RAM.  Keep enough user
// frames available for real workloads on the 256 MiB RISC-V QEMU machine.
pub const KERNEL_HEAP_SIZE: usize = 0x400_0000;

pub const PAGE_SIZE: usize = 0x1000;
/// One readable/executable page kept outside the reserved initial stack VMA.
/// This leaves room for normal downward stack growth without turning it into
/// a write fault on the signal-restorer code page.
pub const SIGNAL_RETURN_TRAMPOLINE: usize = 0x7_fc00_0000;

/// Maximum number of scheduler CPUs supported by the QEMU SMP build.
///
/// CPU0 is always present. Secondary harts become schedulable only after they
/// register in the online mask, so the same kernel remains usable with
/// `-smp 1` for compatibility checks.
pub const MAX_CPUS: usize = 12;

/// Maximum number of application CPUs used by each supported contest
/// platform. A smaller `-smp` value remains supported because secondary CPUs
/// register online dynamically during boot.
#[cfg(target_arch = "riscv64")]
pub const PLATFORM_CPUS: usize = 8;
#[cfg(target_arch = "loongarch64")]
pub const PLATFORM_CPUS: usize = 12;
#[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
pub const PLATFORM_CPUS: usize = 1;
