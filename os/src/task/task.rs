use super::{current_task, SignalActions, UserFloatState};
use super::{pid_alloc, PidHandle, SignalFlags};
use crate::config::KERNEL_STACK_SIZE;
use crate::fs::vfs::{Dentry, ROOT_DENTRY};
use crate::fs::{File, Stdin, Stdout};
use crate::mm::{translated_refmut, LoadedElf, MapArea, MapPermission, MapType, MemorySet};
use crate::sync::UPSafeCell;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Ref, RefMut};
use core::mem::size_of;
use polyhal::kcontext::{read_current_tp, KContext, KContextArgs};
use polyhal::pagetable::PageTable;
use polyhal_trap::trap::run_user_task;
use polyhal_trap::trapframe::{TrapFrame, TrapFrameArgs};

pub struct TaskControlBlock {
    // immutable
    pub pid: PidHandle,
    // mutable
    inner: UPSafeCell<TaskControlBlockInner>,
}
use log::*;
impl Drop for TaskControlBlock {
    fn drop(&mut self) {
        trace!("drop task control block");
    }
}

pub struct KernelStack {
    inner: Arc<[u128; KERNEL_STACK_SIZE / size_of::<u128>()]>,
}

impl KernelStack {
    pub fn new() -> Self {
        Self {
            inner: Arc::new([0u128; KERNEL_STACK_SIZE / size_of::<u128>()]),
        }
    }

    pub fn get_position(&self) -> (usize, usize) {
        let bottom = self.inner.as_ptr() as usize;
        (bottom, bottom + KERNEL_STACK_SIZE)
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
    pub rlimit_nofile: usize,
    pub rlimit_nofile_max: usize,
    pub cwd: Arc<Dentry>,
    pub root: Arc<Dentry>,
    pub tid_ptr: usize,
    pub tgid: usize,
    pub pgid: usize,
    pub sid: usize,
    pub uid: u32,
    pub euid: u32,
    pub gid: u32,
    pub egid: u32,
    pub umask: u32,
    pub signals: SignalFlags,
    pub signal_mask: SignalFlags,
    pub signal_mask_backup: Option<SignalFlags>,
    // the signal which is being handling
    pub handling_sig: isize,
    // Signal actions
    pub signal_actions: SignalActions,
    // if the task is killed
    pub killed: bool,
    // if the task is frozen by a signal
    pub frozen: bool,
    pub trap_ctx_backup: Option<TrapFrame>,
    // record the stack
    pub heap_bottom: usize,
    pub program_brk: usize,
    pub sleeping: Option<u64>,
    pub utime: usize,
    pub stime: usize,
    pub cutime: usize,
    pub cstime: usize,
    pub last_time: usize,
    pub block_reason: Option<BlockReason>,
    pub itimer_expire: u64,
    pub itimer_interval_us: u64,
    pub child_tid: usize,
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
    pub fn close_fd(&mut self, fd: usize) -> bool {
        if fd < self.fd_table.len() {
            if let Some(file) = self.fd_table[fd].take() {
                file.close_ref();
                return true;
            }
        }
        false
    }
    pub fn clear_fd_table(&mut self) {
        for fd in 0..self.fd_table.len() {
            self.close_fd(fd);
        }
        self.fd_table.clear();
        self.fd_cloexec.clear();
    }

    pub fn close_cloexec_fds(&mut self) {
        let len = self.fd_table.len().min(self.fd_cloexec.len());
        for fd in 0..len {
            if self.fd_cloexec[fd] {
                self.close_fd(fd);
            }
        }
    }
}

fn task_entry() {
    trace!("os::task::task_entry");
    loop {
        let task = current_task().unwrap();
        task.restore_user_float_state();
        let trap_cx = task.inner.exclusive_access().get_trap_cx() as *mut TrapFrame;
        let ctx_mut = unsafe { trap_cx.as_mut().unwrap() };
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
    pub fn inner_exclusive_access(&self) -> RefMut<'_, TaskControlBlockInner> {
        self.inner.exclusive_access()
    }
    pub fn from_raw(pid: PidHandle, inner: TaskControlBlockInner) -> Self {
        Self {
            pid,
            inner: unsafe { UPSafeCell::new(inner) },
        }
    }
    pub fn try_inner_exclusive_access(&self) -> Option<RefMut<'_, TaskControlBlockInner>> {
        match self.inner.try_borrow_mut() {
            Some(g) => Some(g),
            None => None,
        }
    }
    pub fn try_inner_borrow(&self) -> Option<core::cell::Ref<'_, TaskControlBlockInner>> {
        match self.inner.try_borrow() {
            Some(g) => Some(g),
            None => None,
        }
    }
    pub fn save_user_float_state(&self) {
        if let Some(mut inner) = self.try_inner_exclusive_access() {
            inner.fp_state.save_current();
        }
    }
    pub fn restore_user_float_state(&self) {
        let fp_state = {
            let inner = self.inner_exclusive_access();
            inner.fp_state
        };
        fp_state.restore_current();
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
        // alloc a pid and a kernel stack in kernel space
        let pid_handle = pid_alloc();
        let pid_value = pid_handle.0;
        let kstack = KernelStack::new();
        let task_control_block = Self {
            pid: pid_handle,
            inner: unsafe {
                UPSafeCell::new_named(
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
                            Some(Arc::new(Stdin)),
                            // 1 -> stdout
                            Some(Arc::new(Stdout)),
                            // 2 -> stderr
                            Some(Arc::new(Stdout)),
                        ],
                        fd_cloexec: vec![false, false, false],
                        rlimit_nofile: 1024,
                        rlimit_nofile_max: 1024,
                        cwd: ROOT_DENTRY.clone(),
                        root: ROOT_DENTRY.clone(),
                        tid_ptr: 0,
                        tgid: pid_value,
                        pgid: pid_value,
                        sid: pid_value,
                        uid: 0,
                        euid: 0,
                        gid: 0,
                        egid: 0,
                        umask: 0o022,
                        signals: SignalFlags::empty(),
                        signal_mask: SignalFlags::empty(),
                        signal_mask_backup: None,
                        handling_sig: -1,
                        signal_actions: SignalActions::default(),
                        killed: false,
                        frozen: false,
                        trap_ctx_backup: None,
                        kernel_stack: kstack,
                        // added
                        heap_bottom,
                        program_brk: heap_bottom,
                        sleeping: None,
                        utime: 0,
                        stime: 0,
                        cutime: 0,
                        cstime: 0,
                        last_time: polyhal::time::Time::now().to_nsec() as usize,
                        block_reason: None,
                        itimer_expire: 0,
                        itimer_interval_us: 0,
                        child_tid: 0,
                        alt_stack: 0,
                        alt_stack_size: 0,
                        alt_stack_flags: 0,
                        in_syscall: false,
                        exit_signal: 0,
                    },
                    "TaskInner",
                )
            },
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
        elf_file: &dyn File,
        interp_file: Option<&dyn File>,
        args: Vec<String>,
        envs: Vec<String>,
    ) -> bool {
        trace!("os::task::TaskControlBlock::exec_file_with_interp");
        const INTERP_BASE: usize = 0x10_0000_0000;

        let mut loaded = match MemorySet::from_elf_file(elf_file) {
            Some(loaded) => loaded,
            None => return false,
        };
        if let Some(file) = interp_file {
            let Some((interp_entry, interp_base)) =
                MemorySet::load_interp_file(&mut loaded.memory_set, file, INTERP_BASE)
            else {
                return false;
            };
            loaded.entry_point = interp_entry;
            loaded.interp_base = interp_base;
        }
        self.exec_loaded(loaded, args, envs);
        true
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

        memory_set.activate();
        let token = memory_set.token();
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
        auxv_write!(AT_HWCAP, 0u64);
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

        let mut inner = self.inner_exclusive_access();
        inner.close_cloexec_fds();
        inner.memory_set = memory_set;
        inner.heap_bottom = heap_bottom;
        inner.program_brk = heap_bottom;
        inner.fp_state = UserFloatState::new();
        inner.trap_cx = TrapFrame::new();

        let mut trap_cx = TrapFrame::new();
        trap_cx[TrapFrameArgs::SEPC] = entry_point;
        trap_cx[TrapFrameArgs::SP] = crt_sp;
        trap_cx[TrapFrameArgs::ARG0] = argc;
        trap_cx[TrapFrameArgs::ARG1] = argv_base;
        trap_cx[TrapFrameArgs::ARG2] = envp_base;
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
        *inner.get_trap_cx() = trap_cx;
    }
    pub fn fork(self: &Arc<TaskControlBlock>) -> Option<Arc<TaskControlBlock>> {
        trace!("os::task::TaskControlBlock::fork");
        // Copy the user address space before collecting the remaining task state.
        let memory_set = {
            let parent_inner = self.inner_exclusive_access();
            MemorySet::from_existed_user(&parent_inner.memory_set)?
        };

        let (
            trap_cx_clone,
            base_size,
            fd_table,
            fd_cloexec,
            rlimit_nofile,
            rlimit_nofile_max,
            cwd,
            root,
            tid_ptr,
            _parent_tgid,
            pgid,
            sid,
            uid,
            euid,
            gid,
            egid,
            umask,
            signal_mask,
            signal_actions,
            heap_bottom,
            program_brk,
            fp_state,
        ) = {
            let parent_inner = self.inner_exclusive_access();
            (
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
                parent_inner.cwd.clone(),
                parent_inner.root.clone(),
                parent_inner.tid_ptr,
                parent_inner.tgid,
                parent_inner.pgid,
                parent_inner.sid,
                parent_inner.uid,
                parent_inner.euid,
                parent_inner.gid,
                parent_inner.egid,
                parent_inner.umask,
                parent_inner.signal_mask,
                parent_inner.signal_actions.clone(),
                parent_inner.heap_bottom,
                parent_inner.program_brk,
                parent_inner.fp_state,
            )
        };

        let pid_handle = pid_alloc();
        let pid_value = pid_handle.0;
        let kstack = KernelStack::new();
        let task_control_block = Arc::new(TaskControlBlock {
            pid: pid_handle,
            inner: unsafe {
                UPSafeCell::new_named(
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
                        rlimit_nofile,
                        rlimit_nofile_max,
                        cwd,
                        root,
                        tid_ptr,
                        tgid: pid_value,
                        pgid,
                        sid,
                        uid,
                        euid,
                        gid,
                        egid,
                        umask,
                        signals: SignalFlags::empty(),
                        signal_mask,
                        signal_mask_backup: None,
                        handling_sig: -1,
                        signal_actions,
                        killed: false,
                        frozen: false,
                        trap_ctx_backup: None,
                        kernel_stack: kstack,
                        heap_bottom,
                        program_brk,
                        sleeping: None,
                        utime: 0,
                        stime: 0,
                        cutime: 0,
                        cstime: 0,
                        last_time: polyhal::time::Time::now().to_nsec() as usize,
                        block_reason: None,
                        itimer_expire: 0,
                        itimer_interval_us: 0,
                        child_tid: 0,
                        alt_stack: 0,
                        alt_stack_size: 0,
                        alt_stack_flags: 0,
                        in_syscall: false,
                        exit_signal: 0,
                    },
                    "TaskInner",
                )
            },
        });
        // add child to parent's children list
        {
            let mut parent_inner = self.inner_exclusive_access();
            parent_inner.children.push(task_control_block.clone());
        }
        Some(task_control_block)
    }
    pub fn getpid(&self) -> usize {
        self.pid.0
    }
    pub fn record_time_user_to_kernel(&self) {
        let Some(mut inner) = self.try_inner_exclusive_access() else {
            return;
        };
        let now = polyhal::time::Time::now().to_nsec() as usize;
        inner.utime += now - inner.last_time;
        inner.last_time = now;
    }
    pub fn record_time_kernel_to_user(&self) {
        let Some(mut inner) = self.try_inner_exclusive_access() else {
            return;
        };
        let now = polyhal::time::Time::now().to_nsec() as usize;
        inner.stime += now - inner.last_time;
        inner.last_time = now;
    }
    pub fn record_time_suspend(&self) {
        let Some(mut inner) = self.try_inner_exclusive_access() else {
            return;
        };
        let now = polyhal::time::Time::now().to_nsec() as usize;
        inner.stime += now - inner.last_time;
        inner.last_time = now;
    }
    pub fn record_time_resume(&self) {
        let Some(mut inner) = self.try_inner_exclusive_access() else {
            return;
        };
        inner.last_time = polyhal::time::Time::now().to_nsec() as usize;
    }
}

#[derive(Debug, Clone, Copy)]
pub enum BlockReason {
    TcpRecv(usize),
    TcpSend(usize),
    TcpAccept(usize),
    TcpConnect(usize),
    UdpRecv(usize),
    PollWait,
    PipeRead(usize),
    PipeWrite(usize),
}

#[derive(Copy, Clone, PartialEq)]
pub enum TaskStatus {
    Ready,
    Running,
    Blocked,
    Zombie,
}
