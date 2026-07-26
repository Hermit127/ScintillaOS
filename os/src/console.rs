// 这里负责操作系统的控制台输出：封装调试控制台输出，并提供 print!/println! 宏。
use core::fmt;

// println!/print! 宏最后都会走到这里。
// 执行时把 format_args! 传进 Stdout，让 fmt 框架回调 write_str 输出到调试串口。
pub fn print(args: fmt::Arguments) {
    // Serialize one complete formatting operation across harts.  Calling
    // DebugConsole::putchar directly allowed byte-level interleaving, which
    // corrupted diagnostics and even split machine-verifiable END markers.
    polyhal::debug_console::_print(args);
}

// 同时写串口和内核日志缓冲区，dmesg 会读后者。
// 执行时先把格式化文本追加进 klog 环形缓冲区，再通过 Stdout 写到调试串口。
pub fn log_write(args: fmt::Arguments) {
    crate::logging::klog_push(&alloc::format!("{}", args));
    polyhal::debug_console::_print(args);
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
