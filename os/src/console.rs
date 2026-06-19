// 这里负责操作系统的控制台输出：封装调试控制台输出，并提供 print!/println! 宏。
use core::fmt::{self, Write};

use polyhal::debug_console::DebugConsole;

struct Stdout;

impl Write for Stdout {
    // 把格式化出来的字符串逐字节写到调试控制台。
    // 执行时遍历字符串的每个字节，逐个交给 DebugConsole::putchar，写完后返回 fmt::Result。
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.bytes() {
            DebugConsole::putchar(c);
        }
        Ok(())
    }
}

// println!/print! 宏最后都会走到这里。
// 执行时把 format_args! 传进 Stdout，让 fmt 框架回调 write_str 输出到调试串口。
pub fn print(args: fmt::Arguments) {
    Stdout.write_fmt(args).unwrap();
}

// 同时写串口和内核日志缓冲区，dmesg 会读后者。
// 执行时先把格式化文本追加进 klog 环形缓冲区，再通过 Stdout 写到调试串口。
pub fn log_write(args: fmt::Arguments) {
    use core::fmt::Write;
    crate::logging::klog_push(&alloc::format!("{}", args));
    Stdout.write_fmt(args).unwrap();
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
