// 这里负责操作系统的块设备驱动：选择当前架构的块设备实现，并提供根盘和测试盘句柄。
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
#[cfg(all(
    any(feature = "smp-diagnostics", feature = "lock-stats"),
    any(
        target_arch = "riscv64",
        target_arch = "aarch64",
        target_arch = "loongarch64"
    )
))]
pub use virtio_blk::dump_block_io_stats;

use alloc::sync::Arc;
use alloc::vec::Vec;
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
    static ref BLOCK_DEVICES: Vec<Arc<dyn BlockDevice>> = virtio_blk::probe_all();
}

// 按探测顺序取块设备。
// 执行时从 BLOCK_DEVICES 里按下标取 Arc，存在就 clone 一份返回，不存在就返回 None。
pub fn get_block_device(index: usize) -> Option<Arc<dyn BlockDevice>> {
    BLOCK_DEVICES.get(index).cloned()
}

lazy_static! {
    // 测试盘，通常是探测到的第一个设备。
    pub static ref BLOCK_DEVICE: Arc<dyn BlockDevice> = {
        BLOCK_DEVICES.first().cloned()
            .unwrap_or_else(|| Arc::new(VirtIOBlock::new_at(polyhal::pa!(0x10001000))))
    };
    // 根文件系统盘，通常是探测到的第二个设备。
    pub static ref ROOT_BLOCK_DEVICE: Arc<dyn BlockDevice> = {
        BLOCK_DEVICES.get(1).cloned()
            .unwrap_or_else(|| BLOCK_DEVICE.clone())
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
// 简单读写一批块，验证块设备基本可用。
// 执行时给每个测试块填入固定字节模式，写入后立刻读回比较，全部一致才打印通过。
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
