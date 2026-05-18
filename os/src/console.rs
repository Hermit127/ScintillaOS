use core::fmt::{self, Write};

use polyhal::debug_console::DebugConsole;

struct Stdout;

impl Write for Stdout {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.bytes() {
            DebugConsole::putchar(c);
        }
        Ok(())
    }
}

pub fn print(args: fmt::Arguments) {
    Stdout.write_fmt(args).unwrap();
}

/// Write to both UART and kernel log buffer (for dmesg)
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
