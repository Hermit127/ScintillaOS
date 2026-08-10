use crate::time::Time;
use core::sync::atomic::{AtomicUsize, Ordering};
use riscv::register::{sie, time};

const DEFAULT_CLOCK_FREQ: usize = 10_000_000;
static CLOCK_FREQ: AtomicUsize = AtomicUsize::new(DEFAULT_CLOCK_FREQ);

fn discover_clock_frequency() -> usize {
    crate::mem::get_fdt()
        .ok()
        .and_then(|fdt| fdt.find_nodes("/cpus").next())
        .and_then(|cpus| cpus.find_property("timebase-frequency"))
        .map(|property| property.u32() as usize)
        .filter(|frequency| *frequency != 0)
        .unwrap_or(DEFAULT_CLOCK_FREQ)
}

impl Time {
    #[inline]
    pub fn get_freq() -> usize {
        CLOCK_FREQ.load(Ordering::Acquire)
    }

    #[inline]
    pub fn now() -> Self {
        Self(time::read())
    }
}

// Setting the time interval for then next time
#[inline]
pub fn set_next_timeout() {
    // Setting the timer through calling SBI.
    sbi_rt::set_timer((time::read() + Time::get_freq() / 100) as _);
}

// Initialize the Timer 
pub fn init() {
    CLOCK_FREQ.store(discover_clock_frequency(), Ordering::Release);
    unsafe {
        sie::set_stimer();
    }
    set_next_timeout();
    log::info!("initialize timer interrupt");
}
