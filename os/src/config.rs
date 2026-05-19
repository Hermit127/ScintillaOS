#[allow(unused)]

pub const USER_STACK_SIZE: usize = 4096 * 64;
pub const KERNEL_STACK_SIZE: usize = 4096 * 64;
pub const KERNEL_HEAP_SIZE: usize = 0x200_0000;

pub const PAGE_SIZE: usize = 0x1000;
