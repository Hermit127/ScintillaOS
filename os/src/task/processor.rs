// 这里负责操作系统的调度：保存每个 CPU 的运行任务，并实现从 idle 到用户任务的调度切换。
use core::mem::MaybeUninit;

use super::TaskControlBlock;
use super::{fetch_task, reap_exited_tasks};
use super::manager::complete_context_switch;
use crate::config::MAX_CPUS;
use crate::mm::AddressSpaceActivityHandle;
use crate::sync::{assert_no_spin_locks_held, CpuLocalLock};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(target_arch = "loongarch64")]
use core::sync::atomic::{fence, AtomicBool};
use lazyinit::LazyInit;
use log::*;
use polyhal::irq::IRQ;
use polyhal::kcontext::{context_switch_pt, read_current_tp, KContext, KContextArgs};
use polyhal::pagetable::PageTable;
/// State that belongs exclusively to the current hart.
///
/// The backing storage uses polyhal's `percpu` section, so every hart receives
/// a distinct `current`, idle context and timer counter.  The short
/// `CpuLocalLock` protects against local interrupt re-entry; it is never used
/// as a global scheduler lock.
pub struct CpuLocal {
    current: Option<Arc<TaskControlBlock>>,
    idle_task_cx: KContext,
    ticks: usize,
}

impl CpuLocal {
    // 初始化当前 CPU 的 processor 状态。
    // 执行时把当前任务置空，并准备一份 idle 上下文作为之后切换回内核的落脚点。
    pub fn new() -> Self {
        Self {
            current: None,
            idle_task_cx: KContext::blank(),
            ticks: 0,
        }
    }
    // 返回 idle 上下文指针，调度切换时作为内核落脚点。
    // 执行时返回 idle_task_cx 的裸指针，底层 context_switch 需要直接写这个上下文。
    fn get_idle_task_cx_ptr(&mut self) -> *mut KContext {
        &mut self.idle_task_cx as *mut _
    }
    // 取走当前正在运行的任务。
    // 执行时把 processor.current 置空，并把原来的任务引用交给阻塞或退出路径继续处理。
    pub fn take_current(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.current.take()
    }
    // 克隆一份当前任务引用。
    // 执行时不取走 current，只克隆 Arc，让系统调用能临时访问当前任务。
    pub fn current(&self) -> Option<Arc<TaskControlBlock>> {
        self.current.as_ref().map(Arc::clone)
    }
}

#[polyhal::percpu]
static PROCESSOR: MaybeUninit<CpuLocalLock<CpuLocal>> = MaybeUninit::uninit();

/// Map the boot protocol's stable hart ID to the unique polyhal per-CPU base.
///
/// The vendored RISC-V HAL currently reports CPU0 from `polyhal::hart_id()`
/// on secondary harts after its `gp`-based per-CPU setup.  The boot entry's
/// `hartid` argument is correct, and the per-CPU base remains unique across
/// user/kernel transitions, so this small registration table is the scheduler
/// authority for the fixed four-hart platform.
static HART_BY_PERCPU_BASE: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(usize::MAX) }; MAX_CPUS];

/// Pending IPI work, indexed by the stable QEMU hart ID. The producer sets a
/// bit before issuing the architecture-specific IPI; the receiving hart
/// clears it after acknowledging the interrupt. Keeping this state outside
/// `CpuLocal` is necessary:
/// a remote CPU must be able to publish its request without accessing another
/// hart's per-CPU storage directly.
const IPI_RESCHEDULE: usize = 1;
#[cfg(target_arch = "loongarch64")]
const IPI_TLB_FLUSH: usize = 1 << 1;
#[cfg(target_arch = "loongarch64")]
const LA_IPI_RESCHEDULE: u32 = 1 << 1;
#[cfg(target_arch = "loongarch64")]
const LA_IPI_TLB_FLUSH: u32 = 1 << 2;
#[cfg(target_arch = "loongarch64")]
const LA_IPI_KERNEL_MASK: u32 = LA_IPI_RESCHEDULE | LA_IPI_TLB_FLUSH;
static PENDING_IPI: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_CPUS];
#[cfg(target_arch = "loongarch64")]
static TLB_SHOOTDOWN_BUSY: AtomicBool = AtomicBool::new(false);
#[cfg(target_arch = "loongarch64")]
static TLB_GLOBAL_SEQ: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_arch = "loongarch64")]
static TLB_REQUEST_SEQ: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_CPUS];
#[cfg(target_arch = "loongarch64")]
static TLB_ACK_SEQ: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_CPUS];
#[cfg(all(target_arch = "loongarch64", feature = "smp-diagnostics"))]
static IPI_HANDLER_COUNT: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_CPUS];
#[cfg(all(target_arch = "loongarch64", feature = "smp-diagnostics"))]
static LAST_IPI_ACTIONS: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_CPUS];
static SCHEDULER_PHASE: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_CPUS];
static CURRENT_PID: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_CPUS];
static CURRENT_TGID: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_CPUS];
static CURRENT_TCB: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_CPUS];

#[derive(Clone, Copy, Debug)]
pub struct CpuSchedulerSnapshot {
    pub phase: usize,
    pub pid: usize,
    pub tgid: usize,
    pub tcb: usize,
    pub pending_ipi: usize,
}

pub fn scheduler_cpu_snapshots() -> [CpuSchedulerSnapshot; MAX_CPUS] {
    core::array::from_fn(|cpu| CpuSchedulerSnapshot {
        phase: SCHEDULER_PHASE[cpu].load(Ordering::Acquire),
        pid: CURRENT_PID[cpu].load(Ordering::Acquire),
        tgid: CURRENT_TGID[cpu].load(Ordering::Acquire),
        tcb: CURRENT_TCB[cpu].load(Ordering::Acquire),
        pending_ipi: PENDING_IPI[cpu].load(Ordering::Acquire),
    })
}

pub fn scheduler_phases() -> [usize; MAX_CPUS] {
    core::array::from_fn(|cpu| SCHEDULER_PHASE[cpu].load(Ordering::Acquire))
}

pub(crate) fn set_scheduler_phase(phase: usize) {
    SCHEDULER_PHASE[current_hart_id()].store(phase, Ordering::Release);
}

/// Initialize this hart's private processor state.
///
/// The boot loader sets polyhal's per-CPU pointer before entering `main`, so
/// this writes only the slot belonging to the current hart.  Every enabled
/// hart must call this exactly once before using scheduler APIs.
pub fn init_current_cpu(hart_id: usize) {
    assert!(
        hart_id < MAX_CPUS,
        "unsupported hart {}",
        hart_id
    );
    HART_BY_PERCPU_BASE[hart_id].store(polyhal::percpu::get_percpu_ptr(), Ordering::Release);
    PROCESSOR
        .ref_mut()
        .write(CpuLocalLock::new_named(CpuLocal::new(), "CpuLocal"));
    #[cfg(target_arch = "loongarch64")]
    {
        let pending = loongArch64::ipi::ipi_status();
        if pending != 0 {
            loongArch64::ipi::clear_ipi(pending);
        }
        loongArch64::ipi::enable_ipi(LA_IPI_KERNEL_MASK);
    }
}

/// Return the boot-protocol hart ID of the current CPU.
#[inline]
pub fn try_current_hart_id() -> Option<usize> {
    let base = polyhal::percpu::get_percpu_ptr();
    HART_BY_PERCPU_BASE
        .iter()
        .position(|entry| entry.load(Ordering::Acquire) == base)
}

/// Return the boot-protocol hart ID of the current CPU.
#[inline]
pub fn current_hart_id() -> usize {
    try_current_hart_id()
        .expect("current hart has not registered its per-CPU base")
}

/// Wake a remote idle hart so it promptly rechecks its local ready queue.
///
/// This function must be called after releasing SchedulerLock and every other
/// object lock. It intentionally does not force a running task to switch;
/// first-generation SMP only needs a prompt wakeup for an idle CPU.
pub fn request_reschedule(cpu: usize) {
    if cpu >= MAX_CPUS || cpu == current_hart_id() {
        return;
    }
    if (super::manager::online_cpu_mask() & (1usize << cpu)) == 0 {
        return;
    }
    PENDING_IPI[cpu].fetch_or(IPI_RESCHEDULE, Ordering::Release);
    #[cfg(target_arch = "riscv64")]
    {
        // QEMU virt/OpenSBI implements the standard sPI extension. A failed
        // request remains harmless: the target will still see the work on its
        // next timer interrupt.
        let _ = sbi_rt::send_ipi(1usize << cpu, 0);
    }
    #[cfg(target_arch = "loongarch64")]
    {
        // The IPI is an MMIO notification rather than an atomic operation on
        // PENDING_IPI. Publish the mailbox before making the hardware signal
        // observable by the target hart.
        fence(Ordering::SeqCst);
        loongArch64::ipi::send_ipi_single(cpu, LA_IPI_RESCHEDULE);
    }
}

/// Acknowledge a supervisor software interrupt on this hart and consume its
/// pending work bits. The scheduler loop will recheck its queue immediately
/// when this interrupt wakes it from `wfi`.
pub fn handle_ipi() -> usize {
    #[cfg(target_arch = "riscv64")]
    // SSIP is the architectural acknowledgement for an SBI-delivered IPI.
    // This function only runs from the RISC-V supervisor software-interrupt
    // trap arm, so the CSR operation is valid for the current hart.
    unsafe { riscv::register::sip::clear_ssoft() };
    #[cfg(target_arch = "loongarch64")]
    {
        let hardware_actions = loongArch64::ipi::ipi_status();
        #[cfg(feature = "smp-diagnostics")]
        {
            let cpu = current_hart_id();
            IPI_HANDLER_COUNT[cpu].fetch_add(1, Ordering::Relaxed);
            LAST_IPI_ACTIONS[cpu].store(hardware_actions as usize, Ordering::Release);
        }
        if hardware_actions != 0 {
            loongArch64::ipi::clear_ipi(hardware_actions);
            // Match Linux's smp_mb() after clearing IPI_STATUS. The hardware
            // notification orders neither the sender's mailbox writes nor
            // this hart's following read of PENDING_IPI by itself.
            fence(Ordering::SeqCst);
        }
    }
    let cpu = current_hart_id();
    let pending = PENDING_IPI[cpu].swap(0, Ordering::AcqRel);
    #[cfg(target_arch = "loongarch64")]
    if pending & IPI_TLB_FLUSH != 0 {
        let sequence = TLB_REQUEST_SEQ[cpu].load(Ordering::Acquire);
        polyhal::pagetable::TLB::flush_all();
        TLB_ACK_SEQ[cpu].store(sequence, Ordering::Release);
    }
    pending
}

#[cfg(target_arch = "loongarch64")]
fn service_local_ipi_while_waiting() {
    let cpu = current_hart_id();
    if loongArch64::ipi::ipi_status() != 0
        || PENDING_IPI[cpu].load(Ordering::Acquire) != 0
    {
        let _ = handle_ipi();
    }
}

#[cfg(target_arch = "loongarch64")]
struct TlbShootdownGuard;

#[cfg(target_arch = "loongarch64")]
impl TlbShootdownGuard {
    fn acquire() -> Self {
        let mut spins = 0usize;
        while TLB_SHOOTDOWN_BUSY
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            service_local_ipi_while_waiting();
            spins = spins.wrapping_add(1);
            if spins == 100_000_000 {
                panic!(
                    "LoongArch TLB shootdown gate timeout: cpu={}, online={:#x}",
                    current_hart_id(),
                    super::manager::online_cpu_mask()
                );
            }
            core::hint::spin_loop();
        }
        Self
    }
}

#[cfg(target_arch = "loongarch64")]
impl Drop for TlbShootdownGuard {
    fn drop(&mut self) {
        TLB_SHOOTDOWN_BUSY.store(false, Ordering::Release);
    }
}

/// Invalidate a virtual-address range on the other harts that currently use
/// the same user page table.
///
/// Callers retain `activity` while releasing every Task/Vm/VFS lock before
/// calling this helper. The local hart is intentionally excluded:
/// `PageTable::{map,unmap}_page` already executes the architecture's local
/// invalidation for every changed PTE. RISC-V uses OpenSBI RFENCE; LoongArch
/// sends an IPI and waits for an explicit acknowledgement after the remote
/// CPU flushes its complete TLB.
///
/// A target which leaves the address space while the request is in flight has
/// already switched to the kernel page table. Its next entry performs a local
/// full TLB flush before it publishes itself active again, so it is safe to
/// treat that transition as a completed quiescent state instead of waiting for
/// an IPI that raced with the context switch.
#[track_caller]
pub fn flush_remote_tlb(
    activity: &AddressSpaceActivityHandle,
    start_addr: usize,
    size: usize,
) -> bool {
    let current = current_hart_id();
    let remote = activity
        .active_cpu_mask()
        & super::manager::online_cpu_mask()
        & !(1usize << current);
    if remote == 0 {
        return true;
    }

    #[cfg(target_arch = "riscv64")]
    {
        let result = sbi_rt::remote_sfence_vma(remote, 0, start_addr, size);
        if result.is_err() {
            panic!(
                "remote sfence.vma failed: targets={:#x}, start={:#x}, size={:#x}, sbi={:?}",
                remote, start_addr, size, result
            );
        }
    }

    #[cfg(target_arch = "loongarch64")]
    {
        let _guard = TlbShootdownGuard::acquire();
        let mut sequence = TLB_GLOBAL_SEQ.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        if sequence == 0 {
            sequence = TLB_GLOBAL_SEQ.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        }

        for cpu in 0..MAX_CPUS {
            if remote & (1usize << cpu) == 0 {
                continue;
            }
            TLB_REQUEST_SEQ[cpu].store(sequence, Ordering::Release);
            PENDING_IPI[cpu].fetch_or(IPI_TLB_FLUSH, Ordering::Release);
        }
        // The IPI device is outside the cache-coherent atomic domain. Ensure
        // every target can observe its request before its interrupt arrives.
        fence(Ordering::SeqCst);
        for cpu in 0..MAX_CPUS {
            if remote & (1usize << cpu) != 0 {
                loongArch64::ipi::send_ipi_single(cpu, LA_IPI_TLB_FLUSH);
            }
        }

        let mut spins = 0usize;
        loop {
            service_local_ipi_while_waiting();
            let active_cpus = activity.active_cpu_mask();
            let mut waiting = usize::MAX;
            for cpu in 0..MAX_CPUS {
                if remote & (1usize << cpu) != 0
                    && active_cpus & (1usize << cpu) != 0
                    && TLB_ACK_SEQ[cpu].load(Ordering::Acquire) != sequence
                {
                    waiting = cpu;
                    break;
                }
            }
            if waiting == usize::MAX {
                break;
            }
            spins = spins.wrapping_add(1);
            if spins == 100_000_000 {
                let snapshot = scheduler_cpu_snapshots()[waiting];
                #[cfg(feature = "smp-diagnostics")]
                {
                    let caller = core::panic::Location::caller();
                    println!(
                        "[tlb-diag] target={} handled={} last_actions={:#x} request={} ack={} caller={}:{}",
                        waiting,
                        IPI_HANDLER_COUNT[waiting].load(Ordering::Acquire),
                        LAST_IPI_ACTIONS[waiting].load(Ordering::Acquire),
                        TLB_REQUEST_SEQ[waiting].load(Ordering::Acquire),
                        TLB_ACK_SEQ[waiting].load(Ordering::Acquire),
                        caller.file(),
                        caller.line(),
                    );
                }
                panic!(
                    "LoongArch TLB shootdown timeout: requester={}, target={}, seq={}, start={:#x}, size={:#x}, online={:#x}, active={:#x}, phase={}, pid={}, tgid={}, pending={:#x}",
                    current,
                    waiting,
                    sequence,
                    start_addr,
                    size,
                    super::manager::online_cpu_mask(),
                    active_cpus,
                    snapshot.phase,
                    snapshot.pid,
                    snapshot.tgid,
                    snapshot.pending_ipi,
                );
            }
            core::hint::spin_loop();
        }
    }

    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    let _ = (remote, start_addr, size);

    true
}

#[inline]
fn current_cpu() -> &'static CpuLocalLock<CpuLocal> {
    // `init_current_cpu` runs before any task or interrupt path uses this
    // accessor.  Each hart sees its own `percpu` copy of the MaybeUninit slot.
    unsafe { (&*PROCESSOR.get_mut_ptr()).assume_init_ref() }
}

// 调度主循环：不断取 Ready 任务并从 idle 上下文切过去。
// 执行时从就绪队列取任务，标记为 Running，记录到 Processor.current，然后切到任务上下文和用户页表。
pub fn run_tasks() {
    trace!("os::task::processor::run_tasks");
    let mut first = true;
    loop {
        let cpu_id = current_hart_id();
        SCHEDULER_PHASE[cpu_id].store(1, Ordering::Release);
        // When a task yields or blocks, control returns here only after its
        // task context has been written by `context_switch_pt`.  Complete the
        // state transition now, rather than before making the switch, so no
        // other hart can run stale register/context data.
        SCHEDULER_PHASE[cpu_id].store(2, Ordering::Release);
        if let Some(task) = take_current_task() {
            SCHEDULER_PHASE[cpu_id].store(3, Ordering::Release);
            complete_context_switch(task);
            SCHEDULER_PHASE[cpu_id].store(4, Ordering::Release);
        }
        reap_exited_tasks();
        SCHEDULER_PHASE[cpu_id].store(5, Ordering::Release);
        if let Some(task) = fetch_task() {
            SCHEDULER_PHASE[cpu_id].store(6, Ordering::Release);
            // The scheduler already performed Ready -> Running under its
            // lock.  Only snapshot the context and page table here.
            let mut task_inner = task.inner_exclusive_access();
            // A task context includes the kernel thread pointer used by
            // `current_task()` and all `#[percpu]` data.  It may have been
            // created on a different hart, so refresh it immediately before
            // dispatching rather than restoring the creator's CPU-local base.
            task_inner.task_cx[KContextArgs::KTP] = read_current_tp();
            #[cfg(target_arch = "riscv64")]
            {
                // OpenSBI completes remote sfence.vma independently of the
                // target hart's interrupt state. Publish before the switch so
                // an unmap cannot race the SATP handoff; the local page-table
                // switch performs its own full TLB invalidation afterwards.
                task_inner.memory_set.enter_cpu(current_hart_id());
            }
            let pid = task.getpid();
            let tgid = task_inner.tgid;
            let tcb = Arc::as_ptr(&task) as usize;
            CURRENT_PID[cpu_id].store(pid, Ordering::Release);
            CURRENT_TGID[cpu_id].store(tgid, Ordering::Release);
            CURRENT_TCB[cpu_id].store(tcb, Ordering::Release);
            if first {
                println!(
                    "[smp] hart {}: first dispatched tid={} tgid={} tcb={:#x}",
                    current_hart_id(),
                    pid,
                    tgid,
                    tcb
                );
                first = false;
            }
            let next_task_cx_ptr = &task_inner.task_cx as *const KContext;
            let token = task_inner.memory_set.token();
            drop(task_inner);
            // Publishing `current` and entering its saved context form one
            // ownership handoff.  A timer in between used to preempt the idle
            // stack as though the selected task were already executing,
            // clear `current`, and then let the outer switch enter that task
            // with no CPU owner.  User return (or the idle path below)
            // re-enables interrupts after the handoff is complete.
            IRQ::int_disable();
            SCHEDULER_PHASE[cpu_id].store(7, Ordering::Release);
            // current 记录本 CPU 正在运行的任务。 这个局部锁在切换前
            // 释放，绝不跨越 context switch。
            let idle_task_cx_ptr = {
                let mut cpu = current_cpu().exclusive_access();
                cpu.current = Some(task);
                cpu.get_idle_task_cx_ptr()
            };
            // 从 idle 切到用户任务，同时切换到该任务的用户页表。
            unsafe { context_switch_pt(idle_task_cx_ptr, next_task_cx_ptr, token) }
        } else {
            SCHEDULER_PHASE[cpu_id].store(8, Ordering::Release);
            // The idle context saved by a context switch does not carry the
            // interrupted task's interrupt-enable state.  Explicitly enable
            // local interrupts before waiting so timer deadlines can wake a
            // fully blocked system (notably futex and signal timeouts).
            assert_no_spin_locks_held("idle wait");
            IRQ::int_enable();
            #[cfg(target_arch = "riscv64")]
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack))
            };
            #[cfg(target_arch = "loongarch64")]
            unsafe {
                loongArch64::asm::idle()
            };
            #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
            core::hint::spin_loop();
        }
    }
}

// 对外取走当前任务，通常在阻塞或退出时调用。
// 执行时进入全局 Processor，把当前 CPU 上运行的任务移交给调用方。
pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    let task = current_cpu().exclusive_access().take_current();
    if let Some(task) = &task {
        let cpu = current_hart_id();
        CURRENT_PID[cpu].store(0, Ordering::Release);
        CURRENT_TGID[cpu].store(0, Ordering::Release);
        CURRENT_TCB[cpu].store(0, Ordering::Release);
        // `schedule` restored BOOT_PAGE_TABLE before execution returned to
        // this idle loop, so this CPU may no longer receive user-page-table
        // shootdowns for the outgoing task.
        task.inner_exclusive_access().memory_set.leave_cpu(cpu);
        task.clear_running_on(cpu);
    }
    task
}

// 返回当前任务引用。
// 执行时进入全局 Processor，克隆当前正在 CPU 上运行的任务引用。
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    current_cpu().exclusive_access().current()
}

/// Publish the current task's address space after its page table is installed.
///
/// `context_switch_pt` invalidates the local TLB before entering a task.  The
/// activity bit therefore only needs to cover the interval in which this hart
/// can execute user mappings.  Publishing it before the context switch would
/// make a remote unmap wait for a hart that still has interrupts disabled in
/// the scheduler handoff.
pub fn mark_current_task_address_space_active() {
    let task = current_task().expect("activate address space without current task");
    task.inner_exclusive_access()
        .memory_set
        .enter_cpu(current_hart_id());
}

#[cfg(target_arch = "loongarch64")]
const TIMER_TICKS_PER_USER_SLICE: usize = 10;
#[cfg(not(target_arch = "loongarch64"))]
const TIMER_TICKS_PER_USER_SLICE: usize = 1;

/// Account a timer interrupt and report whether the current user time slice
/// has expired. LoongArch keeps its 1 ms timer resolution for deadlines but
/// uses a 10 ms scheduling quantum, matching the RISC-V timer cadence.
pub fn record_timer_tick() -> bool {
    let mut cpu = current_cpu().exclusive_access();
    cpu.ticks = cpu.ticks.wrapping_add(1);
    cpu.ticks % TIMER_TICKS_PER_USER_SLICE == 0
}

// 返回当前任务的用户页表 token。
// 执行时从当前任务 inner 里取 memory_set 的页表 token，用户指针翻译会用到。
#[track_caller]
pub fn current_user_token() -> PageTable {
    let task = current_task().expect("current_user_token without a current task");
    let token = task.inner_exclusive_access().get_user_token();
    token
}

static BOOT_PAGE_TABLE: LazyInit<PageTable> = LazyInit::new();

// 从当前任务切回 idle 上下文，等待下一轮调度。
// 执行时取出 idle 上下文指针，并用启动时记录的内核页表切回调度主循环。
pub fn schedule(switched_task_cx_ptr: *mut KContext) {
    assert_no_spin_locks_held("schedule");
    // Keep the current-task ownership handoff atomic with local interrupt
    // handlers while context_switch_pt saves or restores the task context.
    let restore_interrupts = IRQ::int_enabled();
    IRQ::int_disable();
    let idle_task_cx_ptr = current_cpu().exclusive_access().get_idle_task_cx_ptr();
    // 当前任务主动让出或退出时，先切回 idle，并恢复启动时记录的内核页表。
    unsafe {
        context_switch_pt(switched_task_cx_ptr, idle_task_cx_ptr, *BOOT_PAGE_TABLE);
    }
    // This continuation runs only after a previously saved task context has
    // been restored with its user page table.  Mark it active before returning
    // to the syscall/trap path, which may resume userspace immediately.
    mark_current_task_address_space_active();
    if restore_interrupts {
        IRQ::int_enable();
    }
}

// 记录启动时的内核页表，之后调度回 idle 时使用。
// 执行时只初始化一次 BOOT_PAGE_TABLE，供 schedule 恢复内核地址空间。
pub fn init_kernel_page() {
    BOOT_PAGE_TABLE.init_once(PageTable::current());
}
