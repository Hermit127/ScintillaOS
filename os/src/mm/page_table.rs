// 这里负责操作系统的内存管理：封装用户指针到内核引用、字符串和缓冲区的转换。
use _core::slice;
use _core::str::from_utf8_unchecked;
use alloc::string::{String, ToString};
use bitflags::*;
use log::*;
use polyhal::pagetable::PageTable;
use polyhal::VirtAddr;
bitflags! {
    pub struct PTEFlags: u8 {
        const V = 1 << 0;
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
        const G = 1 << 5;
        const A = 1 << 6;
        const D = 1 << 7;
    }
}

// 把用户传进来的字节缓冲区视作内核可写切片。
// 执行时直接用用户虚拟地址构造可变切片；调用方必须已经校验过这段用户内存可写。
pub fn translated_byte_buffer(_token: PageTable, ptr: *mut u8, len: usize) -> &'static mut [u8] {
    trace!("os::mm::page_table::translated_byte_buffer");
    unsafe { core::slice::from_raw_parts_mut(ptr, len) }
}

// 从用户指针开始数到结尾的 0 字节。
// 执行时逐字节解引用 ptr.add(i)，直到遇到 0；这是 unsafe，因为它假设调用者传入了有效 C 字符串。
unsafe fn str_len(ptr: *const u8) -> usize {
    let mut i = 0;
    loop {
        if *ptr.add(i) == 0 {
            break i;
        }
        i += 1;
    }
}

// 从用户地址读取 C 字符串，返回时不带结尾的 0。
// 执行时先用 str_len 找长度，再把这段用户字节按 UTF-8 构造成 String。
pub fn translated_str(_token: PageTable, ptr: *const u8) -> String {
    unsafe {
        let len = str_len(ptr);
        from_utf8_unchecked(slice::from_raw_parts(ptr, len)).to_string()
    }
}

// 把用户只读指针转成内核引用。
// 执行时直接把裸指针转成共享引用；调用方必须保证指针非空、已映射且类型 T 对齐正确。
pub fn translated_ref<T>(_token: PageTable, ptr: *const T) -> &'static T {
    unsafe { ptr.as_ref().unwrap() }
}

// 把用户可写指针转成内核可变引用。
// 执行时直接把裸指针转成可变引用；调用方必须保证这段用户内存可写且没有别名冲突。
pub fn translated_refmut<T>(_token: PageTable, ptr: *mut T) -> &'static mut T {
    unsafe { ptr.as_mut().unwrap() }
}
