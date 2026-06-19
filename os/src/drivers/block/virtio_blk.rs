// 这里负责操作系统的块设备驱动：实现 VirtIO 块设备驱动和 virtio-drivers 需要的 HAL 适配。
use core::ptr::NonNull;

use super::BlockDevice;
use crate::mm::{frame_dealloc, frames_alloc, FrameTracker};
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;
use polyhal::consts::VIRT_ADDR_START;
use polyhal::pagetable::PAGE_SIZE;
use polyhal::PhysAddr;
use virtio_drivers::device::blk::{VirtIOBlk, SECTOR_SIZE};
#[cfg(not(target_arch = "loongarch64"))]
use virtio_drivers::transport::mmio::MmioTransport;
#[cfg(target_arch = "loongarch64")]
use virtio_drivers::transport::{
    pci::{
        bus::{BarInfo, Cam, Command, MemoryBarType, PciRoot},
        virtio_device_type, PciTransport,
    },
    DeviceType,
};
use virtio_drivers::{BufferDirection, Hal};

#[allow(unused)]
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
const VIRTIO0: PhysAddr = polyhal::pa!(0x10001000);

#[allow(unused)]
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
const VIRTIO1: PhysAddr = polyhal::pa!(0x10002000);

#[cfg(target_arch = "aarch64")]
const VIRTIO0: PhysAddr = polyhal::pa!(0xa00_0000);

#[cfg(target_arch = "loongarch64")]
const PCI_ECAM_BASE: PhysAddr = polyhal::pa!(0x20000000);

const DMA_BOUNCE_SIZE: usize = 4096;

#[cfg(not(target_arch = "loongarch64"))]
pub struct VirtIOBlock(UPSafeCell<VirtIOBlk<VirtioHal, MmioTransport>>);
#[cfg(target_arch = "loongarch64")]
pub struct VirtIOBlock(UPSafeCell<VirtIOBlk<VirtioHal, PciTransport>>);

lazy_static! {
    static ref QUEUE_FRAMES: UPSafeCell<Vec<FrameTracker>> = unsafe { UPSafeCell::new(Vec::new()) };
}

unsafe impl Sync for VirtIOBlock {}
unsafe impl Send for VirtIOBlock {}

impl BlockDevice for VirtIOBlock {
    // 通过 VirtIO 块设备读取扇区，分块走 bounce buffer。
    // 执行时先确认缓冲区按扇区对齐，再每次读一段到 bounce buffer，最后复制进调用者缓冲区。
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        let mut blk = self.0.exclusive_access();
        let mut done = 0;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(DMA_BOUNCE_SIZE);
            let mut bounce = alloc::vec![0u8; chunk];
            blk.read_blocks(block_id + done / SECTOR_SIZE, &mut bounce)
                .expect("Error when reading VirtIOBlk");
            buf[done..done + chunk].copy_from_slice(&bounce);
            done += chunk;
        }
    }
    // 通过 VirtIO 块设备写入扇区，分块走 bounce buffer。
    // 执行时先确认缓冲区按扇区对齐，再把每段数据复制进 bounce buffer 后提交给 VirtIO 设备。
    fn write_block(&self, block_id: usize, buf: &[u8]) {
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        let mut blk = self.0.exclusive_access();
        let mut done = 0;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(DMA_BOUNCE_SIZE);
            let mut bounce = alloc::vec![0u8; chunk];
            bounce.copy_from_slice(&buf[done..done + chunk]);
            blk.write_blocks(block_id + done / SECTOR_SIZE, &bounce)
                .expect("Error when writing VirtIOBlk");
            done += chunk;
        }
    }

    // 按扇区数换算成字节容量。
    // 执行时读取 VirtIO 设备报告的扇区数，乘以扇区大小后返回字节容量。
    fn capacity_bytes(&self) -> Option<u64> {
        let blk = self.0.exclusive_access();
        Some((blk.capacity() as u64) * (SECTOR_SIZE as u64))
    }
}

impl VirtIOBlock {
    #[allow(unused)]
    // 创建默认地址上的 VirtIO 块设备。
    // 执行时直接调用 new_at，把默认 VirtIO0 地址交给统一初始化路径。
    pub fn new() -> Self {
        Self::new_at(VIRTIO0)
    }

    // 按指定 MMIO 地址或 LoongArch PCI 索引创建块设备。
    // 执行时按架构选择 PCI 或 MMIO 初始化；MMIO 路径把物理地址转成 transport 后创建 VirtIOBlk。
    pub fn new_at(addr: PhysAddr) -> Self {
        #[cfg(target_arch = "loongarch64")]
        {
            let index = if addr == VIRTIO1 { 1 } else { 0 };
            return Self::new_pci(index);
        }
        #[cfg(not(target_arch = "loongarch64"))]
        unsafe {
            let transport = MmioTransport::new(NonNull::new_unchecked(addr.get_mut_ptr()))
                .expect("this is not a valid virtio device");
            let blk = VirtIOBlk::<VirtioHal, MmioTransport>::new(transport).unwrap();
            let capacity = blk.capacity();
            crate::println!(
                "[virtio-blk] online at {:#x}, capacity={} sectors",
                addr.raw(),
                capacity
            );
            Self(UPSafeCell::new(blk))
        }
    }

    #[cfg(target_arch = "loongarch64")]
    // LoongArch 下按 PCI 枚举顺序创建第 index 个 VirtIO 块设备。
    // 执行时枚举 PCI 总线，跳过非块设备，命中目标序号后分配 BAR、打开总线主控并创建 VirtIOBlk。
    fn new_pci(index: usize) -> Self {
        let mut root = unsafe {
            PciRoot::new(
                (PCI_ECAM_BASE.raw() | VIRT_ADDR_START) as *mut u8,
                Cam::Ecam,
            )
        };
        let mut block_index = 0;
        for (device_function, info) in root.enumerate_bus(0) {
            if virtio_device_type(&info) != Some(DeviceType::Block) {
                continue;
            }
            if block_index == index {
                let (_, command) = root.get_status_command(device_function);
                Self::alloc_pci_bars(&mut root, device_function, index);
                root.set_command(
                    device_function,
                    command | Command::MEMORY_SPACE | Command::BUS_MASTER,
                );
                let transport = PciTransport::new::<VirtioHal>(&mut root, device_function)
                    .expect("this is not a valid virtio pci block device");
                return Self(unsafe {
                    UPSafeCell::new(VirtIOBlk::<VirtioHal, PciTransport>::new(transport).unwrap())
                });
            }
            block_index += 1;
        }
        panic!("virtio pci block device not found");
    }

    #[cfg(target_arch = "loongarch64")]
    // 给 LoongArch PCI VirtIO 设备分配 BAR 地址。
    // 执行时遍历 6 个 BAR，给还没分配地址的内存 BAR 按大小对齐分配到预留 PCI MMIO 区间。
    fn alloc_pci_bars(
        root: &mut PciRoot,
        device_function: virtio_drivers::transport::pci::bus::DeviceFunction,
        index: usize,
    ) {
        let mut next = 0x4000_0000u64 + index as u64 * 0x10_0000;
        let mut bar_index = 0u8;
        while bar_index < 6 {
            let info = root.bar_info(device_function, bar_index).unwrap();
            let takes_two = info.takes_two_entries();
            if let BarInfo::Memory {
                address_type,
                address,
                size,
                ..
            } = info
            {
                if address == 0 && size != 0 {
                    let align = size as u64;
                    next = (next + align - 1) & !(align - 1);
                    match address_type {
                        MemoryBarType::Width64 => root.set_bar_64(device_function, bar_index, next),
                        _ => root.set_bar_32(device_function, bar_index, next as u32),
                    }
                    next += align;
                }
            }
            bar_index += if takes_two { 2 } else { 1 };
        }
    }
}

pub struct VirtioHal;

unsafe impl Hal for VirtioHal {
    // 为 VirtIO 队列分配连续 DMA 页。
    // 执行时向物理页分配器申请连续页，把页帧保存到 QUEUE_FRAMES 防止释放，再返回设备可见物理地址。
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (usize, NonNull<u8>) {
        let frames = frames_alloc(pages).unwrap();
        let paddr = frames[0].paddr;
        QUEUE_FRAMES.exclusive_access().extend(frames);
        unsafe { (paddr.raw(), NonNull::new_unchecked(paddr.get_mut_ptr())) }
    }

    // 释放 VirtIO 归还的 DMA 页。
    // 执行时从起始物理地址开始逐页调用 frame_dealloc，按 virtio-drivers 约定返回 0 表示成功。
    unsafe fn dma_dealloc(paddr: usize, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        log::error!("dealloc paddr: {:?}", paddr);
        let mut pa = PhysAddr::new(paddr);
        for _ in 0..pages {
            frame_dealloc(pa);
            pa = pa + PAGE_SIZE;
        }
        0
    }

    // 把 MMIO 物理地址转成内核高半区虚拟地址。
    // 执行时给物理地址加上内核高半区偏移，并包装成 NonNull 指针交给 VirtIO 驱动访问寄存器。
    unsafe fn mmio_phys_to_virt(paddr: usize, _size: usize) -> NonNull<u8> {
        NonNull::new((usize::from(paddr) | VIRT_ADDR_START) as *mut u8).unwrap()
    }

    // 把内核缓冲区地址转换成设备可见的物理地址。
    // 执行时把高半区虚拟地址减去偏移得到物理地址，让设备能通过 DMA 访问同一段内存。
    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> usize {
        buffer.as_ptr() as *mut u8 as usize - VIRT_ADDR_START
    }

    // 当前没有额外拷贝缓冲区，所以解除共享时无需动作。
    // 执行时不做任何事情，因为 share 没有创建临时映射或 bounce buffer。
    unsafe fn unshare(_paddr: usize, _buffer: NonNull<[u8]>, _direction: BufferDirection) {}
}

// 探测所有 VirtIO-MMIO 地址，并按总线顺序返回块设备。
#[cfg(not(target_arch = "loongarch64"))]
// 执行时逐个读取候选 MMIO 地址的 magic 和 device_id，确认是块设备后创建 transport 与 VirtIOBlk。
pub fn probe_all() -> Vec<Arc<dyn BlockDevice>> {
    const BASES: &[usize] = &[
        0x1000_1000,
        0x1000_2000,
        0x1000_3000,
        0x1000_4000,
        0x1000_5000,
        0x1000_6000,
        0x1000_7000,
        0x1000_8000,
    ];
    let mut out = Vec::new();
    for &base in BASES {
        // MMIO 访问用物理地址加内核高半区偏移。
        let va = base | VIRT_ADDR_START;
        let magic = unsafe { core::ptr::read_volatile(va as *const u32) };
        crate::println!(
            "[virtio-blk] probe {:#x} (va={:#x}): magic={:#x}",
            base,
            va,
            magic
        );
        if magic != 0x7472_6976 {
            continue;
        }
        let device_id = unsafe { core::ptr::read_volatile((va + 0x08) as *const u32) };
        crate::println!("[virtio-blk] probe {:#x}: device_id={}", base, device_id);
        if device_id != 2 {
            continue;
        }
        let header = match unsafe { NonNull::new(va as *mut u8) } {
            Some(h) => h,
            None => continue,
        };
        let transport = match unsafe { MmioTransport::new(header.cast()) } {
            Ok(t) => t,
            Err(e) => {
                crate::println!("[virtio-blk] probe {:#x}: transport err {:?}", base, e);
                continue;
            }
        };
        let blk = match VirtIOBlk::<VirtioHal, MmioTransport>::new(transport) {
            Ok(b) => b,
            Err(e) => {
                crate::println!("[virtio-blk] probe {:#x}: blk err {:?}", base, e);
                continue;
            }
        };
        let dev: Arc<dyn BlockDevice> = Arc::new(VirtIOBlock(unsafe { UPSafeCell::new(blk) }));
        crate::println!("[virtio-blk] probe {:#x}: OK (device #{})", base, out.len());
        out.push(dev);
    }
    crate::println!("[virtio-blk] probe_all: found {} devices", out.len());
    out
}

// LoongArch 下探测所有 VirtIO PCI 块设备，并按 PCI 枚举顺序返回。
#[cfg(target_arch = "loongarch64")]
// 执行时枚举 PCI 总线中的 VirtIO 块设备，分配 BAR 并创建块设备对象，最后返回全部结果。
pub fn probe_all() -> Vec<Arc<dyn BlockDevice>> {
    let mut root = unsafe {
        PciRoot::new(
            (PCI_ECAM_BASE.raw() | VIRT_ADDR_START) as *mut u8,
            Cam::Ecam,
        )
    };
    let mut out = Vec::new();
    for (device_function, info) in root.enumerate_bus(0) {
        if virtio_device_type(&info) != Some(DeviceType::Block) {
            continue;
        }
        let (_, command) = root.get_status_command(device_function);
        VirtIOBlock::alloc_pci_bars(&mut root, device_function, out.len());
        root.set_command(
            device_function,
            command | Command::MEMORY_SPACE | Command::BUS_MASTER,
        );
        let transport = match PciTransport::new::<VirtioHal>(&mut root, device_function) {
            Ok(transport) => transport,
            Err(e) => {
                crate::println!(
                    "[virtio-blk] pci {:?}: transport err {:?}",
                    device_function,
                    e
                );
                continue;
            }
        };
        let blk = match VirtIOBlk::<VirtioHal, PciTransport>::new(transport) {
            Ok(blk) => blk,
            Err(e) => {
                crate::println!("[virtio-blk] pci {:?}: blk err {:?}", device_function, e);
                continue;
            }
        };
        let capacity = blk.capacity();
        crate::println!(
            "[virtio-blk] pci {:?}: OK, capacity={} sectors",
            device_function,
            capacity
        );
        out.push(Arc::new(VirtIOBlock(unsafe { UPSafeCell::new(blk) })) as Arc<dyn BlockDevice>);
    }
    crate::println!("[virtio-blk] probe_all: found {} devices", out.len());
    out
}
