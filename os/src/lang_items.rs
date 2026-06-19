// 这里负责操作系统的内核运行时支撑：放 no_std 内核需要自己提供的 panic 处理入口。
use core::panic::PanicInfo;
use log::*;
use polyhal::instruction::shutdown;

#[panic_handler]
// 内核 panic 入口，尽量打印位置和消息后关机。
// 执行时优先打印 panic 的文件和行号；没有位置信息时只打印消息，最后调用 shutdown 停机。
fn panic(info: &PanicInfo) -> ! {
    if let Some(location) = info.location() {
        error!(
            "[kernel] Panicked at {}:{} {}",
            location.file(),
            location.line(),
            info.message()
        );
    } else {
        error!("[kernel] Panicked: {}", info.message());
    }
    shutdown()
}
