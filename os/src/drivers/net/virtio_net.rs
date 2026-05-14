use core::ptr::NonNull;
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

use crate::drivers::block::VirtioHal;
use crate::sync::UPSafeCell;
use polyhal::PhysAddr;
use virtio_drivers::device::net::VirtIONet;
use virtio_drivers::transport::mmio::MmioTransport;

#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
const VIRTIO2: usize = 0x10003000;

#[cfg(target_arch = "aarch64")]
const VIRTIO2: usize = 0xa00_0300;

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
}

impl VirtIONetAdapter {
    pub fn new() -> Option<Self> {
        let ptr = PhysAddr::new(VIRTIO2).get_mut_ptr();
        println!("[net] probing VirtIO at {:#x}", VIRTIO2);
        let transport = match unsafe { MmioTransport::new(NonNull::new_unchecked(ptr)) } {
            Ok(t) => t,
            Err(e) => {
                println!("[net] MmioTransport::new failed: {:?}", e);
                return None;
            }
        };
        let net = match VirtIONet::new(transport, NET_BUF_SIZE) {
            Ok(n) => n,
            Err(e) => {
                println!("[net] VirtIONet::new failed: {:?}", e);
                return None;
            }
        };
        println!("[net] VirtIONetAdapter initialized successfully");
        Some(Self {
            inner: unsafe {
                UPSafeCell::new(VirtIONetInner { net })
            },
            rx_buf: [0u8; NET_BUF_SIZE],
            rx_len: 0,
            tx_buf: [0u8; NET_BUF_SIZE],
        })
    }

    pub fn ack_interrupt(&self) {
        self.inner.exclusive_access().net.ack_interrupt();
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.inner.exclusive_access().net.mac_address()
    }
}

unsafe impl Sync for VirtIONetAdapter {}
unsafe impl Send for VirtIONetAdapter {}

pub struct VirtioRxToken<'a> {
    buf: &'a mut [u8],
}

impl RxToken for VirtioRxToken<'_> {
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
}

impl TxToken for VirtioTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let result = f(&mut self.buf[..len]);
        let mut inner = self.device.exclusive_access();
        let mut tx_buf = inner.net.new_tx_buffer(len);
        tx_buf.packet_mut().copy_from_slice(&self.buf[..len]);
        inner.net.send(tx_buf).ok();
        result
    }
}

impl Device for VirtIONetAdapter {
    type RxToken<'a> = VirtioRxToken<'a>;
    type TxToken<'a> = VirtioTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
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
            },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VirtioTxToken {
            buf: &mut self.tx_buf,
            device: &self.inner,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.max_burst_size = Some(1);
        caps.medium = Medium::Ethernet;
        caps
    }
}
