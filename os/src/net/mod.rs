#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
mod imp {
    use smoltcp::iface::{Config, Interface, SocketSet};
    use smoltcp::socket::dhcpv4::Socket as DhcpSocket;
    use smoltcp::time::Instant;
    use smoltcp::wire::{EthernetAddress, HardwareAddress};

    use crate::drivers::net::VirtIONetAdapter;
    use crate::sync::UPSafeCell;
    use lazy_static::lazy_static;

    const DEFAULT_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    pub struct NetworkState {
        pub device: VirtIONetAdapter,
        pub iface: Interface,
        pub sockets: SocketSet<'static>,
        pub dhcp_handle: Option<smoltcp::iface::SocketHandle>,
        pub clock_ms: u64,
        pub inited: bool,
    }

    impl NetworkState {
    pub fn new() -> Option<Self> {
        let mut device = VirtIONetAdapter::new()?;
        let mac_addr = device.mac_address();
            let mac = if mac_addr == [0u8; 6] {
                DEFAULT_MAC
            } else {
                mac_addr
            };

            let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
            let now = Instant::from_millis(0);
            let iface = Interface::new(config, &mut device, now);

            let mut sockets = SocketSet::new(alloc::vec::Vec::new());
            let dhcp_socket = DhcpSocket::new();
            let dhcp_handle = sockets.add(dhcp_socket);

        Some(Self {
            device,
            iface,
            sockets,
            dhcp_handle: Some(dhcp_handle),
            clock_ms: 0,
            inited: false,
        })
    }

        pub fn poll(&mut self, ms_since_boot: u64) {
            self.clock_ms = ms_since_boot;
            let timestamp = Instant::from_millis(self.clock_ms as i64);
            self.iface.poll(timestamp, &mut self.device, &mut self.sockets);
        }
    }

    lazy_static! {
        pub static ref NET_IFACE: UPSafeCell<Option<NetworkState>> = unsafe {
            UPSafeCell::new(NetworkState::new())
        };
    }
}

pub mod socket;
pub mod syscall;

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
use imp::NET_IFACE;
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
use imp::NetworkState;

pub fn net_init() {
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    {
        let mut state = NET_IFACE.exclusive_access();
        if let Some(state) = state.as_mut() {
            state.inited = true;
            log::info!("net: VirtIO-Net initialized, MAC={:02x?}", state.device.mac_address());
        } else {
            log::warn!("net: No virtio-net device available, networking disabled");
        }
    }
}

pub fn net_poll() {
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    {
        let mut state = NET_IFACE.exclusive_access();
        if let Some(state) = state.as_mut() {
            if !state.inited {
                return;
            }
            state.clock_ms += 10;
            let clock = state.clock_ms;
            let timestamp = smoltcp::time::Instant::from_millis(clock as i64);
            let NetworkState { ref mut iface, ref mut device, ref mut sockets, .. } = state;
            iface.poll(timestamp, device, sockets);
        }
    }
}

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
pub fn with_net_iface<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut NetworkState) -> R,
{
    let mut guard = NET_IFACE.exclusive_access();
    guard.as_mut().map(|state| f(state))
}
