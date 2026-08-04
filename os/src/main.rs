#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![cfg_attr(target_arch = "loongarch64", feature(loongarch_target_feature))]

use crate::{
    syscall::{syscall, SYSCALL_EXEC},
    task::{
        check_and_wake_up_tasks, check_itimers, check_signals_error_of_current, current_add_signal,
        current_task, exit_current_and_run_next, force_exit_task_group, handle_signals,
        suspend_current_and_run_next, SignalFlags,
    },
};
// use polyhal::api::ArchInterface;
use log::*;
use polyhal::{common::PageAlloc, irq::IRQ, mem::get_mem_areas, PhysAddr};
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// Secondary harts wait behind the global-init barrier. After release they
/// enter the same per-CPU idle/scheduler loop as hart 0. RISC-V uses up to
/// four application harts and LoongArch uses up to two.
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
static SMP_GLOBAL_INIT_DONE: AtomicBool = AtomicBool::new(false);
// Secondary CPUs must not join the scheduler until CPU0 has installed the
// first task's page table and processor ownership. This closes the boot-time
// race between a newly online idle CPU and the primary CPU's first handoff.
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
pub(crate) static SMP_FIRST_TASK_ENTERED: AtomicBool = AtomicBool::new(false);
/// Every secondary reaches Rust only after polyhal has allocated and installed
/// its per-CPU area. Hart 0 must wait for every configured allocation before
/// it asks polyhal for the remaining physical-memory ranges; otherwise the buddy
/// allocator can import and overwrite a late secondary's per-CPU storage.
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
static SMP_PERCPU_READY: AtomicUsize = AtomicUsize::new(0);

#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
fn park_secondary_hart() -> ! {
    // Invalid harts are never published to the scheduler.  Keep their local
    // interrupts disabled and park permanently rather than touching globals.
    loop {
        #[cfg(target_arch = "riscv64")]
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack))
        };
        #[cfg(target_arch = "loongarch64")]
        unsafe {
            loongArch64::asm::idle()
        };
    }
}

#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
fn expected_cpu_mask() -> usize {
    let cpu_count = polyhal::common::get_cpu_num()
        .min(crate::config::PLATFORM_CPUS)
        .max(1);
    (1usize << cpu_count) - 1
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

/// Resolve a user page fault and synchronize shared address-space metadata.
///
/// Fault resolution only installs a previously absent mapping or upgrades a
/// restrictive PTE.  It never retires a frame or revokes access, so a remote
/// hart may safely retain an older TLB entry until it faults and reloads the
/// updated PTE.  Requiring an immediate remote acknowledgement here can
/// deadlock two threads while one holds a local IRQ-masking kernel lock.
/// Operations that revoke mappings still perform a synchronous shootdown
/// before releasing frames.
fn handle_current_user_page_fault(vaddr: polyhal::VirtAddr, access: crate::mm::PageFaultAccess) {
    let task = crate::task::current_task().unwrap();
    let fault_result = {
        let mut inner = task.inner_exclusive_access();
        inner.memory_set.handle_page_fault(vaddr, access)
    };

    let Some(fault_result) = fault_result else {
        #[cfg(feature = "smp-diagnostics")]
        {
            let inner = task.inner_exclusive_access();
            let ctx = inner.get_trap_cx();
            let pc = ctx[TrapFrameArgs::SEPC];
            let instruction = if inner.memory_set.valid_user_buf(
                pc as *const u8,
                core::mem::size_of::<u32>(),
                false,
            ) {
                let token = inner.get_user_token();
                Some(*crate::mm::translated_ref(token, pc as *const u32))
            } else {
                None
            };
            let args = ctx.args();
            println!(
                "[fault-diag] user page fault tid={} tgid={} access={} sepc={:#x} raw={:#010x} bad={:#x} sp={:#x} tp={:#x} ra={:#x} a0={:#x} a1={:#x} a2={:#x} a3={:#x} a4={:#x} a5={:#x} reason={} frames={}/{} free={} alloc_failures={}",
                task.getpid(),
                inner.tgid,
                match access {
                    crate::mm::PageFaultAccess::Read => "read",
                    crate::mm::PageFaultAccess::Write => "write",
                    crate::mm::PageFaultAccess::Execute => "execute",
                },
                pc,
                instruction.unwrap_or(0),
                vaddr.raw(),
                ctx[TrapFrameArgs::SP],
                ctx[TrapFrameArgs::TLS],
                ctx[TrapFrameArgs::RA],
                args[0],
                args[1],
                args[2],
                args[3],
                args[4],
                args[5],
                crate::mm::last_page_fault_failure(),
                crate::mm::allocated_frames(),
                crate::mm::total_frames(),
                crate::mm::available_frames(),
                crate::mm::frame_alloc_failures(),
            );
            #[cfg(target_arch = "loongarch64")]
            println!(
                "[fault-diag] loongarch r21={:#x} s0={:#x} s1={:#x} s2={:#x} s3={:#x} s4={:#x} s5={:#x} s6={:#x} s7={:#x} s8={:#x}",
                ctx.regs[21],
                ctx.regs[23],
                ctx.regs[24],
                ctx.regs[25],
                ctx.regs[26],
                ctx.regs[27],
                ctx.regs[28],
                ctx.regs[29],
                ctx.regs[30],
                ctx.regs[31],
            );
            #[cfg(target_arch = "loongarch64")]
            inner
                .memory_set
                .print_fault_mapping(polyhal::VirtAddr::from(ctx.regs[28]));
            inner.memory_set.print_fault_mapping(vaddr);
        }
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
}

fn exit_current_on_user_fault(signal: i32) -> ! {
    let exit_code = -signal;
    if let Some(task) = current_task() {
        force_exit_task_group(&task, exit_code);
    }
    exit_current_and_run_next(exit_code);
}

#[inline]
fn trap_from_user(ctx: &TrapFrame) -> bool {
    #[cfg(target_arch = "riscv64")]
    {
        ctx.from_user()
    }
    #[cfg(target_arch = "loongarch64")]
    {
        ctx.prmd & 0b11 != 0
    }
    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    {
        current_task().is_some()
    }
}

#[cfg(feature = "smp-diagnostics")]
fn diagnose_watched_frame_corruption(ctx: &TrapFrame, trap_type: TrapType) {
    let Some((paddr, offset, expected_head, actual_head, expected_word, actual_word)) =
        crate::mm::take_watched_frame_corruption()
    else {
        return;
    };
    println!(
        "[frame-diag] corruption paddr={:#x} offset={:#x} expected_head={:#010x} actual_head={:#010x} expected_word={:#010x} actual_word={:#010x} trap={:?} sepc={:#x}",
        paddr.raw(),
        offset,
        expected_head,
        actual_head,
        expected_word,
        actual_word,
        trap_type,
        ctx[TrapFrameArgs::SEPC],
    );
    for task in crate::task::task_snapshot() {
        let tid = task.getpid();
        let Some(inner) = task.try_inner_borrow() else {
            println!("[frame-diag] alias-scan tid={} busy", tid);
            continue;
        };
        let (page_table, areas, shared) = inner.memory_set.diagnostic_identity();
        println!(
            "[frame-diag] alias-scan tid={} tgid={} page_table={:#x} areas={:#x} shared={}",
            tid,
            inner.tgid,
            page_table,
            areas,
            shared,
        );
        inner.memory_set.print_frame_aliases(paddr);
    }
}

/// kernel interrupt
#[polyhal::arch_interrupt]
fn kernel_interrupt(ctx: &mut TrapFrame, trap_type: TrapType) {
    let from_user = trap_from_user(ctx);
    if from_user {
        if let Some(task) = crate::task::current_task() {
            task.save_user_float_state();
        }
        // user time accounting
        if let Some(task) = crate::task::current_task() {
            task.record_time_user_to_kernel();
        }
    }
    #[cfg(feature = "smp-diagnostics")]
    diagnose_watched_frame_corruption(ctx, trap_type);
    // trace!("trap_type @ {:x?} {:#x?}", trap_type, ctx);
    match trap_type {
        Breakpoint => return,
        SysCall => {
            // Exception entry disables CRMD.IE.  Restore it for the
            // interruptible part of a syscall: SpinNoIrq guards still mask
            // local interrupts around every shared-kernel critical section,
            // while a remote TLB shootdown can now be acknowledged during
            // long filesystem and loader operations.
            IRQ::int_enable();
            // jump to next instruction anyway
            ctx.syscall_ok();
            let args = ctx.args();
            let syscall_id = ctx[TrapFrameArgs::SYSCALL];
            // get system call return value
            // info!("syscall: {}", ctx[TrapFrameArgs::SYSCALL]);

            let result = syscall(
                syscall_id,
                [args[0], args[1], args[2], args[3], args[4], args[5]],
            );
            // A successful exec has replaced this trap frame with the new
            // program's entry context.  In particular, ARG0/RET now contains
            // argc.  Writing the syscall result (zero) would clobber argc.
            // Failed exec calls still return their errno to the old program.
            if syscall_id != SYSCALL_EXEC || result != 0 {
                ctx[TrapFrameArgs::RET] = result as usize;
            }
        }
        StorePageFault(bad_addr) => {
            IRQ::int_enable();
            handle_current_user_page_fault(
                polyhal::VirtAddr::from(bad_addr),
                crate::mm::PageFaultAccess::Write,
            );
        }
        InstructionPageFault(bad_addr)
            if bad_addr == task::SIGNAL_RETURN_TRAMPOLINE
                || ctx[TrapFrameArgs::SEPC] == task::SIGNAL_RETURN_TRAMPOLINE =>
        {
            IRQ::int_enable();
            if let Some(ret) = task::restore_current_signal_context() {
                ctx[TrapFrameArgs::RET] = ret as usize;
            } else {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        LoadPageFault(bad_addr) => {
            IRQ::int_enable();
            handle_current_user_page_fault(
                polyhal::VirtAddr::from(bad_addr),
                crate::mm::PageFaultAccess::Read,
            );
        }
        InstructionPageFault(bad_addr) => {
            IRQ::int_enable();
            #[cfg(feature = "smp-diagnostics")]
            if bad_addr == 0 || ctx[TrapFrameArgs::SEPC] == 0 {
                let args = ctx.args();
                let (tid, tgid) = current_task()
                    .map(|task| {
                        let tid = task.getpid();
                        let tgid = task.inner_exclusive_access().tgid;
                        (tid, tgid)
                    })
                    .unwrap_or((0, 0));
                println!(
                    "[fault-diag] instruction fault tid={} tgid={} sepc={:#x} bad={:#x} sp={:#x} ra={:#x} a0={:#x} a1={:#x} a2={:#x} a3={:#x} a4={:#x} a5={:#x}",
                    tid,
                    tgid,
                    ctx[TrapFrameArgs::SEPC],
                    bad_addr,
                    ctx[TrapFrameArgs::SP],
                    ctx[TrapFrameArgs::RA],
                    args[0],
                    args[1],
                    args[2],
                    args[3],
                    args[4],
                    args[5],
                );
            }
            handle_current_user_page_fault(
                polyhal::VirtAddr::from(bad_addr),
                crate::mm::PageFaultAccess::Execute,
            );
        }
        IllegalInstruction(bad_addr) => {
            IRQ::int_enable();
            #[cfg(feature = "smp-diagnostics")]
            {
                static ILLEGAL_DIAG_COUNT: core::sync::atomic::AtomicUsize =
                    core::sync::atomic::AtomicUsize::new(0);
                if ILLEGAL_DIAG_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 16 {
                    let (tid, tgid) = current_task()
                        .map(|task| {
                            let tid = task.getpid();
                            let tgid = task.inner_exclusive_access().tgid;
                            (tid, tgid)
                        })
                        .unwrap_or((0, 0));
                    let instruction = current_task().and_then(|task| {
                        let inner = task.inner_exclusive_access();
                        let pc = ctx[TrapFrameArgs::SEPC];
                        if !inner
                            .memory_set
                            .valid_user_buf(pc as *const u8, core::mem::size_of::<u32>(), false)
                        {
                            return None;
                        }
                        let token = inner.get_user_token();
                        drop(inner);
                        Some(*crate::mm::translated_ref(token, pc as *const u32))
                    });
            println!(
                "[fault-diag] illegal instruction tid={} tgid={} user={} sepc={:#x} mapped={} raw={:#010x} bad={:#x} sp={:#x} ra={:#x}",
                        tid,
                        tgid,
                        from_user,
                        ctx[TrapFrameArgs::SEPC],
                        instruction.is_some(),
                        instruction.unwrap_or(0),
                        bad_addr,
                        ctx[TrapFrameArgs::SP],
                ctx[TrapFrameArgs::RA],
            );
                    if let Some(task) = current_task() {
                        let inner = task.inner_exclusive_access();
                        inner
                            .memory_set
                            .print_fault_mapping(polyhal::VirtAddr::from(ctx[TrapFrameArgs::SEPC]));
                    }
                }
            }
            if !from_user {
                panic!(
                    "kernel illegal instruction sepc={:#x} bad={:#x}",
                    ctx[TrapFrameArgs::SEPC],
                    bad_addr
                );
            }
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
            #[cfg(feature = "mount-watchdog")]
            fs::vfs::mount_watchdog_poll();
            // Sleep queues, interval timers, and the network device are global
            // resources. A single timekeeper avoids interrupt-side lock
            // inversion while every hart still receives local timer ticks.
            if task::current_hart_id() == 0 {
                net::net_poll();
                check_and_wake_up_tasks();
                check_itimers();
                #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
                task::diagnose_scheduler_stall();
            }
            // Kernel code is non-preemptible: switching only from a user trap
            // keeps lock ownership and kernel stacks local to the current hart.
            if from_user && current_task().is_some() {
                suspend_current_and_run_next();
            }
        }
        _ => {
            warn!("unsuspended trap type: {:?}", trap_type);
        }
    }
    if !from_user {
        return;
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
    #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
    if hartid >= crate::config::PLATFORM_CPUS {
        IRQ::int_disable();
        park_secondary_hart();
    }
    #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
    if hartid != 0 {
        // polyhal_boot already installed this hart's per-CPU pointer.  Do not
        // touch heap, drivers, VFS, or the scheduler until hart 0 publishes
        // their completed global initialization.
        IRQ::int_disable();
        task::init_current_cpu(hartid);
        SMP_PERCPU_READY.fetch_or(1usize << hartid, Ordering::Release);
        while !SMP_GLOBAL_INIT_DONE.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        // Keep secondary harts out of scheduler accounting until CPU0 has
        // completed its first task handoff.  At this point a task has a valid
        // per-CPU owner and an installed user page table, so remote wakeups
        // and timer activity cannot race the bootstrap switch.
        while !SMP_FIRST_TASK_ENTERED.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        let online_mask = task::register_online_cpu(hartid);
        println!(
            "[smp] hart {} online (scheduler={}, percpu={:#x}, mask={:#x})",
            hartid,
            task::current_hart_id(),
            polyhal::percpu::get_percpu_ptr(),
            online_mask
        );
        if online_mask == expected_cpu_mask() {
            println!("[smp] online mask {:#x}", online_mask);
        }
        task::run_tasks();
    }
    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    if hartid != 0 {
        return;
    }
    println!("[kernel] Hello, world!");
    mm::init_heap();
    logging::init(option_env!("LOG"));
    println!("init logging");
    task::init_current_cpu(hartid);
    #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
    {
        let expected = expected_cpu_mask();
        SMP_PERCPU_READY.fetch_or(1, Ordering::Release);
        while SMP_PERCPU_READY.load(Ordering::Acquire) != expected {
            core::hint::spin_loop();
        }
    }
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
    #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
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

    .global _la_secondary_start
    _la_secondary_start:
        ori         $t0, $zero, 0x1
        lu52i.d     $t0, $t0, -2048
        csrwr       $t0, 0x180
        ori         $t0, $zero, 0x11
        lu52i.d     $t0, $t0, -1792
        csrwr       $t0, 0x181

        li.w        $t0, 0xb0
        csrwr       $t0, 0x0
        li.w        $t0, 0x00
        csrwr       $t0, 0x1
        li.w        $t0, 0x00
        csrwr       $t0, 0x2

        li.w        $t0, {mailbox1}
        iocsrrd.d   $sp, $t0
        csrrd       $a0, 0x20
        la.global   $t0, {secondary_entry}
        jirl        $zero, $t0, 0
    "#,
    entry = sym loongarch64_rust_tmp_main,
    secondary_entry = sym loongarch64_rust_secondary_main,
    mailbox1 = const loongArch64::consts::LOONGARCH_CSR_MAIL_BUF1,
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
    loongArch64::register::euen::set_sxe(true);
    polyhal::ctor::ph_init_iter(polyhal::ctor::CtorType::Cpu).for_each(|x| (x.func)());
    polyhal::mem::parse_system_info();
    polyhal::ctor::ph_init_iter(polyhal::ctor::CtorType::Platform).for_each(|x| (x.func)());
    polyhal::ctor::ph_init_iter(polyhal::ctor::CtorType::HALDriver).for_each(|x| (x.func)());
    const SECONDARY_STACK_SIZE: usize = 0x40_0000;
    let cpu_count = polyhal::common::get_cpu_num()
        .min(crate::config::PLATFORM_CPUS)
        .max(1);
    for cpu in 1..cpu_count {
        let stack_top = unsafe {
            polyhal::mem::alloc(SECONDARY_STACK_SIZE).add(SECONDARY_STACK_SIZE) as usize
                + polyhal::consts::VIRT_ADDR_START
        };
        polyhal::multicore::boot_core(
            cpu,
            _la_secondary_start as usize,
            stack_top,
        );
    }
    main(hart_id);
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "loongarch64")]
extern "C" {
    fn _la_secondary_start() -> !;
}

#[cfg(target_arch = "loongarch64")]
extern "C" fn loongarch64_rust_secondary_main(hart_id: usize) -> ! {
    polyhal::percpu::set_local_thread_pointer(hart_id);
    loongArch64::register::euen::set_fpe(true);
    loongArch64::register::euen::set_sxe(true);
    polyhal::ctor::ph_init_iter(polyhal::ctor::CtorType::Cpu).for_each(|x| (x.func)());
    main(hart_id);
    park_secondary_hart()
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
