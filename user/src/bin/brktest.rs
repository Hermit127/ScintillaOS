#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib; // 注意这里的名字要和你的用户库 crate 名字一致

use user_lib::brk;

#[no_mangle]
pub fn main() -> i32 {
    println!("--- Test brk start ---");

    // 1. 探测初始堆顶
    let start_brk = brk(0) as usize;
    println!("Initial brk: {:#x}", start_brk);
    assert!(start_brk > 0, "brk(0) should return a valid address");

    // 2. 向上扩展堆（分配 4KB = 1个页面）
    let page_size = 4096;
    let new_brk_target = start_brk + page_size;
    let new_brk_actual = brk(new_brk_target) as usize;

    println!("Allocated 1 page. New brk: {:#x}", new_brk_actual);
    assert_eq!(
        new_brk_actual, new_brk_target,
        "brk failed to allocate memory"
    );

    // 3. 内存映射验证（核心！）
    // 如果你在内核里的 MapArea 和页表映射写错了，这里写内存时就会触发 StorePageFault 崩溃。
    println!("Testing memory read/write permissions...");
    let ptr = start_brk as *mut u8;
    unsafe {
        // 向新分配的页面第一字节写入数据
        *ptr = 0x42;
        // 读出来校验
        assert_eq!(*ptr, 0x42, "Memory write/read validation failed");

        // 顺便测试一下页面末尾
        let ptr_end = (new_brk_actual - 1) as *mut u8;
        *ptr_end = 0x99;
        assert_eq!(
            *ptr_end, 0x99,
            "Memory write/read validation failed at page end"
        );
    }
    println!("Memory R/W works perfectly!");

    // 4. 收缩堆（释放内存）
    let shrink_brk = brk(start_brk) as usize;
    println!("Shrink brk back to: {:#x}", shrink_brk);
    assert_eq!(shrink_brk, start_brk, "brk failed to shrink memory");

    println!("--- Test brk passed! ---");
    0
}
