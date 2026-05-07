use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::sync::UPSafeCell;

const BLOCK_SIZE: usize = 4096;
const MAX_CACHED: usize = 64;

pub struct BlockCache {
    inner: Arc<dyn ext4_rs::BlockDevice>,
    cache: UPSafeCell<BTreeMap<usize, Vec<u8>>>,
}

impl BlockCache {
    pub fn new(inner: Arc<dyn ext4_rs::BlockDevice>) -> Self {
        Self {
            inner,
            cache: unsafe { UPSafeCell::new(BTreeMap::new()) },
        }
    }

    fn aligned(offset: usize) -> usize {
        (offset / BLOCK_SIZE) * BLOCK_SIZE
    }
}

impl ext4_rs::BlockDevice for BlockCache {
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        let aligned = Self::aligned(offset);
        let blk_off = offset - aligned;
        if blk_off == 0 {
            if let Some(cached) = self.cache.exclusive_access().get(&aligned) {
                return cached.clone();
            }
        }
        let data = self.inner.read_offset(offset);
        if blk_off == 0 {
            self.cache.exclusive_access().insert(aligned, data.clone());
        }
        data
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        self.inner.write_offset(offset, data);
        let aligned = Self::aligned(offset);
        let blk_off = offset - aligned;
        let mut block = self.cache.exclusive_access().get(&aligned)
            .cloned()
            .unwrap_or_else(|| self.inner.read_offset(aligned));
        let end = blk_off + data.len();
        if end > block.len() { block.resize(end, 0); }
        block[blk_off..end].copy_from_slice(data);
        let mut cache = self.cache.exclusive_access();
        if cache.len() >= MAX_CACHED {
            if let Some(k) = cache.keys().next().copied() { cache.remove(&k); }
        }
        cache.insert(aligned, block);
    }
}
