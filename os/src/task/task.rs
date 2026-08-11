use super::{current_task, mark_current_task_address_space_active, SignalActions, UserFloatState};
use super::{pid_alloc, PendingSignalInfo, PidHandle, SignalFlags};
use crate::config::{KERNEL_STACK_SIZE, PAGE_SIZE, USER_STACK_SIZE};
use crate::fs::vfs::{Dentry, ROOT_DENTRY};
use crate::fs::{File, Stdin, Stdout};
use crate::mm::{
    translated_refmut, LoadedElf, MapArea, MapPermission, MapType, MemorySet, PageFaultAccess,
};
use crate::sync::{SignalActionsLock, TaskGuard, TaskLock};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use polyhal::irq::IRQ;
use polyhal::kcontext::{read_current_tp, KContext, KContextArgs};
use polyhal::pagetable::PageTable;
use polyhal_trap::trap::run_user_task;
use polyhal_trap::trapframe::{TrapFrame, TrapFrameArgs};

pub struct TaskControlBlock {
    // immutable
    pub pid: PidHandle,
    /// Lock-free scheduler ownership metadata.
    ///
    /// `inner.task_status` remains the compatibility-facing task state during
    /// this transition.  These atomics give the scheduler a separate source of
    /// truth for CPU ownership and queue membership, so it can later perform a
    /// Ready -> Running handoff while holding `SchedulerLock` without exposing
    /// a partially moved task to another hart.
    scheduling: TaskSchedulingMeta,
    runtime: TaskRuntime,
    // mutable
    /// Mutable per-task state.  Scheduler queue membership remains owned by
    /// `SchedulerLock`; callers must not retain this guard while scheduling,
    /// translating user pointers, performing I/O, or invoking callbacks.
    inner: TaskLock<TaskControlBlockInner>,
}

/// Sentinel used when a task is not executing on any CPU.
pub const NO_RUNNING_CPU: usize = usize::MAX;

/// Per-task data that must be observable without taking the broad TaskLock.
///
/// RISC-V/QEMU starts with the fixed four-hart topology enabled after boot.
/// Other architectures retain the old single-CPU default. Shared-address-
/// space clone groups may run on multiple harts: every page-table mutation
/// records the active CPU mask and completes a remote TLB shootdown before
/// recycled frames become available again.
struct TaskSchedulingMeta {
    running_cpu: AtomicUsize,
    last_cpu: AtomicUsize,
    allowed_cpus: AtomicUsize,
    enqueued: AtomicBool,
    /// Stable task-list identity used by exec/exit group walks without taking
    /// another task's broad inner lock.
    thread_group_id: AtomicUsize,
    /// Published with release ordering when begin_exit has committed the
    /// terminal status and exit code.  A parent may reap this zombie before
    /// the deferred idle-context resource cleanup completes.
    waitable_zombie: AtomicBool,
    exit_cleanup_done: AtomicBool,
    /// Non-zero while this task is a vfork child whose parent must remain
    /// suspended.  The value is the parent's TID, allowing exec/exit to
    /// complete the handoff without taking either task's broad inner lock.
    vfork_parent: AtomicUsize,
}

struct TaskRuntime {
    utime: AtomicUsize,
    stime: AtomicUsize,
    last_time: AtomicUsize,
}

impl TaskRuntime {
    fn new() -> Self {
        Self {
            utime: AtomicUsize::new(0),
            stime: AtomicUsize::new(0),
            last_time: AtomicUsize::new(polyhal::time::Time::now().to_nsec() as usize),
        }
    }

    fn charge(&self, counter: &AtomicUsize) {
        let now = polyhal::time::Time::now().to_nsec() as usize;
        let last = self.last_time.swap(now, Ordering::Relaxed);
        counter.fetch_add(now.saturating_sub(last), Ordering::Relaxed);
    }
}

impl TaskSchedulingMeta {
    const fn new(thread_group_id: usize) -> Self {
        Self {
            running_cpu: AtomicUsize::new(NO_RUNNING_CPU),
            last_cpu: AtomicUsize::new(NO_RUNNING_CPU),
            allowed_cpus: AtomicUsize::new(DEFAULT_TASK_CPU_MASK),
            enqueued: AtomicBool::new(false),
            thread_group_id: AtomicUsize::new(thread_group_id),
            waitable_zombie: AtomicBool::new(false),
            exit_cleanup_done: AtomicBool::new(false),
            vfork_parent: AtomicUsize::new(0),
        }
    }
}

const DEFAULT_TASK_CPU_MASK: usize = (1usize << crate::config::PLATFORM_CPUS) - 1;
use log::*;
impl Drop for TaskControlBlock {
    fn drop(&mut self) {
        trace!("drop task control block");
    }
}

pub struct KernelStack {
    inner: Box<[u128]>,
}

#[cfg(feature = "smp-diagnostics")]
const KERNEL_STACK_SENTINEL: u128 = 0x6b737461636b6775_6172647363696e74;
#[cfg(feature = "smp-diagnostics")]
const KERNEL_STACK_GUARD_WORDS: usize = 32;
#[cfg(feature = "smp-diagnostics")]
static KERNEL_STACK_HIGH_WATER: AtomicUsize = AtomicUsize::new(0);

impl KernelStack {
    pub fn new() -> Self {
        #[cfg(feature = "smp-diagnostics")]
        let initial_word = KERNEL_STACK_SENTINEL;
        #[cfg(not(feature = "smp-diagnostics"))]
        let initial_word = 0;
        Self {
            // Building `[0; KERNEL_STACK_SIZE]` inside `Arc::new` materializes
            // the full array in the caller's kernel stack before allocation.
            // fork therefore needed more stack than the child stack it was
            // creating. Vec initializes the storage directly on the heap.
            inner: alloc::vec![initial_word; KERNEL_STACK_SIZE / size_of::<u128>()]
                .into_boxed_slice(),
        }
    }

    pub fn get_position(&self) -> (usize, usize) {
        let bottom = self.inner.as_ptr() as usize;
        (bottom, bottom + KERNEL_STACK_SIZE)
    }

    #[cfg(feature = "smp-diagnostics")]
    pub fn validate_saved_sp(&self, saved_sp: usize, pid: usize) {
        let (bottom, top) = self.get_position();
        let guard_end = bottom + KERNEL_STACK_GUARD_WORDS * size_of::<u128>();
        assert!(
            (guard_end..=top).contains(&saved_sp),
            "pid {} saved kernel SP {:#x} outside stack {:#x}..{:#x}",
            pid,
            saved_sp,
            guard_end,
            top,
        );
        assert!(
            self.inner[..KERNEL_STACK_GUARD_WORDS]
                .iter()
                .all(|word| *word == KERNEL_STACK_SENTINEL),
            "pid {} overflowed its kernel stack guard at {:#x}",
            pid,
            bottom,
        );

        let first_used = self
            .inner
            .iter()
            .position(|word| *word != KERNEL_STACK_SENTINEL)
            .unwrap_or(self.inner.len());
        let used = KERNEL_STACK_SIZE - first_used * size_of::<u128>();
        let previous = KERNEL_STACK_HIGH_WATER.fetch_max(used, Ordering::Relaxed);
        if used > previous && used / 1024 > previous / 1024 {
            crate::println!(
                "[kstack-diag] pid={} used={} bytes saved_sp={:#x}",
                pid,
                used,
                saved_sp,
            );
        }
    }
}

pub struct TaskControlBlockInner {
    pub trap_cx: TrapFrame,
    pub fp_state: UserFloatState,
    pub base_size: usize,
    pub task_cx: KContext,
    pub task_status: TaskStatus,
    pub memory_set: MemorySet,
    pub kernel_stack: KernelStack,
    pub parent: Option<Weak<TaskControlBlock>>,
    pub children: Vec<Arc<TaskControlBlock>>,
    pub exit_code: i32,
    pub fd_table: Vec<Option<Arc<dyn File + Send + Sync>>>,
    pub fd_cloexec: Vec<bool>,
    /// Identifies the Linux files_struct shared through CLONE_FILES.
    pub files_id: usize,
    pub rlimit_nofile: usize,
    pub rlimit_nofile_max: usize,
    pub rlimit_memlock: usize,
    pub rlimit_memlock_max: usize,
    pub cwd: Arc<Dentry>,
    pub root: Arc<Dentry>,
    pub executable_path: String,
    pub tid_ptr: usize,
    pub tgid: usize,
    pub pgid: usize,
    pub sid: usize,
    pub uid: u32,
    pub euid: u32,
    pub suid: u32,
    pub gid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub supplementary_groups: Vec<u32>,
    pub umask: u32,
    pub nice: i32,
    pub signals: SignalFlags,
    pub pending_signal_info: [PendingSignalInfo; super::MAX_SIG + 1],
    pub signal_mask: SignalFlags,
    pub signal_mask_backup: Option<SignalFlags>,
    /// `rt_sigsuspend` restores its caller's mask before returning EINTR, but
    /// the signal that woke it still has to be delivered at this trap exit.
    /// This records that one selected pending signal without changing its
    /// disposition or losing the mask restored by the syscall.
    pub sigsuspend_delivery: Option<SignalFlags>,
    // the signal which is being handling
    pub handling_sig: isize,
    // Signal actions
    /// Signal dispositions are process-wide when CLONE_SIGHAND is used.
    ///
    /// Keeping the table behind a separately locked Arc avoids the former
    /// best-effort per-thread propagation, which could silently miss a thread
    /// running on another hart.
    pub signal_actions: Arc<SignalActionsLock<SignalActions>>,
    // if the task is killed
    pub killed: bool,
    // Set while execve is retiring this sibling thread. Unlike a process-wide
    // fatal signal, this must not make the retiring thread kill the execing
    // thread group leader.
    pub terminating_for_exec: bool,
    // Set for siblings selected by exit_group or a process-wide fatal fault.
    // The stored exit_code is authoritative even when it is zero.
    pub terminating_for_group: bool,
    // if the task is frozen by a signal
    pub frozen: bool,
    pub trap_ctx_backup: Option<TrapFrame>,
    pub signal_ucontext: Option<usize>,
    // record the stack
    pub heap_bottom: usize,
    pub program_brk: usize,
    pub sleeping: Option<u64>,
    pub cutime: usize,
    pub cstime: usize,
    pub block_reason: Option<BlockReason>,
    pub itimer_expire: u64,
    pub itimer_interval_us: u64,
    pub child_tid: usize,
    pub robust_list_head: usize,
    pub robust_list_len: usize,
    pub alt_stack: usize,
    pub alt_stack_size: usize,
    pub alt_stack_flags: u32,
    pub in_syscall: bool,
    pub exit_signal: i32,
}

impl TaskControlBlockInner {
    pub fn get_trap_cx(&self) -> &'static mut TrapFrame {
        let paddr = &self.trap_cx as *const TrapFrame as usize as *mut TrapFrame;
        // let paddr: PhysAddr = self.trap_cx.into();
        // unsafe { paddr.get_mut_ptr::<TrapFrame>().as_mut().unwrap() }
        unsafe { paddr.as_mut().unwrap() }
    }
    pub fn get_user_token(&self) -> PageTable {
        self.memory_set.token()
    }
    fn get_status(&self) -> TaskStatus {
        self.task_status
    }
    pub fn is_zombie(&self) -> bool {
        self.get_status() == TaskStatus::Zombie
    }
    pub fn alloc_fd(&mut self) -> usize {
        let max_fd = self.rlimit_nofile.min(1024);
        let search_end = self.fd_table.len().min(max_fd);
        if let Some(fd) = (0..search_end).find(|fd| self.fd_table[*fd].is_none()) {
            self.fd_cloexec[fd] = false;
            fd
        } else if self.fd_table.len() < max_fd {
            self.fd_table.push(None);
            self.fd_cloexec.push(false);
            self.fd_table.len() - 1
        } else {
            max_fd
        }
    }
    pub fn take_fd_for_close(&mut self, fd: usize) -> Option<Arc<dyn File + Send + Sync>> {
        if fd < self.fd_table.len() {
            self.fd_cloexec[fd] = false;
            return self.fd_table[fd].take();
        }
        None
    }

    pub fn drain_fd_table(&mut self) -> Vec<Arc<dyn File + Send + Sync>> {
        let mut files = Vec::new();
        for slot in self.fd_table.iter_mut() {
            if let Some(file) = slot.take() {
                files.push(file);
            }
        }
        self.fd_table.clear();
        self.fd_cloexec.clear();
        files
    }

    pub fn take_cloexec_fds(&mut self) -> Vec<Arc<dyn File + Send + Sync>> {
        let mut files = Vec::new();
        let len = self.fd_table.len().min(self.fd_cloexec.len());
        for fd in 0..len {
            if self.fd_cloexec[fd] {
                if let Some(file) = self.fd_table[fd].take() {
                    files.push(file);
                }
                self.fd_cloexec[fd] = false;
            }
        }
        files
    }
}

fn task_entry() {
    trace!("os::task::task_entry");
    // The scheduler has installed this task's page table and invalidated this
    // hart's local TLB.  Publish it before the first return to userspace.
    mark_current_task_address_space_active();
    #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
    crate::SMP_FIRST_TASK_ENTERED.store(true, Ordering::Release);
    // A kernel context is permanently paired with one TCB and kernel stack.
    // exec replaces the TrapFrame value in place, so its address stays valid
    // across traps, scheduling and migration for the lifetime of this task.
    let task = current_task().unwrap();
    let trap_cx = task.inner.exclusive_access().get_trap_cx() as *mut TrapFrame;
    loop {
        task.restore_user_float_state();
        let ctx_mut = unsafe { trap_cx.as_mut().unwrap() };
        // sscratch identifies a user trap frame while user_restore is loading
        // registers. Keep that handoff non-reentrant; sret restores the user
        // interrupt state from the trap frame.
        IRQ::int_disable();
        run_user_task(ctx_mut);
    }
}

pub fn blank_kcontext(ksp: usize) -> KContext {
    let mut kcx = KContext::blank();
    kcx[KContextArgs::KPC] = task_entry as usize;
    kcx[KContextArgs::KSP] = ksp;
    kcx[KContextArgs::KTP] = read_current_tp();
    kcx
}

impl TaskControlBlock {
    #[track_caller]
    pub fn inner_exclusive_access(&self) -> TaskGuard<'_, TaskControlBlockInner> {
        self.inner.exclusive_access()
    }
    pub fn from_raw(pid: PidHandle, inner: TaskControlBlockInner) -> Self {
        let thread_group_id = inner.tgid;
        Self {
            pid,
            scheduling: TaskSchedulingMeta::new(thread_group_id),
            runtime: TaskRuntime::new(),
            inner: TaskLock::new_named(inner, "TaskInner"),
        }
    }
    #[track_caller]
    pub fn try_inner_exclusive_access(&self) -> Option<TaskGuard<'_, TaskControlBlockInner>> {
        match self.inner.try_borrow_mut() {
            Some(g) => Some(g),
            None => None,
        }
    }
    pub fn try_inner_borrow(&self) -> Option<TaskGuard<'_, TaskControlBlockInner>> {
        match self.inner.try_borrow() {
            Some(g) => Some(g),
            None => None,
        }
    }
    pub fn save_user_float_state(&self) {
        if !super::user_float_state_is_live(self) {
            return;
        }
        let mut inner = self.inner_exclusive_access();
        inner.fp_state.save_current();
    }
    pub fn restore_user_float_state(&self) {
        if super::user_float_state_is_live(self) {
            return;
        }
        let fp_state = {
            let inner = self.inner_exclusive_access();
            inner.fp_state
        };
        fp_state.restore_current();
        super::mark_user_float_state_live(self);
    }
    pub fn new(elf_data: &[u8]) -> Self {
        trace!("os::task::TaskControlBlock::new");
        let LoadedElf {
            memory_set,
            user_sp,
            entry_point,
            tls_base,
            tls_size,
            ..
        } = MemorySet::from_elf(elf_data);

        let heap_bottom = user_sp;
        memory_set.set_program_brk(heap_bottom);
        // alloc a pid and a kernel stack in kernel space
        let pid_handle = pid_alloc();
        let pid_value = pid_handle.0;
        let kstack = KernelStack::new();
        let task_control_block = Self {
            pid: pid_handle,
            scheduling: TaskSchedulingMeta::new(pid_value),
            runtime: TaskRuntime::new(),
            inner: TaskLock::new_named(
                TaskControlBlockInner {
                    trap_cx: TrapFrame::new(),
                    fp_state: UserFloatState::new(),
                    base_size: user_sp,
                    task_cx: blank_kcontext(kstack.get_position().1), // Set task_cx's Kernel Stack Top
                    task_status: TaskStatus::Ready,
                    memory_set,
                    parent: None,
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table: vec![
                        // 0 -> stdin
                        Some(Arc::new(Stdin::new())),
                        // 1 -> stdout
                        Some(Arc::new(Stdout::new())),
                        // 2 -> stderr
                        Some(Arc::new(Stdout::new())),
                    ],
                    fd_cloexec: vec![false, false, false],
                    files_id: pid_value,
                    rlimit_nofile: 1024,
                    rlimit_nofile_max: 1024,
                    rlimit_memlock: 64 * 1024,
                    rlimit_memlock_max: 64 * 1024,
                    cwd: ROOT_DENTRY.clone(),
                    root: ROOT_DENTRY.clone(),
                    executable_path: String::from("/initproc"),
                    tid_ptr: 0,
                    tgid: pid_value,
                    pgid: pid_value,
                    sid: pid_value,
                    uid: 0,
                    euid: 0,
                    suid: 0,
                    gid: 0,
                    egid: 0,
                    sgid: 0,
                    supplementary_groups: Vec::new(),
                    umask: 0o022,
                    nice: 0,
                    signals: SignalFlags::empty(),
                    pending_signal_info: [PendingSignalInfo::default(); super::MAX_SIG + 1],
                    signal_mask: SignalFlags::empty(),
                    signal_mask_backup: None,
                    sigsuspend_delivery: None,
                    handling_sig: -1,
                    signal_actions: Arc::new(SignalActionsLock::new_named(
                        SignalActions::default(),
                        "SignalActions",
                    )),
                    killed: false,
                    terminating_for_exec: false,
                    terminating_for_group: false,
                    frozen: false,
                    trap_ctx_backup: None,
                    signal_ucontext: None,
                    kernel_stack: kstack,
                    // added
                    heap_bottom,
                    program_brk: heap_bottom,
                    sleeping: None,
                    cutime: 0,
                    cstime: 0,
                    block_reason: None,
                    itimer_expire: 0,
                    itimer_interval_us: 0,
                    child_tid: 0,
                    robust_list_head: 0,
                    robust_list_len: 0,
                    alt_stack: 0,
                    alt_stack_size: 0,
                    alt_stack_flags: 0,
                    in_syscall: false,
                    exit_signal: 0,
                },
                "TaskInner",
            ),
        };
        // prepare TrapContext in user space
        let tp_val = {
            let mut inner = task_control_block.inner_exclusive_access();
            inner.trap_cx[TrapFrameArgs::SEPC] = entry_point;
            inner.trap_cx[TrapFrameArgs::SP] = user_sp;
            inner.trap_cx[TrapFrameArgs::ARG0] = 0;
            inner.trap_cx[TrapFrameArgs::ARG1] = 0;
            inner.trap_cx[TrapFrameArgs::ARG2] = 0;
            if tls_size > 0 {
                tls_base + tls_size
            } else {
                let max_va: usize = inner.memory_set.max_end_vaddr().into();
                let tls_va = (max_va + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE
                    * crate::config::PAGE_SIZE;
                let ok = inner.memory_set.insert_framed_area(
                    tls_va.into(),
                    (tls_va + crate::config::PAGE_SIZE).into(),
                    crate::mm::MapPermission::R
                        | crate::mm::MapPermission::W
                        | crate::mm::MapPermission::U,
                );
                if ok {
                    tls_va + crate::config::PAGE_SIZE
                } else {
                    0
                }
            }
        };
        let trap_cx = task_control_block.inner_exclusive_access().get_trap_cx();
        trap_cx[TrapFrameArgs::TLS] = tp_val;
        task_control_block
    }
    pub fn exec(&self, elf_data: &[u8], args: Vec<String>, envs: Vec<String>) {
        self.exec_with_interp(elf_data, None, args, envs);
    }

    pub fn exec_with_interp(
        &self,
        elf_data: &[u8],
        interp_data: Option<&[u8]>,
        args: Vec<String>,
        envs: Vec<String>,
    ) {
        trace!("os::task::TaskControlBlock::exec_with_interp");
        const INTERP_BASE: usize = 0x10_0000_0000;

        let mut loaded = MemorySet::from_elf(elf_data);
        if let Some(id) = interp_data {
            let (interp_entry, interp_base) =
                MemorySet::load_interp_elf(&mut loaded.memory_set, id, INTERP_BASE);
            loaded.entry_point = interp_entry;
            loaded.interp_base = interp_base;
        }
        self.exec_loaded(loaded, args, envs);
    }

    pub fn exec_file_with_interp(
        &self,
        elf_file: Arc<dyn File>,
        interp_file: Option<Arc<dyn File>>,
        args: Vec<String>,
        envs: Vec<String>,
    ) -> bool {
        trace!("os::task::TaskControlBlock::exec_file_with_interp");
        const INTERP_BASE: usize = 0x10_0000_0000;

        let mut loaded = match MemorySet::from_elf_file(elf_file) {
            Some(loaded) => loaded,
            None => {
                #[cfg(feature = "smp-diagnostics")]
                crate::println!("[exec-diag] tid={} main ELF load failed", self.getpid());
                return false;
            }
        };
        if let Some(file) = interp_file {
            let Some((interp_entry, interp_base)) =
                MemorySet::load_interp_file(&mut loaded.memory_set, file, INTERP_BASE)
            else {
                #[cfg(feature = "smp-diagnostics")]
                crate::println!(
                    "[exec-diag] tid={} interpreter ELF load failed",
                    self.getpid()
                );
                return false;
            };
            loaded.entry_point = interp_entry;
            loaded.interp_base = interp_base;
        }
        if !Self::prepare_initial_stack(&loaded.memory_set, loaded.user_sp, &args, &envs) {
            #[cfg(feature = "smp-diagnostics")]
            crate::println!(
                "[exec-diag] tid={} initial stack allocation failed",
                self.getpid()
            );
            return false;
        }
        self.exec_loaded(loaded, args, envs);
        true
    }

    fn prepare_initial_stack(
        memory_set: &MemorySet,
        stack_top: usize,
        args: &[String],
        envs: &[String],
    ) -> bool {
        const AUXV_COUNT: usize = 18;
        let platform_len = if cfg!(target_arch = "riscv64") {
            "riscv64\0".len()
        } else if cfg!(target_arch = "loongarch64") {
            "loongarch64\0".len()
        } else {
            "unknown\0".len()
        };
        let Some(string_bytes) = args
            .iter()
            .chain(envs.iter())
            .try_fold(platform_len, |total, value| {
                total.checked_add(value.len() + 1)
            })
        else {
            return false;
        };
        let Some(pointer_bytes) = args
            .len()
            .checked_add(envs.len())
            .and_then(|count| count.checked_add(3))
            .and_then(|count| count.checked_mul(core::mem::size_of::<u64>()))
        else {
            return false;
        };
        let Some(required_bytes) = string_bytes
            .checked_add(15)
            .and_then(|size| size.checked_add(16))
            .and_then(|size| size.checked_add(AUXV_COUNT * 2 * core::mem::size_of::<u64>()))
            .and_then(|size| size.checked_add(pointer_bytes))
        else {
            return false;
        };
        if required_bytes > USER_STACK_SIZE {
            return false;
        }
        let Some(required_bottom) = stack_top.checked_sub(required_bytes) else {
            return false;
        };
        let mapped_bottom = required_bottom / PAGE_SIZE * PAGE_SIZE;
        memory_set.populate_lazy_range(
            mapped_bottom.into(),
            stack_top.into(),
            PageFaultAccess::Write,
        )
    }

    fn exec_loaded(&self, loaded: LoadedElf, args: Vec<String>, envs: Vec<String>) {
        let entry_point = loaded.entry_point;
        let program_entry = loaded.program_entry;
        let program_break = loaded.program_break;
        let stack_top = loaded.user_sp;
        let heap_bottom = program_break;
        let ph_addr = loaded.ph_addr;
        let ph_entsize = loaded.ph_entsize;
        let ph_num = loaded.ph_num;
        let tls_base = loaded.tls_base;
        let tls_size = loaded.tls_size;
        let interp_base = loaded.interp_base;
        let memory_set = loaded.memory_set;

        memory_set.set_program_brk(heap_bottom);
        #[cfg(feature = "smp-diagnostics")]
        crate::println!(
            "[exec-diag] tid={} tgid={} entry={:#x} program_entry={:#x} phdr={:#x} phent={} phnum={} stack_top={:#x} brk={:#x} tls={:#x}+{:#x} interp_base={:#x} argc={} envc={}",
            self.getpid(),
            self.inner_exclusive_access().tgid,
            entry_point,
            program_entry,
            ph_addr,
            ph_entsize,
            ph_num,
            stack_top,
            heap_bottom,
            tls_base,
            tls_size,
            interp_base,
            args.len(),
            envs.len(),
        );

        // Keep the TCB's address-space metadata in step with the hardware
        // page-table switch. A timer may schedule from an interruptible exec
        // syscall, so publishing the replacement only after `activate()`
        // would let the scheduler clear or mark the wrong activity bitmap.
        let restore_interrupts = IRQ::int_enabled();
        IRQ::int_disable();
        let cpu = crate::task::current_hart_id();
        let retired_memory_set = {
            let mut inner = self.inner_exclusive_access();
            inner.memory_set.leave_cpu(cpu);
            core::mem::replace(&mut inner.memory_set, memory_set)
        };
        let token = {
            let inner = self.inner_exclusive_access();
            inner.memory_set.activate();
            inner.memory_set.enter_cpu(cpu);
            inner.memory_set.token()
        };
        // The TCB and hardware page table now agree. The rest of exec builds
        // userspace data and may release file-backed mappings, so it must remain
        // interruptible rather than extending this atomic handoff window.
        if restore_interrupts {
            IRQ::int_enable();
        }
        // The processor no longer uses the old page table, so its frames may
        // be released before constructing the new initial stack.
        drop(retired_memory_set);
        let argc = args.len();
        let envc = envs.len();

        const AT_NULL: u64 = 0;
        const AT_PHDR: u64 = 3;
        const AT_PHENT: u64 = 4;
        const AT_PHNUM: u64 = 5;
        const AT_PAGESZ: u64 = 6;
        const AT_BASE: u64 = 7;
        const AT_FLAGS: u64 = 8;
        const AT_ENTRY: u64 = 9;
        const AT_UID: u64 = 11;
        const AT_EUID: u64 = 12;
        const AT_GID: u64 = 13;
        const AT_EGID: u64 = 14;
        const AT_CLKTCK: u64 = 17;
        const AT_SECURE: u64 = 23;
        const AT_RANDOM: u64 = 25;
        const AT_HWCAP: u64 = 16;
        const AT_PLATFORM: u64 = 15;
        const AUXV_COUNT: usize = 18;

        // Stack layout (high to low):
        //   [platform str] [env strings] [arg strings] [16-align] [random 16B]
        //   [auxv entries][AT_NULL] [envp NULL] [envp ptrs] [argv NULL] [argv ptrs] [argc]

        let mut sp = stack_top;

        // Step 0: write AT_PLATFORM string at top of stack
        let platform_str = if cfg!(target_arch = "riscv64") {
            "riscv64\0"
        } else if cfg!(target_arch = "loongarch64") {
            "loongarch64\0"
        } else {
            "unknown\0"
        };
        sp -= platform_str.len();
        let platform_addr = sp;
        for (i, &b) in platform_str.as_bytes().iter().enumerate() {
            *translated_refmut(token, (platform_addr + i) as *mut u8) = b;
        }

        // Step 1: write arg/env strings at top of stack
        let mut string_addrs: Vec<usize> = Vec::with_capacity(argc);
        for arg in args.iter().rev() {
            sp -= arg.len() + 1;
            let addr = sp;
            string_addrs.push(addr);
            for (i, &b) in arg.as_bytes().iter().enumerate() {
                *translated_refmut(token, (addr + i) as *mut u8) = b;
            }
            *translated_refmut(token, (addr + arg.len()) as *mut u8) = 0;
        }
        string_addrs.reverse();
        let mut env_addrs: Vec<usize> = Vec::with_capacity(envc);
        for env in envs.iter().rev() {
            sp -= env.len() + 1;
            let addr = sp;
            env_addrs.push(addr);
            for (i, &b) in env.as_bytes().iter().enumerate() {
                *translated_refmut(token, (addr + i) as *mut u8) = b;
            }
            *translated_refmut(token, (addr + env.len()) as *mut u8) = 0;
        }
        env_addrs.reverse();

        // Step 2: pad so the final initial stack pointer points at argc and is 16-byte aligned.
        let fixed_bytes = 16 + AUXV_COUNT * 16 + (argc + envc + 3) * 8;
        let target_mod = fixed_bytes % 16;
        sp -= (sp + 16 - target_mod) % 16;

        // Step 3: AT_RANDOM data (16 bytes below string area)
        sp -= 16;
        let random_addr = sp;
        let rand1 = (polyhal::time::Time::now().to_nsec() as u64)
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let rand2 = rand1
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *translated_refmut(token, random_addr as *mut u64) = rand1;
        *translated_refmut(token, (random_addr + 8) as *mut u64) = rand2;

        // Step 4: auxv entries (each 16 bytes = 2*u64)
        sp -= AUXV_COUNT * 16;
        let mut a = sp;
        macro_rules! auxv_write {
            ($t:expr, $v:expr) => {
                *translated_refmut(token, a as *mut u64) = $t;
                a += 8;
                *translated_refmut(token, a as *mut u64) = $v;
                a += 8;
            };
        }

        auxv_write!(AT_PHDR, ph_addr as u64);
        auxv_write!(AT_PHENT, ph_entsize as u64);
        auxv_write!(AT_PHNUM, ph_num as u64);
        auxv_write!(AT_PAGESZ, crate::config::PAGE_SIZE as u64);
        auxv_write!(AT_ENTRY, program_entry as u64);
        auxv_write!(AT_BASE, interp_base as u64);
        auxv_write!(AT_FLAGS, 0u64);
        auxv_write!(AT_UID, 0u64);
        auxv_write!(AT_EUID, 0u64);
        auxv_write!(AT_GID, 0u64);
        auxv_write!(AT_EGID, 0u64);
        auxv_write!(AT_CLKTCK, 100u64);
        auxv_write!(AT_SECURE, 0u64);
        #[cfg(target_arch = "loongarch64")]
        const PLATFORM_HWCAP: u64 = 1 << 2; // HWCAP_LOONGARCH_UAL
        #[cfg(not(target_arch = "loongarch64"))]
        const PLATFORM_HWCAP: u64 = 0;
        auxv_write!(AT_HWCAP, PLATFORM_HWCAP);
        auxv_write!(AT_PLATFORM, platform_addr as u64);
        auxv_write!(AT_RANDOM, random_addr as u64);
        // AT_EXECFN normally points at argv[0]. Some internal runners are
        // launched with argc=0, so keep auxv construction tolerant of that.
        auxv_write!(
            31,
            string_addrs.first().copied().unwrap_or(platform_addr) as u64
        );
        auxv_write!(AT_NULL, 0u64);

        // Step 5: envp NULL and pointers
        sp -= 8;
        *translated_refmut(token, sp as *mut u64) = 0;
        sp -= envc * 8;
        let envp_base = sp;
        for i in 0..envc {
            *translated_refmut(token, (envp_base + i * 8) as *mut u64) = env_addrs[i] as u64;
        }

        // Step 6: argv NULL sentinel
        sp -= 8;
        *translated_refmut(token, sp as *mut u64) = 0;

        // Step 7: argv pointers
        sp -= argc * 8;
        let argv_base = sp;
        for i in 0..argc {
            *translated_refmut(token, (argv_base + i * 8) as *mut u64) = string_addrs[i] as u64;
        }

        // Step 8: argc
        sp -= 8;
        *translated_refmut(token, sp as *mut u64) = argc as u64;
        let crt_sp = sp;

        super::invalidate_user_float_state();
        let mut inner = self.inner_exclusive_access();
        #[cfg(feature = "smp-diagnostics")]
        for (fd, file) in inner.fd_table.iter().enumerate() {
            if inner.fd_cloexec.get(fd).copied().unwrap_or(false)
                && file
                    .as_ref()
                    .map(|file| file.as_any().is::<crate::fs::UnixSocketFile>())
                    .unwrap_or(false)
            {
                crate::println!(
                    "[socketpair-diag] exec-close tid={} tgid={} fd={}",
                    self.getpid(),
                    inner.tgid,
                    fd,
                );
            }
        }
        let cloexec_files = inner.take_cloexec_fds();
        let mut exec_actions = inner.signal_actions.lock().clone();
        exec_actions.reset_for_exec();
        inner.signal_actions =
            Arc::new(SignalActionsLock::new_named(exec_actions, "SignalActions"));
        inner.handling_sig = -1;
        inner.signal_mask_backup = None;
        inner.sigsuspend_delivery = None;
        inner.trap_ctx_backup = None;
        inner.signal_ucontext = None;
        inner.heap_bottom = heap_bottom;
        inner.program_brk = heap_bottom;
        inner.fp_state = UserFloatState::new();
        inner.trap_cx = TrapFrame::new();

        let mut trap_cx = TrapFrame::new();
        trap_cx[TrapFrameArgs::SEPC] = entry_point;
        trap_cx[TrapFrameArgs::SP] = crt_sp;
        // Linux RISC-V programs obtain argc/argv/envp from the initial stack.
        // At the ELF entry point a0 carries rtld_fini, which is zero when the
        // kernel starts a program directly.  Supplying argc in a0 makes static
        // glibc register it as an exit callback and later jump to that value.
        #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
        {
            trap_cx[TrapFrameArgs::ARG0] = argc;
            trap_cx[TrapFrameArgs::ARG1] = argv_base;
            trap_cx[TrapFrameArgs::ARG2] = envp_base;
        }
        // ld.so owns TCB/DTV initialization for a dynamically linked program.
        if interp_base == 0 {
            if tls_size > 0 {
                trap_cx[TrapFrameArgs::TLS] = tls_base + tls_size;
            } else {
                let max_va: usize = inner.memory_set.max_end_vaddr().into();
                let tls_va = (max_va + crate::config::PAGE_SIZE - 1) / crate::config::PAGE_SIZE
                    * crate::config::PAGE_SIZE;
                let ok = inner.memory_set.insert_framed_area(
                    tls_va.into(),
                    (tls_va + crate::config::PAGE_SIZE).into(),
                    crate::mm::MapPermission::R
                        | crate::mm::MapPermission::W
                        | crate::mm::MapPermission::U,
                );
                if ok {
                    trap_cx[TrapFrameArgs::TLS] = tls_va + crate::config::PAGE_SIZE;
                }
            }
        }
        #[cfg(feature = "smp-diagnostics")]
        let committed_tp = trap_cx[TrapFrameArgs::TLS];
        *inner.get_trap_cx() = trap_cx;
        #[cfg(feature = "smp-diagnostics")]
        crate::println!(
            "[exec-diag] tid={} committed sepc={:#x} sp={:#x} argv={:#x} envp={:#x} tp={:#x}",
            self.getpid(),
            entry_point,
            crt_sp,
            argv_base,
            envp_base,
            committed_tp,
        );
        drop(inner);
        for file in cloexec_files {
            file.close_ref();
        }
    }
    pub fn fork(self: &Arc<TaskControlBlock>) -> Option<Arc<TaskControlBlock>> {
        trace!("os::task::TaskControlBlock::fork");
        // fork inherits the register image as it existed at the syscall entry.
        self.save_user_float_state();
        // Linux inherits the parent's CPU affinity across fork.  Snapshot the
        // atomic mask before publishing the child to any scheduler queue.
        let inherited_affinity = self.allowed_cpus();
        // Copy the user address space before collecting the remaining task state.
        // `from_existed_user` write-protects the parent's present writable
        // mappings for COW. A parent which is already a CLONE_VM group can be
        // active on several harts, so retain its activity mask while the PTEs
        // are changed and shoot those harts down after releasing Task/Vm.
        let (parent_activity, parent_memory_set) = {
            let parent_inner = self.inner_exclusive_access();
            (
                parent_inner.memory_set.activity_handle(),
                parent_inner.memory_set.fault_view(),
            )
        };
        let memory_set = MemorySet::from_existed_user(&parent_memory_set)?;
        assert!(
            super::processor::flush_remote_tlb(&parent_activity, 0, 0),
            "fork COW remote TLB shootdown failed"
        );

        let (
            trap_cx_clone,
            base_size,
            fd_table,
            fd_cloexec,
            rlimit_nofile,
            rlimit_nofile_max,
            rlimit_memlock,
            rlimit_memlock_max,
            cwd,
            root,
            executable_path,
            _tid_ptr,
            _parent_tgid,
            pgid,
            sid,
            uid,
            euid,
            suid,
            gid,
            egid,
            sgid,
            supplementary_groups,
            umask,
            nice,
            signal_mask,
            signal_actions,
            heap_bottom,
            program_brk,
            fp_state,
        ) = loop {
            let files_guard = super::FILE_TABLE_LOCK.lock();
            let Some(parent_inner) = self.try_inner_exclusive_access() else {
                drop(files_guard);
                super::suspend_current_and_run_next_reason(super::SuspendReason::ForkSnapshotRetry);
                continue;
            };
            let fork_signal_actions = Arc::new(SignalActionsLock::new_named(
                parent_inner.signal_actions.lock().clone(),
                "SignalActions",
            ));
            let snapshot = (
                parent_inner.trap_cx.clone(),
                parent_inner.base_size,
                parent_inner
                    .fd_table
                    .iter()
                    .map(|f| {
                        f.as_ref().map(|x| {
                            x.dup_ref();
                            x.clone()
                        })
                    })
                    .collect::<Vec<_>>(),
                parent_inner.fd_cloexec.clone(),
                parent_inner.rlimit_nofile,
                parent_inner.rlimit_nofile_max,
                parent_inner.rlimit_memlock,
                parent_inner.rlimit_memlock_max,
                parent_inner.cwd.clone(),
                parent_inner.root.clone(),
                parent_inner.executable_path.clone(),
                parent_inner.tid_ptr,
                parent_inner.tgid,
                parent_inner.pgid,
                parent_inner.sid,
                parent_inner.uid,
                parent_inner.euid,
                parent_inner.suid,
                parent_inner.gid,
                parent_inner.egid,
                parent_inner.sgid,
                parent_inner.supplementary_groups.clone(),
                parent_inner.umask,
                parent_inner.nice,
                parent_inner.signal_mask,
                fork_signal_actions,
                parent_inner.heap_bottom,
                parent_inner.memory_set.program_brk(),
                parent_inner.fp_state,
            );
            drop(parent_inner);
            drop(files_guard);
            break snapshot;
        };

        let pid_handle = pid_alloc();
        let pid_value = pid_handle.0;
        let kstack = KernelStack::new();
        let task_control_block = Arc::new(TaskControlBlock {
            pid: pid_handle,
            scheduling: TaskSchedulingMeta::new(pid_value),
            runtime: TaskRuntime::new(),
            inner: TaskLock::new_named(
                TaskControlBlockInner {
                    trap_cx: trap_cx_clone,
                    fp_state,
                    base_size,
                    task_cx: blank_kcontext(kstack.get_position().1), // Set task_cx's Kernel Stack Top
                    task_status: TaskStatus::Ready,
                    memory_set,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table,
                    fd_cloexec,
                    files_id: pid_value,
                    rlimit_nofile,
                    rlimit_nofile_max,
                    rlimit_memlock,
                    rlimit_memlock_max,
                    cwd,
                    root,
                    executable_path,
                    tid_ptr: 0,
                    tgid: pid_value,
                    pgid,
                    sid,
                    uid,
                    euid,
                    suid,
                    gid,
                    egid,
                    sgid,
                    supplementary_groups,
                    umask,
                    nice,
                    signals: SignalFlags::empty(),
                    pending_signal_info: [PendingSignalInfo::default(); super::MAX_SIG + 1],
                    signal_mask,
                    signal_mask_backup: None,
                    sigsuspend_delivery: None,
                    handling_sig: -1,
                    signal_actions,
                    killed: false,
                    terminating_for_exec: false,
                    terminating_for_group: false,
                    frozen: false,
                    trap_ctx_backup: None,
                    signal_ucontext: None,
                    kernel_stack: kstack,
                    heap_bottom,
                    program_brk,
                    sleeping: None,
                    cutime: 0,
                    cstime: 0,
                    block_reason: None,
                    itimer_expire: 0,
                    itimer_interval_us: 0,
                    child_tid: 0,
                    robust_list_head: 0,
                    robust_list_len: 0,
                    alt_stack: 0,
                    alt_stack_size: 0,
                    alt_stack_flags: 0,
                    in_syscall: false,
                    exit_signal: 0,
                },
                "TaskInner",
            ),
        });
        task_control_block.set_allowed_cpus(inherited_affinity);
        // add child to parent's children list
        {
            let mut parent_inner = self.inner_exclusive_access();
            parent_inner.children.push(task_control_block.clone());
        }
        Some(task_control_block)
    }
    pub fn clone_shared_vm(
        self: &Arc<TaskControlBlock>,
        share_signal_actions: bool,
        share_files: bool,
        materialize_writable_pages: bool,
    ) -> Option<Arc<TaskControlBlock>> {
        trace!("os::task::TaskControlBlock::clone_shared_vm");
        self.save_user_float_state();
        let inherited_affinity = self.allowed_cpus();
        let mut parent_activity = None;
        let mut remapped_shared_pages = false;
        let (
            memory_set,
            trap_cx_clone,
            base_size,
            fd_table,
            fd_cloexec,
            parent_files_id,
            rlimit_nofile,
            rlimit_nofile_max,
            rlimit_memlock,
            rlimit_memlock_max,
            cwd,
            root,
            executable_path,
            _tid_ptr,
            pgid,
            sid,
            uid,
            euid,
            suid,
            gid,
            egid,
            sgid,
            supplementary_groups,
            umask,
            nice,
            signal_mask,
            signal_actions,
            heap_bottom,
            program_brk,
            fp_state,
        ) = loop {
            let files_guard = super::FILE_TABLE_LOCK.lock();
            let Some(mut parent_inner) = self.try_inner_exclusive_access() else {
                drop(files_guard);
                super::suspend_current_and_run_next_reason(
                    super::SuspendReason::CloneSnapshotRetry,
                );
                continue;
            };
            // COW mappings require normalization only when this is the first
            // CLONE_VM conversion of a private address space. Rewriting an
            // already shared page table would race with its running siblings.
            if materialize_writable_pages && !parent_inner.memory_set.is_shared() {
                parent_activity = Some(parent_inner.memory_set.activity_handle());
                if !parent_inner.memory_set.prepare_shared_writable_pages() {
                    return None;
                }
                remapped_shared_pages = true;
            }
            parent_inner.memory_set.mark_shared();
            let memory_set = MemorySet::from_shared(&parent_inner.memory_set);
            let child_signal_actions = if share_signal_actions {
                parent_inner.signal_actions.clone()
            } else {
                Arc::new(SignalActionsLock::new_named(
                    parent_inner.signal_actions.lock().clone(),
                    "SignalActions",
                ))
            };
            let (fd_table, fd_cloexec) = if share_files {
                (Vec::new(), Vec::new())
            } else {
                (
                    parent_inner
                        .fd_table
                        .iter()
                        .map(|f| {
                            f.as_ref().map(|x| {
                                x.dup_ref();
                                x.clone()
                            })
                        })
                        .collect::<Vec<_>>(),
                    parent_inner.fd_cloexec.clone(),
                )
            };
            let snapshot = (
                memory_set,
                parent_inner.trap_cx.clone(),
                parent_inner.base_size,
                fd_table,
                fd_cloexec,
                parent_inner.files_id,
                parent_inner.rlimit_nofile,
                parent_inner.rlimit_nofile_max,
                parent_inner.rlimit_memlock,
                parent_inner.rlimit_memlock_max,
                parent_inner.cwd.clone(),
                parent_inner.root.clone(),
                parent_inner.executable_path.clone(),
                parent_inner.tid_ptr,
                parent_inner.pgid,
                parent_inner.sid,
                parent_inner.uid,
                parent_inner.euid,
                parent_inner.suid,
                parent_inner.gid,
                parent_inner.egid,
                parent_inner.sgid,
                parent_inner.supplementary_groups.clone(),
                parent_inner.umask,
                parent_inner.nice,
                parent_inner.signal_mask,
                child_signal_actions,
                parent_inner.heap_bottom,
                parent_inner.memory_set.program_brk(),
                parent_inner.fp_state,
            );
            drop(parent_inner);
            drop(files_guard);
            break snapshot;
        };

        if remapped_shared_pages {
            assert!(
                super::processor::flush_remote_tlb(
                    parent_activity.as_ref().expect("missing shared VM activity"),
                    0,
                    0,
                ),
                "CLONE_VM remote TLB shootdown failed"
            );
        }

        let pid_handle = pid_alloc();
        let pid_value = pid_handle.0;
        let kstack = KernelStack::new();
        let task_control_block = Arc::new(TaskControlBlock {
            pid: pid_handle,
            scheduling: TaskSchedulingMeta::new(pid_value),
            runtime: TaskRuntime::new(),
            inner: TaskLock::new_named(
                TaskControlBlockInner {
                    trap_cx: trap_cx_clone,
                    fp_state,
                    base_size,
                    task_cx: blank_kcontext(kstack.get_position().1),
                    task_status: TaskStatus::Ready,
                    memory_set,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table,
                    fd_cloexec,
                    files_id: if share_files { parent_files_id } else { pid_value },
                    rlimit_nofile,
                    rlimit_nofile_max,
                    rlimit_memlock,
                    rlimit_memlock_max,
                    cwd,
                    root,
                    executable_path,
                    tid_ptr: 0,
                    tgid: pid_value,
                    pgid,
                    sid,
                    uid,
                    euid,
                    suid,
                    gid,
                    egid,
                    sgid,
                    supplementary_groups,
                    umask,
                    nice,
                    signals: SignalFlags::empty(),
                    pending_signal_info: [PendingSignalInfo::default(); super::MAX_SIG + 1],
                    signal_mask,
                    signal_mask_backup: None,
                    sigsuspend_delivery: None,
                    handling_sig: -1,
                    signal_actions,
                    killed: false,
                    terminating_for_exec: false,
                    terminating_for_group: false,
                    frozen: false,
                    trap_ctx_backup: None,
                    signal_ucontext: None,
                    kernel_stack: kstack,
                    heap_bottom,
                    program_brk,
                    sleeping: None,
                    cutime: 0,
                    cstime: 0,
                    block_reason: None,
                    itimer_expire: 0,
                    itimer_interval_us: 0,
                    child_tid: 0,
                    robust_list_head: 0,
                    robust_list_len: 0,
                    alt_stack: 0,
                    alt_stack_size: 0,
                    alt_stack_flags: 0,
                    in_syscall: false,
                    exit_signal: 0,
                },
                "TaskInner",
            ),
        });
        task_control_block.set_allowed_cpus(inherited_affinity);
        if share_files {
            loop {
                let mut files = super::FILE_TABLE_LOCK.lock();
                if !task_control_block.try_initialize_shared_files_from(self) {
                    drop(files);
                    super::suspend_current_and_run_next_reason(
                        super::SuspendReason::CloneFilesRetry,
                    );
                    continue;
                }
                super::register_files_task_locked(
                    &mut files,
                    &task_control_block,
                    parent_files_id,
                );
                break;
            }
        }
        {
            let mut parent_inner = self.inner_exclusive_access();
            parent_inner.children.push(task_control_block.clone());
        }
        Some(task_control_block)
    }
    pub(crate) fn try_initialize_shared_files_from(
        &self,
        parent: &Arc<TaskControlBlock>,
    ) -> bool {
        let Some(parent_inner) = parent.try_inner_exclusive_access() else {
            return false;
        };
        let files_id = parent_inner.files_id;
        let fd_table = parent_inner
            .fd_table
            .iter()
            .map(|file| {
                file.as_ref().map(|file| {
                    file.dup_ref();
                    file.clone()
                })
            })
            .collect::<Vec<_>>();
        let fd_cloexec = parent_inner.fd_cloexec.clone();
        drop(parent_inner);
        let mut inner = self.inner_exclusive_access();
        debug_assert!(inner.fd_table.is_empty());
        debug_assert!(inner.fd_cloexec.is_empty());
        debug_assert_eq!(inner.files_id, files_id);
        inner.fd_table = fd_table;
        inner.fd_cloexec = fd_cloexec;
        true
    }
    pub fn getpid(&self) -> usize {
        self.pid.0
    }

    pub fn set_executable_path(&self, executable_path: String) {
        self.inner_exclusive_access().executable_path = executable_path;
    }

    /// The hart currently running this task, or [`NO_RUNNING_CPU`].
    pub fn running_cpu(&self) -> usize {
        self.scheduling.running_cpu.load(Ordering::Acquire)
    }

    pub(crate) fn last_cpu(&self) -> usize {
        self.scheduling.last_cpu.load(Ordering::Relaxed)
    }

    pub fn thread_group_id(&self) -> usize {
        self.scheduling.thread_group_id.load(Ordering::Acquire)
    }

    pub(crate) fn publish_thread_group_id(&self, tgid: usize) {
        self.scheduling
            .thread_group_id
            .store(tgid, Ordering::Release);
    }

    pub fn is_enqueued(&self) -> bool {
        self.scheduling.enqueued.load(Ordering::Acquire)
    }

    pub fn exit_cleanup_done(&self) -> bool {
        self.scheduling.exit_cleanup_done.load(Ordering::Acquire)
    }

    pub fn is_waitable_zombie(&self) -> bool {
        self.scheduling.waitable_zombie.load(Ordering::Acquire)
    }

    pub(crate) fn set_vfork_parent(&self, parent_tid: usize) {
        assert_ne!(parent_tid, 0, "vfork parent TID must be non-zero");
        assert!(
            self.scheduling
                .vfork_parent
                .compare_exchange(0, parent_tid, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "pid {} acquired two vfork parents",
            self.getpid()
        );
    }

    pub(crate) fn vfork_pending(&self) -> bool {
        self.scheduling.vfork_parent.load(Ordering::Acquire) != 0
    }

    pub(crate) fn take_vfork_parent(&self) -> Option<usize> {
        match self.scheduling.vfork_parent.swap(0, Ordering::AcqRel) {
            0 => None,
            tid => Some(tid),
        }
    }

    pub(crate) fn publish_exit_cleanup_done(&self) {
        self.scheduling
            .exit_cleanup_done
            .store(true, Ordering::Release);
    }

    pub(crate) fn try_publish_waitable_zombie(&self) -> bool {
        self.scheduling
            .waitable_zombie
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Return the task's requested CPU mask.  The mask is not user-modifiable
    /// until the four-hart scheduler is active.
    pub fn allowed_cpus(&self) -> usize {
        self.scheduling.allowed_cpus.load(Ordering::Acquire)
    }

    /// Store a previously validated affinity mask.  Callers that also need to
    /// move a Ready task must do so through the scheduler, after this store.
    pub(crate) fn set_allowed_cpus(&self, mask: usize) {
        self.scheduling.allowed_cpus.store(mask, Ordering::Release);
    }

    /// Scheduler-only: record that the task has one ready-queue entry.
    pub(crate) fn mark_enqueued(&self) {
        assert!(
            self.scheduling
                .enqueued
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "pid {} was enqueued twice",
            self.getpid()
        );
    }

    /// Scheduler-only: record removal from the ready queue.
    pub(crate) fn mark_dequeued(&self) {
        assert!(
            self.scheduling
                .enqueued
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "pid {} was dequeued without a ready-queue entry",
            self.getpid()
        );
    }

    /// Scheduler-only: record the CPU which owns the task's execution.
    pub(crate) fn mark_running_on(&self, cpu: usize) {
        assert!(
            self.scheduling
                .running_cpu
                .compare_exchange(NO_RUNNING_CPU, cpu, Ordering::AcqRel, Ordering::Acquire,)
                .is_ok(),
            "pid {} was dispatched while already owned by hart {}",
            self.getpid(),
            self.running_cpu(),
        );
        self.scheduling.last_cpu.store(cpu, Ordering::Relaxed);
    }

    /// Scheduler-only: clear execution ownership after this CPU has removed
    /// the task from its CpuLocal current slot.
    pub(crate) fn clear_running_on(&self, cpu: usize) {
        assert!(
            self.scheduling
                .running_cpu
                .compare_exchange(cpu, NO_RUNNING_CPU, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "pid {} lost running ownership on hart {} (owner {})",
            self.getpid(),
            cpu,
            self.running_cpu(),
        );
    }
    pub fn record_time_user_to_kernel(&self) {
        self.runtime.charge(&self.runtime.utime);
    }
    pub fn record_time_kernel_to_user(&self) {
        self.runtime.charge(&self.runtime.stime);
    }
    pub fn record_time_suspend(&self) {
        self.runtime.charge(&self.runtime.stime);
    }
    pub fn record_time_resume(&self) {
        self.runtime.last_time.store(
            polyhal::time::Time::now().to_nsec() as usize,
            Ordering::Relaxed,
        );
    }
    pub fn user_time_ns(&self) -> usize {
        self.runtime.utime.load(Ordering::Relaxed)
    }
    pub fn system_time_ns(&self) -> usize {
        self.runtime.stime.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum BlockReason {
    TcpRecv(usize),
    TcpSend(usize),
    TcpAccept(usize),
    TcpConnect(usize),
    LocalTcpRecv(usize, bool),
    LocalTcpSend(usize, bool),
    LocalTcpAccept(u16),
    UdpRecv(usize),
    PollWait,
    PipeRead(usize),
    PipeWrite(usize),
    Futex(usize, usize),
    FilePageLoad(usize),
    SleepLock(usize),
    VmReadLock(usize),
    VmWriteLock(usize),
    ExecPeer(usize),
    SignalWait(SignalFlags),
    Vfork(usize),
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum TaskStatus {
    Ready,
    Running,
    /// The task has requested a yield, but its saved kernel context is not
    /// publishable yet.  Only the owning CPU may turn this into `Ready`, and
    /// it does so after `context_switch_pt` has returned to that CPU's idle
    /// context.  Keeping this distinct from `Ready` closes the SMP window in
    /// which another hart could otherwise execute an incompletely saved task.
    Switching,
    /// The task is registered in a scheduler wait queue but has not switched
    /// out of its CPU context yet.  A concurrent wake changes this to
    /// `WakePending`, never directly to `Ready`.
    Blocking,
    Blocked,
    /// A wake raced with the outgoing context switch.  The owning CPU turns
    /// this into `Ready` only after it has reached the idle context.
    WakePending,
    Zombie,
}
