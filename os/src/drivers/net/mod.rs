#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
mod virtio_net;

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
pub use virtio_net::{has_mmio_net_device, VirtIONetAdapter};
