// 这里负责操作系统的控制台输出：封装调试控制台输出，并提供 print!/println! 宏。
use core::fmt::{self, Write};
use polyhal::debug_console::DebugConsole;
use polyhal::utils::MutexNoIrq;

// Kernel diagnostics and userspace stdout both use this lock.  polyhal's
// formatter lock cannot protect direct UART writes issued by the stdout file.
static SERIAL_LOCK: MutexNoIrq<()> = MutexNoIrq::new(());

struct SerialWriter;

impl Write for SerialWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            DebugConsole::putchar(byte);
        }
        Ok(())
    }
}

pub fn write_bytes(bytes: &[u8]) {
    let _guard = SERIAL_LOCK.lock();
    for byte in bytes {
        DebugConsole::putchar(*byte);
    }
}

// println!/print! 宏最后都会走到这里。
// 执行时把 format_args! 传进 Stdout，让 fmt 框架回调 write_str 输出到调试串口。
pub fn print(args: fmt::Arguments) {
    let _guard = SERIAL_LOCK.lock();
    SerialWriter
        .write_fmt(args)
        .expect("can't print console arguments");
}

// 同时写串口和内核日志缓冲区，dmesg 会读后者。
// 执行时先把格式化文本追加进 klog 环形缓冲区，再通过 Stdout 写到调试串口。
pub fn log_write(args: fmt::Arguments) {
    crate::logging::klog_push(&alloc::format!("{}", args));
    print(args);
}

#[macro_export]
macro_rules! print {
    ($fmt: literal $(, $($arg: tt)+)?) => {
        $crate::console::print(format_args!($fmt $(, $($arg)+)?));
    }
}

#[macro_export]
macro_rules! println {
    ($fmt: literal $(, $($arg: tt)+)?) => {
        $crate::console::print(format_args!(concat!($fmt, "\n") $(, $($arg)+)?));
    }
}
