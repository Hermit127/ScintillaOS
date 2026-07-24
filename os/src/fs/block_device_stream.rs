// 这里负责操作系统的文件管理：把块设备包装成 lwext4 可以读写和 seek 的流接口。
use crate::drivers::BlockDevice;
use alloc::sync::Arc;
use lwext4_rust::Ext4BlockWrapper;
use lwext4_rust::KernelDevOp;

const SECTOR_SIZE: usize = 512;
const MAX_BULK_IO_SIZE: usize = 64 * 1024;

pub struct BlockDeviceStream {
    pub inner: Arc<dyn BlockDevice>,
    pub pos: usize,
    pub total_size: u64,
    /// Reused for unaligned read-modify-write operations.  lwext4 invokes
    /// these callbacks while its FsLock is held, so the callback must not
    /// enter the heap/frame allocator.
    pub scratch: alloc::vec::Vec<u8>,
}

pub struct BlockDeviceStreamOp;

impl KernelDevOp for BlockDeviceStreamOp {
    type DevType = BlockDeviceStream;

    // 把 lwext4 的写请求落到块设备上，非扇区对齐时先读改写。
    // 执行时先计算当前位置对应的扇区和扇区内偏移；对齐写直接分块写，非对齐写先读整扇区再覆盖对应字节。
    fn write(dev: &mut Self::DevType, buf: &[u8]) -> Result<usize, i32> {
        if buf.is_empty() {
            return Ok(0);
        }
        let pos = dev.pos;
        let sector = pos / SECTOR_SIZE;
        let offset = pos % SECTOR_SIZE;

        let end = pos + buf.len();
        let end_sector = (end + SECTOR_SIZE - 1) / SECTOR_SIZE;

        if offset == 0 && buf.len() % SECTOR_SIZE == 0 {
            let mut written = 0;
            while written < buf.len() {
                let chunk = (buf.len() - written).min(MAX_BULK_IO_SIZE);
                dev.inner.write_block(
                    sector + written / SECTOR_SIZE,
                    &buf[written..written + chunk],
                );
                written += chunk;
            }
        } else {
            let total_sectors = end_sector - sector;
            let raw_len = total_sectors * SECTOR_SIZE;
            if raw_len > dev.scratch.len() {
                return Err(-1);
            }
            let inner = dev.inner.clone();
            let raw = &mut dev.scratch[..raw_len];
            for i in 0..total_sectors {
                let begin = i * SECTOR_SIZE;
                inner.read_block(sector + i, &mut raw[begin..begin + SECTOR_SIZE]);
            }
            raw[offset..offset + buf.len()].copy_from_slice(buf);
            for i in 0..total_sectors {
                let begin = i * SECTOR_SIZE;
                inner.write_block(sector + i, &raw[begin..begin + SECTOR_SIZE]);
            }
        }

        dev.pos = end;
        Ok(buf.len())
    }

    // 从块设备读取数据，遇到非扇区对齐的范围就用临时缓冲拼出来。
    // 执行时按当前位置计算扇区范围；对齐读直接读进用户缓冲，非对齐读先读整扇区再拷贝需要的片段。
    fn read(dev: &mut Self::DevType, buf: &mut [u8]) -> Result<usize, i32> {
        if buf.is_empty() {
            return Ok(0);
        }
        let pos = dev.pos;
        let sector = pos / SECTOR_SIZE;
        let offset = pos % SECTOR_SIZE;

        if offset == 0 && buf.len() % SECTOR_SIZE == 0 {
            let mut read = 0;
            while read < buf.len() {
                let chunk = (buf.len() - read).min(MAX_BULK_IO_SIZE);
                dev.inner
                    .read_block(sector + read / SECTOR_SIZE, &mut buf[read..read + chunk]);
                read += chunk;
            }
            dev.pos = pos + buf.len();
        } else {
            let end = pos + buf.len();
            let end_sector = (end + SECTOR_SIZE - 1) / SECTOR_SIZE;
            let total_sectors = end_sector - sector;
            let raw_len = total_sectors * SECTOR_SIZE;
            if raw_len > dev.scratch.len() {
                return Err(-1);
            }
            let inner = dev.inner.clone();
            let raw = &mut dev.scratch[..raw_len];
            for i in 0..total_sectors {
                let begin = i * SECTOR_SIZE;
                inner.read_block(sector + i, &mut raw[begin..begin + SECTOR_SIZE]);
            }
            let len = buf.len();
            buf.copy_from_slice(&raw[offset..offset + len]);
            dev.pos = pos + len;
        }
        Ok(buf.len())
    }

    // 按 C 风格的 whence 调整当前位置，供 lwext4 随机访问镜像。
    // 执行时按 SEEK_SET/SEEK_CUR/SEEK_END 计算新位置，拒绝负偏移，成功后更新流里的 pos。
    fn seek(dev: &mut Self::DevType, off: i64, whence: i32) -> Result<i64, i32> {
        let new_pos = match whence {
            0 => off as i64,
            1 => dev.pos as i64 + off,
            2 => dev.total_size as i64 + off,
            _ => return Err(-1),
        };
        if new_pos < 0 {
            return Err(-1);
        }
        dev.pos = new_pos as usize;
        Ok(dev.pos as i64)
    }

    // 块设备本身没有额外缓存，这里只给 lwext4 一个成功答复。
    // 执行时不访问设备，只返回 0 表示 flush 已完成，真正缓存刷新由底层块设备或 ext4 负责。
    fn flush(_dev: &mut Self::DevType) -> Result<usize, i32> {
        Ok(0)
    }
}

// ==================== Ext4Wrapper (Send+Sync) ====================

pub struct Ext4Wrapper(pub Ext4BlockWrapper<BlockDeviceStreamOp>);

unsafe impl Send for Ext4Wrapper {}
unsafe impl Sync for Ext4Wrapper {}
