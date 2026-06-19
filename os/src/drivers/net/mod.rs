// 这里负责操作系统的网络设备驱动：汇总网络设备驱动，目前主要导出 VirtIO 网卡。
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
mod virtio_net;

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
pub use virtio_net::{has_mmio_net_device, VirtIONetAdapter};
