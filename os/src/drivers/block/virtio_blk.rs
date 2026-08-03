use core::ptr::NonNull;
#[cfg(feature = "mount-trace")]
use core::sync::atomic::{AtomicUsize, Ordering};

use super::BlockDevice;
use crate::mm::{frames_alloc, FrameTracker};
use crate::sync::DeviceLock;
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
#[cfg(feature = "mount-trace")]
static MOUNT_TRACE_CAPACITY_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp-diagnostics")]
const RUSTC_DRIVER_TRACE_SECTOR: usize = 715_880 * (PAGE_SIZE / SECTOR_SIZE);

#[cfg(not(target_arch = "loongarch64"))]
#[repr(C)]
pub struct VirtIOBlock(
    DeviceLock<VirtIOBlk<VirtioHal, MmioTransport>>,
    DeviceLock<[u8; DMA_BOUNCE_SIZE]>,
);
#[cfg(target_arch = "loongarch64")]
#[repr(C)]
pub struct VirtIOBlock(
    DeviceLock<VirtIOBlk<VirtioHal, PciTransport>>,
    DeviceLock<[u8; DMA_BOUNCE_SIZE]>,
);

lazy_static! {
    // DMA frames are global to the VirtIO HAL, so their bookkeeping needs its
    // own device-domain lock as soon as more than one hart can initialize or
    // drive a VirtIO queue.
    static ref QUEUE_FRAMES: DeviceLock<Vec<FrameTracker>> =
        DeviceLock::new_named(Vec::new(), "VirtIODmaFrames");
}

unsafe impl Sync for VirtIOBlock {}
unsafe impl Send for VirtIOBlock {}

impl BlockDevice for VirtIOBlock {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        // Allocation is deliberately outside the device lock.  A future
        // allocator implementation may take other locks or re-enter kernel
        // code, both forbidden while a DeviceLock is held.
        let mut blk = self.0.exclusive_access();
        let mut bounce = self.1.exclusive_access();
        let mut done = 0;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(DMA_BOUNCE_SIZE);
            #[cfg(feature = "smp-diagnostics")]
            let watched_before_dma = crate::mm::watched_frame_is_intact();
            blk.read_blocks(block_id + done / SECTOR_SIZE, &mut bounce[..chunk])
                .expect("Error when reading VirtIOBlk");
            #[cfg(feature = "smp-diagnostics")]
            if watched_before_dma && !crate::mm::watched_frame_is_intact() {
                crate::println!(
                    "[frame-diag] watched page changed during virtio DMA read block={} len={}",
                    block_id + done / SECTOR_SIZE,
                    chunk,
                );
            }
            #[cfg(feature = "smp-diagnostics")]
            {
                let first_sector = block_id + done / SECTOR_SIZE;
                let sector_count = chunk / SECTOR_SIZE;
                if first_sector <= RUSTC_DRIVER_TRACE_SECTOR
                    && RUSTC_DRIVER_TRACE_SECTOR < first_sector + sector_count
                {
                    let offset = (RUSTC_DRIVER_TRACE_SECTOR - first_sector) * SECTOR_SIZE;
                    let head = unsafe {
                        core::ptr::read_unaligned(bounce[offset..].as_ptr() as *const u32)
                    };
                    crate::println!(
                        "[block-diag] driver-sector dev={:#x} sector={} head={:#010x}",
                        self as *const Self as usize,
                        RUSTC_DRIVER_TRACE_SECTOR,
                        head,
                    );
                }
            }
            #[cfg(feature = "smp-diagnostics")]
            let watched_before_copy = crate::mm::watched_frame_is_intact();
            buf[done..done + chunk].copy_from_slice(&bounce[..chunk]);
            #[cfg(feature = "smp-diagnostics")]
            if watched_before_copy && !crate::mm::watched_frame_is_intact() {
                crate::println!(
                    "[frame-diag] watched page changed during virtio bounce copy block={} len={} dst={:#x}",
                    block_id + done / SECTOR_SIZE,
                    chunk,
                    buf[done..].as_ptr() as usize,
                );
            }
            done += chunk;
        }
    }
    fn write_block(&self, block_id: usize, buf: &[u8]) {
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        // See read_block: never allocate while the device lock is held.
        let mut blk = self.0.exclusive_access();
        let mut bounce = self.1.exclusive_access();
        let mut done = 0;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(DMA_BOUNCE_SIZE);
            bounce[..chunk].copy_from_slice(&buf[done..done + chunk]);
            blk.write_blocks(block_id + done / SECTOR_SIZE, &bounce[..chunk])
                .expect("Error when writing VirtIOBlk");
            done += chunk;
        }
    }

    fn capacity_bytes(&self) -> Option<u64> {
        #[cfg(feature = "mount-trace")]
        {
            let sequence = MOUNT_TRACE_CAPACITY_COUNT.fetch_add(1, Ordering::Relaxed);
            crate::println!(
                "[mount-trace] virtio-capacity-enter seq={} dev={:#x}",
                sequence,
                self as *const Self as usize
            );
        }
        let blk = self.0.exclusive_access();
        let capacity = (blk.capacity() as u64) * (SECTOR_SIZE as u64);
        #[cfg(feature = "mount-trace")]
        crate::println!(
            "[mount-trace] virtio-capacity-done dev={:#x} bytes={}",
            self as *const Self as usize,
            capacity
        );
        Some(capacity)
    }
}

impl VirtIOBlock {
    #[allow(unused)]
    pub fn new() -> Self {
        Self::new_at(VIRTIO0)
    }

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
            Self(
                DeviceLock::new_named(blk, "VirtIOBlock"),
                DeviceLock::new_named([0u8; DMA_BOUNCE_SIZE], "VirtIOBounce"),
            )
        }
    }

    #[cfg(target_arch = "loongarch64")]
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
                return Self(
                    unsafe {
                        DeviceLock::new_named(
                        VirtIOBlk::<VirtioHal, PciTransport>::new(transport).unwrap(),
                        "VirtIOBlock",
                        )
                    },
                    DeviceLock::new_named([0u8; DMA_BOUNCE_SIZE], "VirtIOBounce"),
                );
            }
            block_index += 1;
        }
        panic!("virtio pci block device not found");
    }

    #[cfg(target_arch = "loongarch64")]
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
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (usize, NonNull<u8>) {
        // let mut ppn_base = PhysPage::new(0);
        // let mut paddr = PhysAddr::new(0);
        // for i in 0..pages {
        //     let frame = frame_alloc().unwrap();
        //     debug!("alloc paddr: {:?}", frame);
        //     if i == 0 {
        //         paddr = frame.paddr;
        //     };
        //     assert_eq!(frame.paddr.raw(), paddr.raw() + PAGE_SIZE);
        //     QUEUE_FRAMES.exclusive_access().push(frame);
        // }
        // unsafe { (paddr.raw(), NonNull::new_unchecked(paddr.get_mut_ptr())) }
        for _ in 0..16 {
            let Some(frames) = frames_alloc(pages) else {
                return (0, NonNull::dangling());
            };
            let paddr = frames[0].paddr;
            if paddr.raw() != 0 {
                QUEUE_FRAMES.exclusive_access().extend(frames);
                return unsafe { (paddr.raw(), NonNull::new_unchecked(paddr.get_mut_ptr())) };
            }
        }
        (0, NonNull::dangling())
    }

    unsafe fn dma_dealloc(paddr: usize, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        // `dma_alloc` transfers FrameTracker ownership into QUEUE_FRAMES.
        // Returning the pages directly here while those trackers remain alive
        // lets the buddy allocator hand active DMA/page-table frames to a
        // second owner. Remove the exact allocation and drop it after the
        // device lock has been released instead.
        let mut retired = Vec::with_capacity(pages);
        {
            let mut allocated = QUEUE_FRAMES.exclusive_access();
            let Some(start) = allocated
                .iter()
                .position(|frame| frame.paddr.raw() == paddr)
            else {
                return -1;
            };
            let Some(end) = start.checked_add(pages) else {
                return -1;
            };
            if end > allocated.len()
                || allocated[start..end]
                    .iter()
                    .enumerate()
                    .any(|(index, frame)| frame.paddr.raw() != paddr + index * PAGE_SIZE)
            {
                return -1;
            }
            retired.extend(allocated.drain(start..end));
        }

        drop(retired);
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: usize, _size: usize) -> NonNull<u8> {
        NonNull::new((usize::from(paddr) | VIRT_ADDR_START) as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> usize {
        buffer.as_ptr() as *mut u8 as usize - VIRT_ADDR_START
        // let pt = PageTable::current();
        // let paddr = pt.translate(VirtAddr::new(buffer.as_ptr() as *const u8 as usize)).expect("can't find vaddr").0;
        // paddr.addr()
    }

    unsafe fn unshare(_paddr: usize, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
        // Nothing to do, as the host already has access to all memory and we didn't copy the buffer
        // anywhere else.
    }
}

/// Probe all virtio-mmio addresses (matching xiande-OS). Returns devices in bus order.
#[cfg(not(target_arch = "loongarch64"))]
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
        // Use virtual address (physical | VIRT_ADDR_START) for MMIO access
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
        let dev: Arc<dyn BlockDevice> =
            Arc::new(VirtIOBlock(
                DeviceLock::new_named(blk, "VirtIOBlock"),
                DeviceLock::new_named([0u8; DMA_BOUNCE_SIZE], "VirtIOBounce"),
            ));
        crate::println!("[virtio-blk] probe {:#x}: OK (device #{})", base, out.len());
        out.push(dev);
    }
    crate::println!("[virtio-blk] probe_all: found {} devices", out.len());
    out
}

/// Probe all virtio PCI block devices on LoongArch. Returns devices in PCI order.
#[cfg(target_arch = "loongarch64")]
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
        out.push(
            Arc::new(VirtIOBlock(
                DeviceLock::new_named(blk, "VirtIOBlock"),
                DeviceLock::new_named([0u8; DMA_BOUNCE_SIZE], "VirtIOBounce"),
            )) as Arc<dyn BlockDevice>,
        );
    }
    crate::println!("[virtio-blk] probe_all: found {} devices", out.len());
    out
}
