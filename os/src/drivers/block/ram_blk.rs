// 这里负责操作系统的块设备驱动：实现 x86_64 本地测试用的内嵌 ramdisk 块设备。
use core::{
    arch::global_asm,
    ptr::{slice_from_raw_parts, slice_from_raw_parts_mut},
};

extern crate alloc;

use super::BlockDevice;
use log::info;

// 虚拟IO设备
pub struct RamDiskBlock {
    start: usize,
    size: usize,
}

impl BlockDevice for RamDiskBlock {
    // 从内嵌 ramdisk 读取一个 512 字节扇区。
    // 执行时先检查缓冲区大小和扇区范围，再从 ramdisk 对应地址复制数据到 buf。
    fn read_block(&self, sector_offset: usize, buf: &mut [u8]) {
        assert!(buf.len() == 0x200, "block size is not 0x200");
        let rlen = buf.len();
        if (sector_offset * 0x200 + rlen) >= self.size {
            panic!("can't out of ramdisk range")
        };
        unsafe {
            buf.copy_from_slice(
                slice_from_raw_parts((self.start + sector_offset * 0x200) as *const u8, buf.len())
                    .as_ref()
                    .expect("can't deref ptr in the Ramdisk"),
            );
        }
    }

    // 写回内嵌 ramdisk 的指定扇区。
    // 执行时先检查写入范围没有越过镜像末尾，再把 buf 复制到 ramdisk 对应地址。
    fn write_block(&self, sector_offset: usize, buf: &[u8]) {
        let wlen = buf.len();
        if (sector_offset * 0x200 + wlen) >= self.size {
            panic!("can't out of ramdisk range")
        };
        unsafe {
            slice_from_raw_parts_mut((self.start + sector_offset * 0x200) as *mut u8, buf.len())
                .as_mut()
                .expect("can't deref ptr in the ramdisk")
                .copy_from_slice(buf);
        }
    }

    // ramdisk 容量就是链接进来的镜像大小。
    // 执行时直接把构造时保存的 size 转成 u64，作为块设备容量返回。
    fn capacity_bytes(&self) -> Option<u64> {
        Some(self.size as u64)
    }
}

impl RamDiskBlock {
    // 从链接脚本暴露的 ramdisk_start/end 构造 ramdisk 设备。
    // 执行时读取两个链接符号的地址，计算镜像大小，确认非空后保存到 RamDiskBlock。
    pub fn new() -> Self {
        extern "C" {
            // 链接进来的 ramdisk 起始符号。
            // 执行时不会真正调用它，只把函数符号地址当作 ramdisk 起点使用。
            fn ramdisk_start();
            // 链接进来的 ramdisk 结束符号。
            // 执行时不会真正调用它，只把函数符号地址当作 ramdisk 终点使用。
            fn ramdisk_end();
        }
        info!(
            "ramdisk range: {:#x} - {:#x}",
            ramdisk_start as usize, ramdisk_end as usize
        );
        let start = ramdisk_start as _;
        let size = ramdisk_end as usize - ramdisk_start as usize;
        assert_ne!(size, 0, "ramdisk size is 0");
        Self { start, size }
    }
}

#[cfg(target_arch = "loongarch64")]
global_asm!(
    "
    .section .data
    .global ramdisk_start
    .global ramdisk_end
    .align 16
    ramdisk_start:
    .incbin \"./fs-img.img\"
    ramdisk_end:
"
);

#[cfg(target_arch = "x86_64")]
global_asm!(
    "
    .section .data
    .global ramdisk_start
    .global ramdisk_end
    .align 0x200
    ramdisk_start:
    .incbin \"./fs-img.img\"
    ramdisk_end:
"
);
