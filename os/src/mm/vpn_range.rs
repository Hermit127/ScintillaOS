// 这里负责操作系统的内存管理：定义按页步进的虚拟地址范围和迭代器。
use polyhal::{pagetable::PAGE_SIZE, VirtAddr};

#[derive(Copy, Clone, Debug)]
pub struct VAddrRange {
    l: VirtAddr,
    r: VirtAddr,
}
impl VAddrRange {
    // 创建一个左闭右开的虚拟地址范围。
    // 执行时先断言 start 不大于 end，再保存左右边界。
    pub fn new(start: VirtAddr, end: VirtAddr) -> Self {
        assert!(start <= end, "start {:?} > end {:?}!", start, end);
        Self { l: start, r: end }
    }
    // 返回这个虚拟地址范围的起点，调用者用它计算遍历或映射边界。
    // 执行时直接返回保存的左边界 l。
    pub fn get_start(&self) -> VirtAddr {
        self.l
    }
    // 返回这个虚拟地址范围的终点，范围本身按左闭右开处理。
    // 执行时直接返回保存的右边界 r。
    pub fn get_end(&self) -> VirtAddr {
        self.r
    }
}
impl IntoIterator for VAddrRange {
    type Item = VirtAddr;
    type IntoIter = SimpleRangeIterator;
    // 把地址范围变成按页步进的迭代器。
    // 执行时用当前范围的左右边界创建 SimpleRangeIterator。
    fn into_iter(self) -> Self::IntoIter {
        SimpleRangeIterator::new(self.l, self.r)
    }
}
pub struct SimpleRangeIterator {
    current: VirtAddr,
    end: VirtAddr,
}
impl SimpleRangeIterator {
    // 创建一个从 l 走到 r 的页粒度迭代器。
    // 执行时把 current 初始化为 l，把 end 初始化为 r。
    pub fn new(l: VirtAddr, r: VirtAddr) -> Self {
        Self { current: l, end: r }
    }
}
impl Iterator for SimpleRangeIterator {
    type Item = VirtAddr;
    // 返回当前页地址，并把迭代器推进一页。
    // 执行时如果 current 已到 end 就结束，否则返回 current 并向后推进一个 PAGE_SIZE。
    fn next(&mut self) -> Option<Self::Item> {
        if self.current == self.end {
            None
        } else {
            let t = self.current;
            self.current = self.current + PAGE_SIZE;
            Some(t)
        }
    }
}
