// 这里负责操作系统的日志管理：实现内核日志缓冲区和 log crate 的输出后端。
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicUsize, Ordering};

use log::{self, info, Level, LevelFilter, Log, Metadata, Record};
use polyhal::debug_console::DebugConsole;

const KLOG_SIZE: usize = 0x10000; // 64KB ring buffer
static KLOG_BUF: [u8; KLOG_SIZE] = [0u8; KLOG_SIZE];
static KLOG_WRITE: AtomicUsize = AtomicUsize::new(0);

// 返回日志环形缓冲区的两段切片，处理已经回绕和没回绕两种情况。
// 执行时读取全局写入计数，按是否超过缓冲区大小返回一段或两段有效日志切片。
pub fn klog_bytes() -> (&'static [u8], &'static [u8]) {
    let w = KLOG_WRITE.load(Ordering::Relaxed);
    let pos = w % KLOG_SIZE;
    if w < KLOG_SIZE {
        // 日志还没有写满一圈时，只有 [0..pos] 这一段有效。
        (&KLOG_BUF[..pos], &[])
    } else {
        // 日志写满后按环形缓冲区顺序返回“旧数据”和“新数据”两段。
        (&KLOG_BUF[pos..], &KLOG_BUF[..pos])
    }
}

// 清空内核日志缓冲区。
// 执行时把写入计数清零；旧字节不必擦除，因为后续读取只按计数决定有效范围。
pub fn klog_clear() {
    KLOG_WRITE.store(0, Ordering::Relaxed);
}

// 追加一段字符串到内核日志环形缓冲区。
// 执行时从当前写入位置开始逐字节覆盖环形缓冲区，最后把新的写入计数存回原子变量。
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
    // 具体级别由 set_max_level 控制，这里统一放行。
    // 执行时直接返回 true，把过滤逻辑交给 log crate 的全局 max level。
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    // 按级别加颜色后写到 Logger，本身也会进入 klog。
    // 执行时先检查 enabled，再按日志级别选颜色，最后格式化出带文件行号的日志行。
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

    // 当前日志后端没有额外缓冲，flush 为空。
    // 执行时什么都不做，因为日志已经同步写入 klog 和串口。
    fn flush(&self) {}
}

impl Write for Logger {
    // 写日志时同时进 klog 和调试串口。
    // 执行时先追加到 klog，再把每个 char 编码成 UTF-8 字节写到调试控制台。
    fn write_str(&mut self, s: &str) -> fmt::Result {
        klog_push(s);
        let mut buffer = [0u8; 4];
        for c in s.chars() {
            puts(c.encode_utf8(&mut buffer).as_bytes())
        }
        Ok(())
    }
}

// 初始化 log crate 的全局 logger 和过滤级别。
// 执行时注册 Logger，再把字符串级别转换成 LevelFilter，最后打印一条初始化日志。
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
// 向调试控制台写一段字节。
// 执行时逐字节调用 DebugConsole::putchar，保证早期内核阶段也能直接输出。
pub fn puts(buffer: &[u8]) {
    // 当前日志后端直接写调试串口，便于早期内核阶段也能输出。
    for i in buffer {
        DebugConsole::putchar(*i);
    }
}
