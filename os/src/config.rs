#[allow(unused)]

pub const USER_STACK_SIZE: usize = 4096 * 256;
pub const KERNEL_STACK_SIZE: usize = 4096 * 8;
pub const KERNEL_HEAP_SIZE: usize = 0x800_0000;

pub const PAGE_SIZE: usize = 0x1000;
