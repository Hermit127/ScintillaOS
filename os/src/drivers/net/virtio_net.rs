// 这里负责操作系统的网络设备驱动：实现 VirtIO 网卡到 smoltcp Device trait 的适配。
use core::ptr::NonNull;
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::drivers::block::VirtioHal;
use crate::sync::UPSafeCell;
use polyhal::consts::VIRT_ADDR_START;
use virtio_drivers::device::net::VirtIONet;
use virtio_drivers::transport::mmio::MmioTransport;

#[cfg(target_arch = "riscv64")]
const VIRTIO_NET_BASES: &[usize] = &[
    0x1000_1000,
    0x1000_2000,
    0x1000_3000,
    0x1000_4000,
    0x1000_5000,
    0x1000_6000,
    0x1000_7000,
    0x1000_8000,
];

#[cfg(target_arch = "aarch64")]
const VIRTIO_NET_BASES: &[usize] = &[0xa00_0300];

const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
const VIRTIO_NET_DEVICE_ID: u32 = 1;

const NET_BUF_SIZE: usize = 1536; // must be multiple of 8 for RxBuffer alignment
const QUEUE_SIZE: usize = 64;

struct VirtIONetInner {
    net: VirtIONet<VirtioHal, MmioTransport, QUEUE_SIZE>,
}

unsafe impl Send for VirtIONetInner {}
unsafe impl Sync for VirtIONetInner {}

pub struct VirtIONetAdapter {
    inner: UPSafeCell<VirtIONetInner>,
    rx_buf: [u8; NET_BUF_SIZE],
    rx_len: usize,
    tx_buf: [u8; NET_BUF_SIZE],
    // 本地回环队列用于把发给自己的以太网帧重新送回 receive。
    loopback_queue: UPSafeCell<VecDeque<Vec<u8>>>,
    mac_addr: [u8; 6],
}

impl VirtIONetAdapter {
    // 探测并初始化第一个 VirtIO net 设备。
    // 执行时先找到 MMIO 设备地址，创建 transport 和 VirtIONet，再读取 MAC 并组装适配器状态。
    pub fn new() -> Option<Self> {
        let (base, ptr) = find_net_mmio()?;
        println!("[net] probing VirtIO net at {:#x}", base);
        let header = unsafe { NonNull::new_unchecked(ptr).cast() };
        let transport = match unsafe { MmioTransport::new(header) } {
            Ok(t) => t,
            Err(e) => {
                println!("[net] MmioTransport::new failed at {:#x}: {:?}", base, e);
                return None;
            }
        };
        let net = match VirtIONet::new(transport, NET_BUF_SIZE) {
            Ok(n) => n,
            Err(e) => {
                println!("[net] VirtIONet::new failed at {:#x}: {:?}", base, e);
                return None;
            }
        };

        // 读取 MAC 地址；设备没给时用一个固定本地地址。
        let mut mac = net.mac_address();
        if mac == [0u8; 6] {
            mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        }

        println!(
            "[net] VirtIONetAdapter initialized successfully, MAC={:02x?}",
            mac
        );
        Some(Self {
            inner: unsafe { UPSafeCell::new(VirtIONetInner { net }) },
            rx_buf: [0u8; NET_BUF_SIZE],
            rx_len: 0,
            tx_buf: [0u8; NET_BUF_SIZE],
            loopback_queue: unsafe { UPSafeCell::new(VecDeque::new()) },
            mac_addr: mac,
        })
    }

    // 确认 VirtIO net 中断。
    // 执行时独占访问 VirtIO 网卡对象，并调用底层 ack_interrupt 清掉设备中断状态。
    pub fn ack_interrupt(&self) {
        self.inner.exclusive_access().net.ack_interrupt();
    }

    // 返回当前网卡 MAC 地址。
    // 执行时直接返回初始化阶段保存的 mac_addr，不访问硬件。
    pub fn mac_address(&self) -> [u8; 6] {
        self.mac_addr
    }
}

// 检查 MMIO 地址表里是否能找到 VirtIO net 设备。
// 执行时复用 find_net_mmio 扫描候选地址，只把是否找到设备转成 bool 返回。
pub fn has_mmio_net_device() -> bool {
    find_net_mmio().is_some()
}

// 扫描候选 MMIO 地址，返回网络设备的物理基址和虚拟指针。
// 执行时逐个读取 VirtIO magic 和 device_id，找到网卡设备就返回物理基址和高半区虚拟地址。
fn find_net_mmio() -> Option<(usize, *mut u8)> {
    for &base in VIRTIO_NET_BASES {
        let va = base | VIRT_ADDR_START;
        let magic = unsafe { core::ptr::read_volatile(va as *const u32) };
        if magic != VIRTIO_MMIO_MAGIC {
            continue;
        }
        let device_id = unsafe { core::ptr::read_volatile((va + 0x08) as *const u32) };
        println!("[net] probe {:#x}: device_id={}", base, device_id);
        if device_id == VIRTIO_NET_DEVICE_ID {
            return Some((base, va as *mut u8));
        }
    }
    None
}

unsafe impl Sync for VirtIONetAdapter {}
unsafe impl Send for VirtIONetAdapter {}

pub struct VirtioRxToken<'a> {
    buf: &'a mut [u8],
}

impl RxToken for VirtioRxToken<'_> {
    // 把收到的帧缓冲交给 smoltcp 处理。
    // 执行时把内部接收缓冲区交给闭包，闭包解析完协议栈数据后把结果原样返回。
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(self.buf)
    }
}

pub struct VirtioTxToken<'a> {
    buf: &'a mut [u8],
    device: &'a UPSafeCell<VirtIONetInner>,
    loopback_queue: &'a UPSafeCell<VecDeque<Vec<u8>>>,
    mac_addr: [u8; 6],
}

impl TxToken for VirtioTxToken<'_> {
    // 让 smoltcp 填充发送缓冲区，然后交给硬件或本地回环队列。
    // 执行时先让 smoltcp 写入帧内容，再判断目标 MAC 是否需要本地回环，最后必要时提交给 VirtIO 网卡。
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let result = f(&mut self.buf[..len]);
        let frame = &self.buf[..len];

        let mut looped_back = false;

        // 解析以太网头部的目标 MAC。
        if len >= 14 {
            let dst_mac = &frame[0..6];
            let is_broadcast = dst_mac == [0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
            let is_self = dst_mac == self.mac_addr;

            // 广播包或发给自己的包放进本地回环队列。
            if is_broadcast || is_self {
                let mut queue = self.loopback_queue.exclusive_access();
                if queue.len() < 2048 {
                    queue.push_back(frame.to_vec());
                }
                looped_back = true;
            }
        }

        // 广播仍然发到硬件；单播给自己的包只走本地回环。
        let mut inner = self.device.exclusive_access();
        if !looped_back || frame[0..6] == [0xff, 0xff, 0xff, 0xff, 0xff, 0xff] {
            let mut tx_buf = inner.net.new_tx_buffer(len);
            tx_buf.packet_mut().copy_from_slice(frame);
            inner.net.send(tx_buf).ok();
        }

        result
    }
}

impl Device for VirtIONetAdapter {
    type RxToken<'a> = VirtioRxToken<'a>;
    type TxToken<'a> = VirtioTxToken<'a>;

    // 先取本地回环包，再尝试从真实 VirtIO 接收队列取包。
    // 执行时优先弹出 loopback_queue；如果没有本地包，再从 VirtIO 队列取包并回收接收缓冲。
    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if let Some(frame) = self.loopback_queue.exclusive_access().pop_front() {
            let len = frame.len().min(NET_BUF_SIZE);
            self.rx_buf[..len].copy_from_slice(&frame[..len]);
            self.rx_len = len;
            return Some((
                VirtioRxToken {
                    buf: &mut self.rx_buf[..self.rx_len],
                },
                VirtioTxToken {
                    buf: &mut self.tx_buf,
                    device: &self.inner,
                    loopback_queue: &self.loopback_queue,
                    mac_addr: self.mac_addr,
                },
            ));
        }

        // 本地回环为空时再读真实 VirtIO 队列。
        {
            let mut inner = self.inner.exclusive_access();
            match inner.net.receive() {
                Ok(rx_buf) => {
                    let data = rx_buf.as_bytes();
                    let len = data.len().min(NET_BUF_SIZE);
                    self.rx_buf[..len].copy_from_slice(&data[..len]);
                    self.rx_len = len;
                    let _ = inner.net.recycle_rx_buffer(rx_buf);
                }
                Err(_) => return None,
            }
        }

        let rx_slice = &mut self.rx_buf[..self.rx_len];
        Some((
            VirtioRxToken { buf: rx_slice },
            VirtioTxToken {
                buf: &mut self.tx_buf,
                device: &self.inner,
                loopback_queue: &self.loopback_queue,
                mac_addr: self.mac_addr,
            },
        ))
    }

    // 提供一个发送 token 给 smoltcp。
    // 执行时把发送缓冲区、设备锁和本地回环队列打包进 TxToken，交给 smoltcp 后续填帧。
    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VirtioTxToken {
            buf: &mut self.tx_buf,
            device: &self.inner,
            loopback_queue: &self.loopback_queue,
            mac_addr: self.mac_addr,
        })
    }

    // 告诉 smoltcp 这块网卡的基础能力。
    // 执行时构造默认能力集，再填 MTU、burst 和以太网介质类型返回给 smoltcp。
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.max_burst_size = Some(1);
        caps.medium = Medium::Ethernet;
        caps
    }
}
