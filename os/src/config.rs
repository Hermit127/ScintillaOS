// 这里负责操作系统的基础配置：集中放内核和用户空间共用的基础配置常量。
#[allow(unused)]

pub const USER_STACK_SIZE: usize = 4096 * 256;
pub const KERNEL_STACK_SIZE: usize = 4096 * 4;
// The heap is statically reserved from the physical RAM.  Keep enough user
// frames available for real workloads on the 256 MiB RISC-V QEMU machine.
pub const KERNEL_HEAP_SIZE: usize = 0x400_0000;

pub const PAGE_SIZE: usize = 0x1000;
/// One readable/executable page immediately below the fixed user stack.
pub const SIGNAL_RETURN_TRAMPOLINE: usize = 0x7ffeff000;

/// Maximum number of scheduler CPUs supported by the fixed QEMU SMP build.
///
/// CPU0 is always present. Secondary harts become schedulable only after they
/// register in the online mask, so the same kernel remains usable with
/// `-smp 1` for compatibility checks.
pub const MAX_CPUS: usize = 4;

/// Maximum number of application CPUs used by each supported contest
/// platform.  The scheduler keeps four slots so the RISC-V build can use all
/// four harts, while the LoongArch 2K1000LA-compatible topology exposes two.
#[cfg(target_arch = "riscv64")]
pub const PLATFORM_CPUS: usize = 4;
#[cfg(target_arch = "loongarch64")]
pub const PLATFORM_CPUS: usize = 2;
#[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
pub const PLATFORM_CPUS: usize = 1;
