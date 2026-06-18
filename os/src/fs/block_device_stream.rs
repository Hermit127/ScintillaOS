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
}

pub struct BlockDeviceStreamOp;

impl KernelDevOp for BlockDeviceStreamOp {
    type DevType = BlockDeviceStream;

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
            let mut raw = alloc::vec![0u8; total_sectors * SECTOR_SIZE];
            for i in 0..total_sectors {
                let begin = i * SECTOR_SIZE;
                dev.inner
                    .read_block(sector + i, &mut raw[begin..begin + SECTOR_SIZE]);
            }
            raw[offset..offset + buf.len()].copy_from_slice(buf);
            for i in 0..total_sectors {
                let begin = i * SECTOR_SIZE;
                dev.inner
                    .write_block(sector + i, &raw[begin..begin + SECTOR_SIZE]);
            }
        }

        dev.pos = end;
        Ok(buf.len())
    }

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
            let mut raw = alloc::vec![0u8; total_sectors * SECTOR_SIZE];
            for i in 0..total_sectors {
                let begin = i * SECTOR_SIZE;
                dev.inner
                    .read_block(sector + i, &mut raw[begin..begin + SECTOR_SIZE]);
            }
            let len = buf.len();
            buf.copy_from_slice(&raw[offset..offset + len]);
            dev.pos = pos + len;
        }
        Ok(buf.len())
    }

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

    fn flush(_dev: &mut Self::DevType) -> Result<usize, i32> {
        Ok(0)
    }
}

// ==================== Ext4Wrapper (Send+Sync) ====================

pub struct Ext4Wrapper(pub Ext4BlockWrapper<BlockDeviceStreamOp>);

unsafe impl Send for Ext4Wrapper {}
unsafe impl Sync for Ext4Wrapper {}
