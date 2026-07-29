use super::exit;

#[panic_handler]
fn panic_handler(panic_info: &core::panic::PanicInfo) -> ! {
    let err = panic_info.message();
    if let Some(location) = panic_info.location() {
        println!(
            "Panicked at {}:{}, {}",
            location.file(),
            location.line(),
            err
        );
    } else {
        println!("Panicked: {}", err);
    }
    // Do not raise SIGABRT through kill() here.  The old implementation passed
    // the SIGABRT bit mask (64) where kill expects signal number 6, so the
    // kernel ignored it and the following unreachable!() recursively panicked.
    exit(-6)
}
