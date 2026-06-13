#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

use crate::{
    syscall::syscall,
    task::{
        check_and_wake_up_tasks, check_signals_error_of_current, current_add_signal,
        current_task, exit_current_and_run_next, handle_signals, ready_queue_len, SignalFlags,
        suspend_current_and_run_next,
    },
};
// use polyhal::api::ArchInterface;
use log::*;
use polyhal::{common::PageAlloc, irq::IRQ, mem::get_mem_areas, PhysAddr};
#[cfg(not(target_arch = "loongarch64"))]
use polyhal_boot::define_entry;
use polyhal_trap::{
    trap::TrapType::{self, *},
    trapframe::{TrapFrame, TrapFrameArgs},
};
extern crate alloc;

#[macro_use]
extern crate bitflags;

#[macro_use]
mod console;
mod config;
mod drivers;
mod fs;
mod lang_items;
mod logging;
mod mm;
mod net;
mod sync;
mod syscall;
mod task;

pub struct ArchInterfaceImpl;

/// kernel interrupt
#[polyhal::arch_interrupt]
fn kernel_interrupt(ctx: &mut TrapFrame, trap_type: TrapType) {
    //user time accounting
    if let Some(task) = crate::task::current_task() {
        task.record_time_user_to_kernel();
    }
    // trace!("trap_type @ {:x?} {:#x?}", trap_type, ctx);
    match trap_type {
        Breakpoint => return,
        SysCall => {
            // jump to next instruction anyway
            ctx.syscall_ok();
            let args = ctx.args();
            // get system call return value
            // info!("syscall: {}", ctx[TrapFrameArgs::SYSCALL]);

            let result = syscall(
                ctx[TrapFrameArgs::SYSCALL],
                [args[0], args[1], args[2], args[3], args[4], args[5]],
            );
            // cx is changed during sys_exec, so we have to call it again
            ctx[TrapFrameArgs::RET] = result as usize;
        }
        StorePageFault(bad_addr) => {
            let vaddr = polyhal::VirtAddr::from(bad_addr);
            let task = crate::task::current_task().unwrap();
            let mut inner = task.inner_exclusive_access();
            if !inner.memory_set.handle_page_fault(vaddr, true) {
                drop(inner);
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        LoadPageFault(bad_addr) | InstructionPageFault(bad_addr) => {
            let vaddr = polyhal::VirtAddr::from(bad_addr);
            let task = crate::task::current_task().unwrap();
            let mut inner = task.inner_exclusive_access();
            if !inner.memory_set.handle_page_fault(vaddr, false) {
                drop(inner);
                exit_current_and_run_next(-11);
            }
        }
        InstructionPageFault(_paddr) => {
            current_add_signal(SignalFlags::SIGSEGV);
        }
        IllegalInstruction(_) => {
            current_add_signal(SignalFlags::SIGILL);
        }
        Timer => {
            static mut TICK: usize = 0;
            unsafe { TICK += 1; }
            net::net_poll();
            net::wake_net_blocked_tasks();
            check_and_wake_up_tasks();
            check_itimer();
            if ready_queue_len() > 0 {
                suspend_current_and_run_next();
            }
        }
        _ => {
            warn!("unsuspended trap type: {:?}", trap_type);
        }
    }
    // handle signals (handle the sent signal)
    // println!("[K] trap_handler:: handle_signals");
    handle_signals();

    // check error signals (if error then exit)
    if let Some((errno, msg)) = check_signals_error_of_current() {
        println!("[kernel] {}", msg);
        exit_current_and_run_next(errno);
    }

    // kernel time accounting
    if let Some(task) = crate::task::current_task() {
        task.record_time_kernel_to_user();
    }
}

fn check_itimer() {
    let Some(task) = current_task() else { return };
    let Some(mut inner) = task.try_inner_exclusive_access() else { return };
    if inner.itimer_expire == 0 {
        return;
    }
    let now = polyhal::time::Time::now().to_usec() as u64;
    if now >= inner.itimer_expire {
        inner.signals.insert(SignalFlags::SIGALRM);
        if inner.itimer_interval_us > 0 {
            inner.itimer_expire = now + inner.itimer_interval_us;
        } else {
            inner.itimer_expire = 0;
        }
    }
}

fn main(hartid: usize) {
    trace!("ch7 main: hartid: {}", hartid);
    if hartid != 0 {
        return;
    }
    println!("[kernel] Hello, world!");
    mm::init_heap();
    logging::init(option_env!("LOG"));
    println!("init logging");
    // polyhal::init_interrupt(); done in polyhal::CPU::rust_main()

    polyhal::common::init(&PageAllocImpl);
    get_mem_areas().for_each(|(start, size)| {
        println!("init memory region {:#x} - {:#x}", start, start + size);
        mm::add_frames_range(*start, start + size);
    });

    fs::mount_root();
    net::net_init();
    fs::list_apps();
    task::init_kernel_page();
    task::add_initproc();
    task::run_tasks();
    panic!("Unreachable in main function of rCore Tutorial kernel!");
}

#[cfg(not(target_arch = "loongarch64"))]
define_entry!(main);

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    r#"
    .section .bss.bstack
    .global bstack
    .global bstack_top
    bstack:
    .fill 0x80000
    .size bstack, . - bstack
    bstack_top:

    .section .text.entry
    .global _start
    _start:
        ori         $t0, $zero, 0x1
        lu52i.d     $t0, $t0, -2048
        csrwr       $t0, 0x180
        ori         $t0, $zero, 0x11
        lu52i.d     $t0, $t0, -1792
        csrwr       $t0, 0x181

        li.d        $t0, 0x9000000000200028
        jirl        $zero, $t0, 0
    1:
        li.w        $t0, 0xb0
        csrwr       $t0, 0x0
        li.w        $t0, 0x00
        csrwr       $t0, 0x1
        li.w        $t0, 0x00
        csrwr       $t0, 0x2

        la.global   $sp, bstack_top
        csrrd       $a0, 0x20
        la.global   $t0, {entry}
        jirl        $zero, $t0, 0
    "#,
    entry = sym loongarch64_rust_tmp_main,
);

#[cfg(target_arch = "loongarch64")]
#[no_mangle]
extern "C" fn loongarch64_rust_tmp_main(hart_id: usize) -> ! {
    unsafe {
        extern "C" {
            fn _sbss();
            fn _ebss();
        }
        core::ptr::write_bytes(_sbss as *mut u8, 0, _ebss as usize - _sbss as usize);
    }
    let _ = polyhal::mem::init_dtb_once(polyhal::consts::QEMU_DTB_ADDR);
    polyhal::percpu::set_local_thread_pointer(hart_id);
    loongArch64::register::euen::set_fpe(true);
    polyhal::ctor::ph_init_iter(polyhal::ctor::CtorType::Cpu).for_each(|x| (x.func)());
    polyhal::mem::parse_system_info();
    polyhal::ctor::ph_init_iter(polyhal::ctor::CtorType::Platform).for_each(|x| (x.func)());
    polyhal::ctor::ph_init_iter(polyhal::ctor::CtorType::HALDriver).for_each(|x| (x.func)());
    main(hart_id);
    loop {
        core::hint::spin_loop();
    }
}

pub struct PageAllocImpl;

impl PageAlloc for PageAllocImpl {
    #[inline]
    fn alloc(&self) -> PhysAddr {
        mm::frame_alloc_persist().expect("can't find memory page")
    }

    #[inline]
    fn dealloc(&self, paddr: PhysAddr) {
        mm::frame_dealloc(paddr)
    }
}
