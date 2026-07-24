#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

use crate::{
    syscall::syscall,
    task::{
        check_and_wake_up_tasks, check_itimers, check_signals_error_of_current, current_add_signal,
        current_task, exit_current_and_run_next, force_exit_task_group, handle_signals,
        suspend_current_and_run_next, SignalFlags,
    },
};
// use polyhal::api::ArchInterface;
use log::*;
use polyhal::{common::PageAlloc, irq::IRQ, mem::get_mem_areas, PhysAddr};
#[cfg(target_arch = "riscv64")]
use core::sync::atomic::{AtomicBool, Ordering};
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

/// The first SMP milestone recognises exactly the four QEMU virt harts.
/// Secondary harts wait behind the global-init barrier.  After release they
/// enter the same per-CPU idle/scheduler loop as hart 0.
#[cfg(target_arch = "riscv64")]
const RISCV_SMP_HARTS: usize = 4;
#[cfg(target_arch = "riscv64")]
static SMP_GLOBAL_INIT_DONE: AtomicBool = AtomicBool::new(false);

#[cfg(target_arch = "riscv64")]
fn park_secondary_hart() -> ! {
    // Invalid harts are never published to the scheduler.  Keep their local
    // interrupts disabled and park permanently rather than touching globals.
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}

fn sync_shared_vm_after_page_fault(
    task: &alloc::sync::Arc<task::TaskControlBlock>,
    vaddr: polyhal::VirtAddr,
) {
    let snapshot = {
        let inner = task.inner_exclusive_access();
        if !inner.memory_set.is_shared() {
            return;
        }
        let vpn = vaddr.floor();
        let Some(frame) = inner.memory_set.clone_frame_metadata(vpn) else {
            return;
        };
        (task.getpid(), inner.tgid, vpn, frame)
    };
    crate::syscall::process::sync_thread_group_vm_frame(
        snapshot.0,
        snapshot.1,
        snapshot.2,
        &snapshot.3,
    );
}

/// Resolve a user page fault and, for a shared page table, flush the changed
/// PTE on any other hart that is currently using it.  Both the TCB lock and
/// peer-metadata synchronization are finished before the SBI RFENCE call.
fn handle_current_user_page_fault(vaddr: polyhal::VirtAddr, access: crate::mm::PageFaultAccess) {
    let task = crate::task::current_task().unwrap();
    let (fault_result, active_cpus) = {
        let mut inner = task.inner_exclusive_access();
        let fault_result = inner.memory_set.handle_page_fault(vaddr, access);
        let active_cpus = fault_result
            .is_some()
            .then(|| inner.memory_set.is_shared())
            .filter(|is_shared| *is_shared)
            .map(|_| inner.memory_set.active_cpu_mask());
        (fault_result, active_cpus)
    };

    let Some(fault_result) = fault_result else {
        // A userspace access-permission fault is delivered through the normal
        // signal path.  The previous unconditional exit bypassed an installed
        // SIGSEGV handler, so `lat_sig prot` could never return to its faulting
        // store and perform the next measurement iteration.
        current_add_signal(SignalFlags::SIGSEGV);
        return;
    };
    if fault_result == crate::mm::PageFaultResult::SharedMetadataChanged {
        sync_shared_vm_after_page_fault(&task, vaddr);
    }
    if let Some(active_cpus) = active_cpus {
        let _ = crate::task::flush_remote_tlb(active_cpus, vaddr.floor().raw(), crate::config::PAGE_SIZE);
    }
}

fn exit_current_on_user_fault(signal: i32) -> ! {
    let exit_code = -signal;
    if let Some(task) = current_task() {
        force_exit_task_group(&task, exit_code);
    }
    exit_current_and_run_next(exit_code);
}

/// kernel interrupt
#[polyhal::arch_interrupt]
fn kernel_interrupt(ctx: &mut TrapFrame, trap_type: TrapType) {
    if let Some(task) = crate::task::current_task() {
        task.save_user_float_state();
    }
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
            handle_current_user_page_fault(
                polyhal::VirtAddr::from(bad_addr),
                crate::mm::PageFaultAccess::Write,
            );
        }
        InstructionPageFault(bad_addr)
            if bad_addr == task::SIGNAL_RETURN_TRAMPOLINE
                || ctx[TrapFrameArgs::SEPC] == task::SIGNAL_RETURN_TRAMPOLINE =>
        {
            if let Some(ret) = task::restore_current_signal_context() {
                ctx[TrapFrameArgs::RET] = ret as usize;
            } else {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        LoadPageFault(bad_addr) => {
            handle_current_user_page_fault(
                polyhal::VirtAddr::from(bad_addr),
                crate::mm::PageFaultAccess::Read,
            );
        }
        InstructionPageFault(bad_addr) => {
            handle_current_user_page_fault(
                polyhal::VirtAddr::from(bad_addr),
                crate::mm::PageFaultAccess::Execute,
            );
        }
        IllegalInstruction(_) => {
            current_add_signal(SignalFlags::SIGILL);
        }
        SupervisorSoft => {
            // An IPI only wakes this hart to revisit scheduler work. Do not
            // context-switch directly from the trap handler or while any
            // interrupt-side subsystem may hold a lock.
            let _ = task::handle_ipi();
        }
        Timer => {
            task::record_timer_tick();
            net::net_poll();
            net::wake_net_blocked_tasks();
            check_and_wake_up_tasks();
            check_itimers();
            #[cfg(target_arch = "riscv64")]
            task::diagnose_scheduler_stall();
            if current_task().is_some() {
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
        if let Some(task) = current_task() {
            force_exit_task_group(&task, errno);
        }
        exit_current_and_run_next(errno);
    }

    // kernel time accounting
    if let Some(task) = crate::task::current_task() {
        task.record_time_kernel_to_user();
    }
}

fn main(hartid: usize) {
    trace!("ch7 main: hartid: {}", hartid);
    #[cfg(target_arch = "riscv64")]
    if hartid >= RISCV_SMP_HARTS {
        IRQ::int_disable();
        park_secondary_hart();
    }
    #[cfg(target_arch = "riscv64")]
    if hartid != 0 {
        // polyhal_boot already installed this hart's per-CPU pointer.  Do not
        // touch heap, drivers, VFS, or the scheduler until hart 0 publishes
        // their completed global initialization.
        IRQ::int_disable();
        task::init_current_cpu(hartid);
        while !SMP_GLOBAL_INIT_DONE.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        task::register_online_cpu(hartid);
        println!(
            "[smp] hart {} online (scheduler={}, percpu={:#x})",
            hartid,
            task::current_hart_id(),
            polyhal::percpu::get_percpu_ptr()
        );
        task::run_tasks();
    }
    #[cfg(not(target_arch = "riscv64"))]
    if hartid != 0 {
        return;
    }
    println!("[kernel] Hello, world!");
    mm::init_heap();
    logging::init(option_env!("LOG"));
    println!("init logging");
    task::init_current_cpu(hartid);
    println!(
        "[smp] hart {} online (scheduler={}, percpu={:#x})",
        hartid,
        task::current_hart_id(),
        polyhal::percpu::get_percpu_ptr()
    );
    // polyhal::init_interrupt(); done in polyhal::CPU::rust_main()

    polyhal::common::init(&PageAllocImpl);
    get_mem_areas().iter().for_each(|(start, size)| {
        println!("init memory region {:#x} - {:#x}", start, start + size);
        mm::add_frames_range(*start, start + size);
    });

    fs::mount_root();
    net::net_init();
    fs::list_apps();
    task::init_kernel_page();
    #[cfg(target_arch = "riscv64")]
    SMP_GLOBAL_INIT_DONE.store(true, Ordering::Release);
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
