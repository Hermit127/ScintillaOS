// 这里负责操作系统的块设备驱动接口：定义块设备统一接口，文件系统只依赖这个 trait。
pub trait BlockDevice: Send + Sync {
    // 从指定块号开始读取一段扇区对齐的数据。
    // 执行时由具体设备实现把 block_id 对应的块数据填进 buf，调用者负责保证缓冲区大小合适。
    fn read_block(&self, block_id: usize, buf: &mut [u8]);
    // 从指定块号开始写入一段扇区对齐的数据。
    // 执行时由具体设备实现把 buf 里的数据写到 block_id 对应的块位置。
    fn write_block(&self, block_id: usize, buf: &[u8]);
    /// Complete all writes submitted before this call.
    fn flush(&self) {}
    // 返回设备容量；不知道容量的设备可以返回 None。
    // 执行时默认直接返回 None，支持容量查询的设备可以覆盖这个实现。
    fn capacity_bytes(&self) -> Option<u64> {
        None
    }
}
