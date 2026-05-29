mod device;
#[cfg(target_arch = "x86_64")]
mod ram_blk;

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
mod virtio_blk;
#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
pub use virtio_blk::{VirtIOBlock, VirtioHal};

use alloc::sync::Arc;
pub use device::BlockDevice;
use lazy_static::*;

#[cfg(target_arch = "x86_64")]
use ram_blk::RamDiskBlock;

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
lazy_static! {
    /// Test disk on virtio bus 0 (provided by evaluation system)
    pub static ref BLOCK_DEVICE: Arc<dyn BlockDevice> = Arc::new(VirtIOBlock::new());
}

#[cfg(any(
    target_arch = "riscv64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
lazy_static! {
    /// Root filesystem disk on virtio bus 1 (user's disk.img)
    pub static ref ROOT_BLOCK_DEVICE: Arc<dyn BlockDevice> = {
        let addr = if cfg!(target_arch = "aarch64") {
            polyhal::pa!(0xa00_0200)
        } else {
            polyhal::pa!(0x10002000)
        };
        Arc::new(VirtIOBlock::new_at(addr))
    };
}

#[cfg(target_arch = "x86_64")]
lazy_static! {
    pub static ref BLOCK_DEVICE: Arc<dyn BlockDevice> = Arc::new(RamDiskBlock::new());
}

#[cfg(target_arch = "x86_64")]
lazy_static! {
    pub static ref ROOT_BLOCK_DEVICE: Arc<dyn BlockDevice> = Arc::new(RamDiskBlock::new());
}

#[allow(unused)]
pub fn block_device_test() {
    let block_device = BLOCK_DEVICE.clone();
    let mut write_buffer = [0u8; 512];
    let mut read_buffer = [0u8; 512];
    for i in 0..512 {
        for byte in write_buffer.iter_mut() {
            *byte = i as u8;
        }
        block_device.write_block(i as usize, &write_buffer);
        block_device.read_block(i as usize, &mut read_buffer);
        assert_eq!(write_buffer, read_buffer);
    }
    println!("block device test passed!");
}
