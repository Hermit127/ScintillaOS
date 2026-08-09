use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use polyhal::time::Time;

static REALTIME_READY: AtomicBool = AtomicBool::new(false);
static REALTIME_OFFSET_US: AtomicU64 = AtomicU64::new(0);

fn monotonic_us() -> u64 {
    (Time::now().to_nsec() / 1000) as u64
}

pub fn uptime_us() -> u64 {
    monotonic_us()
}

pub fn init_realtime_clock() {
    let monotonic = monotonic_us();
    let Some(epoch_us) = platform_realtime_us() else {
        crate::println!("[time] platform RTC unavailable; realtime uses boot clock");
        return;
    };
    REALTIME_OFFSET_US.store(epoch_us.saturating_sub(monotonic), Ordering::Release);
    REALTIME_READY.store(true, Ordering::Release);
    crate::println!(
        "[time] realtime clock initialized at {}",
        epoch_us / 1_000_000
    );
}

pub fn realtime_us() -> u64 {
    let monotonic = monotonic_us();
    if REALTIME_READY.load(Ordering::Acquire) {
        monotonic.saturating_add(REALTIME_OFFSET_US.load(Ordering::Acquire))
    } else {
        monotonic
    }
}

pub fn realtime_sec() -> i64 {
    (realtime_us() / 1_000_000) as i64
}

#[cfg(target_arch = "riscv64")]
fn platform_realtime_us() -> Option<u64> {
    const GOLDFISH_RTC_BASE: usize = 0x0010_1000;
    let base = polyhal::PhysAddr::new(GOLDFISH_RTC_BASE).get_ptr::<u32>();
    // Goldfish latches TIME_HIGH when TIME_LOW is read.
    let low = unsafe { core::ptr::read_volatile(base) } as u64;
    let high = unsafe { core::ptr::read_volatile(base.wrapping_add(1)) } as u64;
    let epoch_ns = (high << 32) | low;
    (epoch_ns >= 946_684_800_000_000_000).then_some(epoch_ns / 1000)
}

#[cfg(target_arch = "loongarch64")]
fn platform_realtime_us() -> Option<u64> {
    const LS7A_RTC_BASE: usize = 0x100d_0100;
    const TOY_READ0: usize = 0x2c;
    const TOY_READ1: usize = 0x30;
    const RTC_CTRL: usize = 0x40;
    const RTC_CTRL_ENABLE: u32 = (1 << 11) | (1 << 8);

    let base = polyhal::PhysAddr::new(LS7A_RTC_BASE).get_ptr::<u8>();
    unsafe {
        core::ptr::write_volatile(base.add(RTC_CTRL) as *mut u32, RTC_CTRL_ENABLE);
    }
    let (raw, year_since_1900) = loop {
        let first = unsafe { core::ptr::read_volatile(base.add(TOY_READ0) as *const u32) };
        let year = unsafe { core::ptr::read_volatile(base.add(TOY_READ1) as *const u32) };
        let second = unsafe { core::ptr::read_volatile(base.add(TOY_READ0) as *const u32) };
        if first == second {
            break (first, year);
        }
    };

    let year = year_since_1900 as i64 + 1900;
    let month = ((raw >> 26) & 0x3f) as i64;
    let day = ((raw >> 21) & 0x1f) as i64;
    let hour = ((raw >> 16) & 0x1f) as i64;
    let minute = ((raw >> 10) & 0x3f) as i64;
    let second = ((raw >> 4) & 0x3f) as i64;
    if !(2000..=2400).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let epoch_sec = days
        .checked_mul(86_400)?
        .checked_add(hour * 3600 + minute * 60 + second)?;
    u64::try_from(epoch_sec).ok()?.checked_mul(1_000_000)
}

#[cfg(target_arch = "loongarch64")]
fn days_from_civil(mut year: i64, month: i64, day: i64) -> i64 {
    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
fn platform_realtime_us() -> Option<u64> {
    None
}
