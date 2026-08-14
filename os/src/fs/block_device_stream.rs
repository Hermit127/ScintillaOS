// 这里负责操作系统的文件管理：把块设备包装成 lwext4 可以读写和 seek 的流接口。
use crate::drivers::BlockDevice;
use alloc::sync::Arc;
#[cfg(feature = "mount-trace")]
use core::sync::atomic::{AtomicUsize, Ordering};
use lwext4_rust::Ext4BlockWrapper;
use lwext4_rust::KernelDevOp;

const SECTOR_SIZE: usize = 512;
const CACHE_BLOCK_SIZE: usize = 4096;
const CACHE_SECTOR_COUNT: usize = CACHE_BLOCK_SIZE / SECTOR_SIZE;
const CACHE_PREFERRED_SLOT_COUNT: usize = 4096;
const CACHE_MIN_SLOT_COUNT: usize = 1024;
const MAX_BULK_IO_SIZE: usize = 128 * 1024;
const SEQUENTIAL_READAHEAD_BLOCKS: usize = 64 * 1024 / CACHE_BLOCK_SIZE;
const SEQUENTIAL_READAHEAD_TRIGGER: usize = 2;
const INVALID_CACHE_SLOT: usize = usize::MAX;
#[cfg(feature = "mount-trace")]
static MOUNT_TRACE_TEST_DISK_READS: AtomicUsize = AtomicUsize::new(0);

pub struct BlockDeviceStream {
    pub inner: Arc<dyn BlockDevice>,
    pub pos: usize,
    pub total_size: u64,
    /// Reused for unaligned read-modify-write operations.  lwext4 invokes
    /// these callbacks while its FsLock is held, so the callback must not
    /// enter the heap/frame allocator.
    pub scratch: alloc::vec::Vec<u8>,
    cache: alloc::vec::Vec<BlockCacheSlot>,
    cache_hash_heads: alloc::vec::Vec<usize>,
    flush_order: alloc::vec::Vec<usize>,
    replacement_cursor: usize,
    valid_count: usize,
    cache_clock: u64,
    dirty_count: usize,
    sequential_next_block: usize,
    sequential_read_streak: usize,
    #[cfg(feature = "lock-stats")]
    cache_hits: u64,
    #[cfg(feature = "lock-stats")]
    cache_misses: u64,
    #[cfg(feature = "lock-stats")]
    dirty_evictions: u64,
    #[cfg(feature = "lock-stats")]
    threshold_flush_blocks: u64,
    #[cfg(feature = "lock-stats")]
    explicit_flush_blocks: u64,
    #[cfg(feature = "lock-stats")]
    dirty_peak: usize,
    #[cfg(feature = "lock-stats")]
    replacement_scans: u64,
    #[cfg(feature = "lock-stats")]
    clean_replacements: u64,
    #[cfg(feature = "lock-stats")]
    writeback_runs: u64,
    #[cfg(feature = "lock-stats")]
    read_cache_hit_ops: u64,
    #[cfg(feature = "lock-stats")]
    read_cache_hit_bytes: u64,
    #[cfg(feature = "lock-stats")]
    read_partial_fills: u64,
    #[cfg(feature = "lock-stats")]
    read_partial_bytes: u64,
    #[cfg(feature = "lock-stats")]
    read_direct_runs: u64,
    #[cfg(feature = "lock-stats")]
    read_direct_bytes: u64,
    #[cfg(feature = "lock-stats")]
    write_rmw_fills: u64,
    #[cfg(feature = "lock-stats")]
    readahead_runs: u64,
    #[cfg(feature = "lock-stats")]
    readahead_blocks: u64,
    #[cfg(feature = "lock-stats")]
    readahead_hits: u64,
}

struct BlockCacheSlot {
    block: usize,
    stamp: u64,
    hash_next: usize,
    valid: bool,
    dirty: bool,
    referenced: bool,
    prefetched: bool,
    data: [u8; CACHE_BLOCK_SIZE],
}

impl BlockCacheSlot {
    const fn new() -> Self {
        Self {
            block: 0,
            stamp: 0,
            hash_next: INVALID_CACHE_SLOT,
            valid: false,
            dirty: false,
            referenced: false,
            prefetched: false,
            data: [0; CACHE_BLOCK_SIZE],
        }
    }
}

impl BlockDeviceStream {
    pub fn new(inner: Arc<dyn BlockDevice>, total_size: u64) -> Self {
        let mut cache = alloc::vec::Vec::new();
        if cfg!(feature = "block-write-cache") {
            for slot_count in [CACHE_PREFERRED_SLOT_COUNT, CACHE_MIN_SLOT_COUNT] {
                if cache.try_reserve_exact(slot_count).is_ok() {
                    for _ in 0..slot_count {
                        cache.push(BlockCacheSlot::new());
                    }
                    break;
                }
            }
        }
        let mut cache_hash_heads = alloc::vec::Vec::new();
        if !cache.is_empty() {
            let bucket_count = (cache.len() * 2).next_power_of_two();
            if cache_hash_heads.try_reserve_exact(bucket_count).is_ok() {
                for _ in 0..bucket_count {
                    cache_hash_heads.push(INVALID_CACHE_SLOT);
                }
            } else {
                cache.clear();
            }
        }
        let mut flush_order = alloc::vec::Vec::new();
        if flush_order.try_reserve_exact(cache.len()).is_err() {
            cache.clear();
            cache_hash_heads.clear();
        }
        Self {
            inner,
            pos: 0,
            total_size,
            scratch: alloc::vec![0u8; MAX_BULK_IO_SIZE],
            cache,
            cache_hash_heads,
            flush_order,
            replacement_cursor: 0,
            valid_count: 0,
            cache_clock: 0,
            dirty_count: 0,
            sequential_next_block: INVALID_CACHE_SLOT,
            sequential_read_streak: 0,
            #[cfg(feature = "lock-stats")]
            cache_hits: 0,
            #[cfg(feature = "lock-stats")]
            cache_misses: 0,
            #[cfg(feature = "lock-stats")]
            dirty_evictions: 0,
            #[cfg(feature = "lock-stats")]
            threshold_flush_blocks: 0,
            #[cfg(feature = "lock-stats")]
            explicit_flush_blocks: 0,
            #[cfg(feature = "lock-stats")]
            dirty_peak: 0,
            #[cfg(feature = "lock-stats")]
            replacement_scans: 0,
            #[cfg(feature = "lock-stats")]
            clean_replacements: 0,
            #[cfg(feature = "lock-stats")]
            writeback_runs: 0,
            #[cfg(feature = "lock-stats")]
            read_cache_hit_ops: 0,
            #[cfg(feature = "lock-stats")]
            read_cache_hit_bytes: 0,
            #[cfg(feature = "lock-stats")]
            read_partial_fills: 0,
            #[cfg(feature = "lock-stats")]
            read_partial_bytes: 0,
            #[cfg(feature = "lock-stats")]
            read_direct_runs: 0,
            #[cfg(feature = "lock-stats")]
            read_direct_bytes: 0,
            #[cfg(feature = "lock-stats")]
            write_rmw_fills: 0,
            #[cfg(feature = "lock-stats")]
            readahead_runs: 0,
            #[cfg(feature = "lock-stats")]
            readahead_blocks: 0,
            #[cfg(feature = "lock-stats")]
            readahead_hits: 0,
        }
    }

    fn next_stamp(&mut self) -> u64 {
        self.cache_clock = self.cache_clock.wrapping_add(1).max(1);
        self.cache_clock
    }

    fn cache_hash(&self, block: usize) -> usize {
        let mut value = block;
        value ^= value >> 16;
        value = value.wrapping_mul(0x7feb_352d);
        value ^= value >> 15;
        value = value.wrapping_mul(0x846c_a68b);
        value ^= value >> 16;
        value & (self.cache_hash_heads.len() - 1)
    }

    fn cache_slot(&self, block: usize) -> Option<usize> {
        if self.cache_hash_heads.is_empty() {
            return None;
        }
        let mut index = self.cache_hash_heads[self.cache_hash(block)];
        while index != INVALID_CACHE_SLOT {
            let slot = &self.cache[index];
            if slot.valid && slot.block == block {
                return Some(index);
            }
            index = slot.hash_next;
        }
        None
    }

    fn insert_cache_index(&mut self, index: usize) {
        let bucket = self.cache_hash(self.cache[index].block);
        self.cache[index].hash_next = self.cache_hash_heads[bucket];
        self.cache_hash_heads[bucket] = index;
    }

    fn remove_cache_index(&mut self, index: usize) {
        let bucket = self.cache_hash(self.cache[index].block);
        let mut current = self.cache_hash_heads[bucket];
        let mut previous = INVALID_CACHE_SLOT;
        while current != INVALID_CACHE_SLOT {
            if current == index {
                let next = self.cache[current].hash_next;
                if previous == INVALID_CACHE_SLOT {
                    self.cache_hash_heads[bucket] = next;
                } else {
                    self.cache[previous].hash_next = next;
                }
                self.cache[index].hash_next = INVALID_CACHE_SLOT;
                return;
            }
            previous = current;
            current = self.cache[current].hash_next;
        }
        debug_assert!(false, "cache index is missing from its hash bucket");
    }

    fn replacement_slot(&mut self) -> Option<usize> {
        if self.cache.is_empty() {
            return None;
        }
        if self.valid_count < self.cache.len() {
            let index = self.valid_count;
            self.valid_count += 1;
            return Some(index);
        }

        let cache_len = self.cache.len();
        let mut scanned = 0usize;
        while scanned < cache_len * 2 {
            let index = self.replacement_cursor;
            self.replacement_cursor = (self.replacement_cursor + 1) % cache_len;
            scanned += 1;
            if self.cache[index].referenced {
                self.cache[index].referenced = false;
                continue;
            }
            if self.cache[index].dirty {
                continue;
            }
            #[cfg(feature = "lock-stats")]
            {
                self.replacement_scans += scanned as u64;
                self.clean_replacements += 1;
            }
            return Some(index);
        }

        // A high-watermark writeback normally leaves many clean slots. This
        // fallback only handles a cache where every clean page was touched
        // during both CLOCK passes.
        for _ in 0..cache_len {
            let index = self.replacement_cursor;
            self.replacement_cursor = (self.replacement_cursor + 1) % cache_len;
            scanned += 1;
            if !self.cache[index].dirty {
                #[cfg(feature = "lock-stats")]
                {
                    self.replacement_scans += scanned as u64;
                    self.clean_replacements += 1;
                }
                return Some(index);
            }
        }

        #[cfg(feature = "lock-stats")]
        {
            self.replacement_scans += scanned as u64;
        }
        Some(self.replacement_cursor)
    }

    fn flush_slot(&mut self, index: usize) {
        if !self.cache[index].dirty {
            return;
        }
        #[cfg(feature = "lock-stats")]
        {
            self.dirty_evictions += 1;
        }
        let sector = self.cache[index].block * CACHE_SECTOR_COUNT;
        self.inner.write_block(sector, &self.cache[index].data);
        self.cache[index].dirty = false;
        self.dirty_count = self.dirty_count.saturating_sub(1);
    }

    fn flush_dirty_cache_to(&mut self, target_dirty: usize) {
        if self.dirty_count <= target_dirty {
            return;
        }
        self.flush_order.clear();
        for (index, slot) in self.cache.iter().enumerate() {
            if slot.dirty {
                self.flush_order.push(index);
            }
        }
        let flush_count = self
            .dirty_count
            .saturating_sub(target_dirty)
            .min(self.flush_order.len());
        #[cfg(feature = "lock-stats")]
        if target_dirty == 0 {
            self.explicit_flush_blocks += flush_count as u64;
        } else {
            self.threshold_flush_blocks += flush_count as u64;
        }
        if flush_count < self.flush_order.len() {
            let cache = &self.cache;
            self.flush_order
                .sort_unstable_by_key(|&index| cache[index].stamp);
            self.flush_order.truncate(flush_count);
        }
        self.flush_selected_slots();
        debug_assert!(self.dirty_count <= target_dirty);
    }

    fn flush_selected_slots(&mut self) {
        if self.flush_order.is_empty() {
            return;
        }
        let cache = &self.cache;
        self.flush_order
            .sort_unstable_by_key(|&index| cache[index].block);

        let max_blocks = self.scratch.len() / CACHE_BLOCK_SIZE;
        let mut cursor = 0usize;
        while cursor < self.flush_order.len() {
            let first_cursor = cursor;
            let first_block = self.cache[self.flush_order[cursor]].block;
            cursor += 1;
            while cursor < self.flush_order.len()
                && cursor - first_cursor < max_blocks
                && self.cache[self.flush_order[cursor]].block
                    == first_block + (cursor - first_cursor)
            {
                cursor += 1;
            }
            let block_count = cursor - first_cursor;
            for offset in 0..block_count {
                let index = self.flush_order[first_cursor + offset];
                let begin = offset * CACHE_BLOCK_SIZE;
                self.scratch[begin..begin + CACHE_BLOCK_SIZE]
                    .copy_from_slice(&self.cache[index].data);
            }
            self.inner.write_block(
                first_block * CACHE_SECTOR_COUNT,
                &self.scratch[..block_count * CACHE_BLOCK_SIZE],
            );
            #[cfg(feature = "lock-stats")]
            {
                self.writeback_runs += 1;
            }
            for order_index in first_cursor..cursor {
                let index = self.flush_order[order_index];
                self.cache[index].dirty = false;
            }
            self.dirty_count = self.dirty_count.saturating_sub(block_count);
        }
    }

    fn flush_dirty_cache(&mut self) {
        self.flush_dirty_cache_to(0);
    }

    fn observe_aligned_read(&mut self, block: usize, block_count: usize) -> bool {
        if block == self.sequential_next_block {
            self.sequential_read_streak = self.sequential_read_streak.saturating_add(1);
        } else {
            self.sequential_read_streak = 1;
        }
        self.sequential_next_block = block.saturating_add(block_count);
        self.sequential_read_streak >= SEQUENTIAL_READAHEAD_TRIGGER
    }

    fn prefetch_read_run(&mut self, first_block: usize) -> Option<usize> {
        let total_blocks = (self.total_size as usize) / CACHE_BLOCK_SIZE;
        let mut block_count = total_blocks
            .saturating_sub(first_block)
            .min(SEQUENTIAL_READAHEAD_BLOCKS);
        for offset in 1..block_count {
            if self.cache_slot(first_block + offset).is_some() {
                block_count = offset;
                break;
            }
        }
        if block_count < 2 {
            return None;
        }

        let mut slots = [INVALID_CACHE_SLOT; SEQUENTIAL_READAHEAD_BLOCKS];
        for (offset, slot) in slots[..block_count].iter_mut().enumerate() {
            *slot = self.prepare_slot(first_block + offset, false)?;
        }
        let read_len = block_count * CACHE_BLOCK_SIZE;
        self.inner.read_block(
            first_block * CACHE_SECTOR_COUNT,
            &mut self.scratch[..read_len],
        );
        let (cache, scratch) = (&mut self.cache, &self.scratch);
        for (offset, &index) in slots[..block_count].iter().enumerate() {
            let begin = offset * CACHE_BLOCK_SIZE;
            cache[index]
                .data
                .copy_from_slice(&scratch[begin..begin + CACHE_BLOCK_SIZE]);
            cache[index].referenced = offset == 0;
            cache[index].prefetched = offset != 0;
        }
        #[cfg(feature = "lock-stats")]
        {
            self.readahead_runs += 1;
            self.readahead_blocks += block_count as u64;
            self.read_direct_runs += 1;
            self.read_direct_bytes += read_len as u64;
        }
        Some(slots[0])
    }

    fn prepare_slot(&mut self, block: usize, load: bool) -> Option<usize> {
        if let Some(index) = self.cache_slot(block) {
            #[cfg(feature = "lock-stats")]
            {
                self.cache_hits += 1;
            }
            let stamp = self.next_stamp();
            self.cache[index].stamp = stamp;
            self.cache[index].referenced = true;
            return Some(index);
        }
        #[cfg(feature = "lock-stats")]
        {
            self.cache_misses += 1;
        }
        let dirty_high = self.cache.len() * 3 / 4;
        if dirty_high != 0 && self.dirty_count >= dirty_high {
            self.flush_dirty_cache_to(self.cache.len() / 2);
        }
        let index = self.replacement_slot()?;
        if self.cache[index].valid {
            self.flush_slot(index);
            self.remove_cache_index(index);
        }
        self.cache[index].block = block;
        self.cache[index].valid = true;
        self.cache[index].dirty = false;
        self.cache[index].referenced = true;
        self.cache[index].prefetched = false;
        self.cache[index].stamp = self.next_stamp();
        self.insert_cache_index(index);
        if load {
            self.inner
                .read_block(block * CACHE_SECTOR_COUNT, &mut self.cache[index].data);
        }
        Some(index)
    }

    fn direct_write(&mut self, buf: &[u8]) -> Result<usize, i32> {
        let pos = self.pos;
        let sector = pos / SECTOR_SIZE;
        let offset = pos % SECTOR_SIZE;
        let end = pos.checked_add(buf.len()).ok_or(-1)?;
        let end_sector = (end + SECTOR_SIZE - 1) / SECTOR_SIZE;
        if offset == 0 && buf.len() % SECTOR_SIZE == 0 {
            let mut written = 0;
            while written < buf.len() {
                let chunk = (buf.len() - written).min(MAX_BULK_IO_SIZE);
                self.inner.write_block(
                    sector + written / SECTOR_SIZE,
                    &buf[written..written + chunk],
                );
                written += chunk;
            }
        } else {
            let raw_len = (end_sector - sector) * SECTOR_SIZE;
            if raw_len > self.scratch.len() {
                return Err(-1);
            }
            self.inner.read_block(sector, &mut self.scratch[..raw_len]);
            self.scratch[offset..offset + buf.len()].copy_from_slice(buf);
            self.inner.write_block(sector, &self.scratch[..raw_len]);
        }
        self.pos = end;
        Ok(buf.len())
    }

    fn direct_read(&mut self, buf: &mut [u8]) -> Result<usize, i32> {
        let pos = self.pos;
        let sector = pos / SECTOR_SIZE;
        let offset = pos % SECTOR_SIZE;
        if offset == 0 && buf.len() % SECTOR_SIZE == 0 {
            let mut read = 0;
            while read < buf.len() {
                let chunk = (buf.len() - read).min(MAX_BULK_IO_SIZE);
                self.inner
                    .read_block(sector + read / SECTOR_SIZE, &mut buf[read..read + chunk]);
                read += chunk;
            }
        } else {
            let end = pos.checked_add(buf.len()).ok_or(-1)?;
            let end_sector = (end + SECTOR_SIZE - 1) / SECTOR_SIZE;
            let raw_len = (end_sector - sector) * SECTOR_SIZE;
            if raw_len > self.scratch.len() {
                return Err(-1);
            }
            self.inner.read_block(sector, &mut self.scratch[..raw_len]);
            buf.copy_from_slice(&self.scratch[offset..offset + buf.len()]);
        }
        self.pos = pos + buf.len();
        Ok(buf.len())
    }
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
        let end = dev.pos.checked_add(buf.len()).ok_or(-1)?;
        if end as u64 > dev.total_size {
            return Err(-1);
        }
        if dev.cache.is_empty() {
            return dev.direct_write(buf);
        }
        let start = dev.pos;
        let mut copied = 0usize;
        while copied < buf.len() {
            let absolute = start + copied;
            let block = absolute / CACHE_BLOCK_SIZE;
            let in_block = absolute % CACHE_BLOCK_SIZE;
            let chunk = (buf.len() - copied).min(CACHE_BLOCK_SIZE - in_block);
            let full_overwrite = in_block == 0 && chunk == CACHE_BLOCK_SIZE;
            #[cfg(feature = "lock-stats")]
            if !full_overwrite && dev.cache_slot(block).is_none() {
                dev.write_rmw_fills += 1;
            }
            let index = dev.prepare_slot(block, !full_overwrite).ok_or(-1)?;
            dev.cache[index].data[in_block..in_block + chunk]
                .copy_from_slice(&buf[copied..copied + chunk]);
            dev.cache[index].prefetched = false;
            if !dev.cache[index].dirty {
                dev.cache[index].dirty = true;
                dev.dirty_count += 1;
                #[cfg(feature = "lock-stats")]
                {
                    dev.dirty_peak = dev.dirty_peak.max(dev.dirty_count);
                }
            }
            copied += chunk;
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
        #[cfg(feature = "mount-trace")]
        let sector = pos / SECTOR_SIZE;
        #[cfg(feature = "mount-trace")]
        let trace = dev.total_size == 512 * 1024 * 1024
            && MOUNT_TRACE_TEST_DISK_READS.fetch_add(1, Ordering::Relaxed) < 32;
        #[cfg(feature = "mount-trace")]
        if trace {
            crate::println!(
                "[mount-trace] test-disk-read-begin pos={} len={} sector={}",
                pos,
                buf.len(),
                sector
            );
        }

        let end = pos.checked_add(buf.len()).ok_or(-1)?;
        if end as u64 > dev.total_size {
            return Err(-1);
        }
        if dev.cache.is_empty() {
            return dev.direct_read(buf);
        }
        let mut copied = 0usize;
        while copied < buf.len() {
            let absolute = pos + copied;
            let block = absolute / CACHE_BLOCK_SIZE;
            let in_block = absolute % CACHE_BLOCK_SIZE;
            let remaining = buf.len() - copied;
            if let Some(index) = dev.cache_slot(block) {
                #[cfg(feature = "lock-stats")]
                {
                    dev.cache_hits += 1;
                }
                let chunk = remaining.min(CACHE_BLOCK_SIZE - in_block);
                #[cfg(feature = "lock-stats")]
                {
                    dev.read_cache_hit_ops += 1;
                    dev.read_cache_hit_bytes += chunk as u64;
                    if dev.cache[index].prefetched {
                        dev.readahead_hits += 1;
                    }
                }
                dev.cache[index].prefetched = false;
                buf[copied..copied + chunk]
                    .copy_from_slice(&dev.cache[index].data[in_block..in_block + chunk]);
                if in_block == 0 && chunk == CACHE_BLOCK_SIZE {
                    dev.observe_aligned_read(block, 1);
                }
                let stamp = dev.next_stamp();
                dev.cache[index].stamp = stamp;
                dev.cache[index].referenced = true;
                copied += chunk;
                continue;
            }
            if in_block == 0 && remaining >= CACHE_BLOCK_SIZE {
                let mut run_blocks =
                    (remaining / CACHE_BLOCK_SIZE).min(MAX_BULK_IO_SIZE / CACHE_BLOCK_SIZE);
                for offset in 1..run_blocks {
                    if dev.cache_slot(block + offset).is_some() {
                        run_blocks = offset;
                        break;
                    }
                }
                let sequential = dev.observe_aligned_read(block, run_blocks);
                if run_blocks == 1 && sequential {
                    if let Some(index) = dev.prefetch_read_run(block) {
                        buf[copied..copied + CACHE_BLOCK_SIZE]
                            .copy_from_slice(&dev.cache[index].data);
                        copied += CACHE_BLOCK_SIZE;
                        continue;
                    }
                }
                let run_len = run_blocks * CACHE_BLOCK_SIZE;
                #[cfg(feature = "lock-stats")]
                {
                    dev.read_direct_runs += 1;
                    dev.read_direct_bytes += run_len as u64;
                }
                dev.inner.read_block(
                    block * CACHE_SECTOR_COUNT,
                    &mut buf[copied..copied + run_len],
                );
                copied += run_len;
                continue;
            }
            #[cfg(feature = "lock-stats")]
            {
                dev.read_partial_fills += 1;
                dev.read_partial_bytes += remaining.min(CACHE_BLOCK_SIZE - in_block) as u64;
            }
            let index = dev.prepare_slot(block, true).ok_or(-1)?;
            let chunk = remaining.min(CACHE_BLOCK_SIZE - in_block);
            buf[copied..copied + chunk]
                .copy_from_slice(&dev.cache[index].data[in_block..in_block + chunk]);
            copied += chunk;
        }
        dev.pos = end;
        #[cfg(feature = "mount-trace")]
        if trace {
            crate::println!(
                "[mount-trace] test-disk-read-done pos={} len={}",
                pos,
                buf.len()
            );
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

    fn flush(dev: &mut Self::DevType) -> Result<usize, i32> {
        dev.flush_dirty_cache();
        dev.inner.flush();
        #[cfg(feature = "lock-stats")]
        crate::println!(
            "[block-cache-stats] slots={} buckets={} hits={} misses={} dirty_evictions={} threshold_flush_blocks={} explicit_flush_blocks={} dirty_peak={} replacement_scans={} clean_replacements={} writeback_runs={}",
            dev.cache.len(),
            dev.cache_hash_heads.len(),
            dev.cache_hits,
            dev.cache_misses,
            dev.dirty_evictions,
            dev.threshold_flush_blocks,
            dev.explicit_flush_blocks,
            dev.dirty_peak,
            dev.replacement_scans,
            dev.clean_replacements,
            dev.writeback_runs,
        );
        #[cfg(feature = "lock-stats")]
        crate::println!(
            "[block-cache-read-stats] hit_ops={} hit_bytes={} partial_fills={} partial_bytes={} direct_runs={} direct_bytes={} write_rmw_fills={} readahead_runs={} readahead_blocks={} readahead_hits={}",
            dev.read_cache_hit_ops,
            dev.read_cache_hit_bytes,
            dev.read_partial_fills,
            dev.read_partial_bytes,
            dev.read_direct_runs,
            dev.read_direct_bytes,
            dev.write_rmw_fills,
            dev.readahead_runs,
            dev.readahead_blocks,
            dev.readahead_hits,
        );
        Ok(0)
    }
}

// ==================== Ext4Wrapper (Send+Sync) ====================

pub struct Ext4Wrapper(pub Ext4BlockWrapper<BlockDeviceStreamOp>);

unsafe impl Send for Ext4Wrapper {}
unsafe impl Sync for Ext4Wrapper {}
