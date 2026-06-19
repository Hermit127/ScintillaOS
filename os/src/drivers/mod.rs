// 这里负责操作系统的设备驱动：汇总平台驱动，并导出块设备和网络设备入口。
pub mod block;
pub mod net;

pub use block::{BlockDevice, BLOCK_DEVICE, ROOT_BLOCK_DEVICE};
