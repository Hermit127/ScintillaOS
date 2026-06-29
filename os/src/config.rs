// 这里负责操作系统的基础配置：集中放内核和用户空间共用的基础配置常量。
#[allow(unused)]

pub const USER_STACK_SIZE: usize = 4096 * 256;
pub const KERNEL_STACK_SIZE: usize = 4096 * 4;
pub const KERNEL_HEAP_SIZE: usize = 0xC00_0000;

pub const PAGE_SIZE: usize = 0x1000;
