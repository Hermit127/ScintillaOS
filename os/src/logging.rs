use core::fmt::{self, Write};
use core::sync::atomic::{AtomicUsize, Ordering};

use log::{self, info, Level, LevelFilter, Log, Metadata, Record};
use polyhal::debug_console::DebugConsole;

const KLOG_SIZE: usize = 0x10000; // 64KB ring buffer
static KLOG_BUF: [u8; KLOG_SIZE] = [0u8; KLOG_SIZE];
static KLOG_WRITE: AtomicUsize = AtomicUsize::new(0);

pub fn klog_bytes() -> (&'static [u8], &'static [u8]) {
    let w = KLOG_WRITE.load(Ordering::Relaxed);
    let pos = w % KLOG_SIZE;
    if w < KLOG_SIZE {
        // Buffer hasn't wrapped yet: data is [0..pos], no second part
        (&KLOG_BUF[..pos], &[])
    } else {
        // Buffer wrapped: oldest data is [pos..KLOG_SIZE], newest is [0..pos]
        (&KLOG_BUF[pos..], &KLOG_BUF[..pos])
    }
}

pub fn klog_clear() {
    KLOG_WRITE.store(0, Ordering::Relaxed);
}

pub fn klog_push(s: &str) {
    let bytes = s.as_bytes();
    let mut w = KLOG_WRITE.load(Ordering::Relaxed);
    for &b in bytes {
        let idx = w % KLOG_SIZE;
        unsafe {
            let ptr = KLOG_BUF.as_ptr().add(idx) as *mut u8;
            core::ptr::write(ptr, b);
        }
        w += 1;
    }
    KLOG_WRITE.store(w, Ordering::Relaxed);
}

pub struct Logger;

impl Log for Logger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let file = record.file();
        let line = record.line();

        let color_code = match record.level() {
            Level::Error => 31u8, // Red
            Level::Warn => 93,    // BrightYellow
            Level::Info => 34,    // Blue
            Level::Debug => 32,   // Green
            Level::Trace => 90,   // BrightBlack
        };
        write!(
            Logger,
            "\u{1B}[{}m\
            [{}] {}:{} {}\
            \u{1B}[0m\n",
            color_code,
            record.level(),
            file.unwrap(),
            line.unwrap(),
            record.args()
        )
        .expect("can't write color string in logging module.");
    }

    fn flush(&self) {}
}

impl Write for Logger {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        klog_push(s);
        let mut buffer = [0u8; 4];
        for c in s.chars() {
            puts(c.encode_utf8(&mut buffer).as_bytes())
        }
        Ok(())
    }
}

pub fn init(level: Option<&str>) {
    log::set_logger(&Logger).unwrap();
    log::set_max_level(match level {
        Some("error") => LevelFilter::Error,
        Some("warn") => LevelFilter::Warn,
        Some("info") => LevelFilter::Info,
        Some("debug") => LevelFilter::Debug,
        Some("trace") => LevelFilter::Trace,
        _ => LevelFilter::Off,
    });
    info!("logging module initialized");
}

#[inline]
pub fn puts(buffer: &[u8]) {
    // use the main uart if it exists.
    for i in buffer {
        DebugConsole::putchar(*i);
    }
}
