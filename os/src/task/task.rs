// 这里负责操作系统的进程管理：定义任务控制块、内核栈、exec/fork 以及任务时间统计。
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
    // TCB 引用计数归零时只打印调试信息，真正资源主要由 inner 清理。
    // 执行时不主动回收子资源，pid、内核栈和地址空间都会随字段 drop 自动释放。
    fn drop(&mut self) {
        trace!("drop task control block");
    }
}

pub struct KernelStack {
    inner: Arc<[u128; KERNEL_STACK_SIZE / size_of::<u128>()]>,
}

impl KernelStack {
    // 分配一块内核栈空间。
    // 执行时用 Arc 包住固定大小数组，保证任务上下文切换时栈内存地址稳定。
    pub fn new() -> Self {
        Self {
            inner: Arc::new([0u128; KERNEL_STACK_SIZE / size_of::<u128>()]),
        }
    }

    // 返回内核栈的底和顶地址。
    // 执行时用数组首地址作为栈底，再加 KERNEL_STACK_SIZE 得到向下增长栈的初始栈顶。
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
    pub cwd: Arc<Dentry>,
    pub root: Arc<Dentry>,
    pub tid_ptr: usize,
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
    // 当前正在处理的信号编号，-1 表示没有。
    pub handling_sig: isize,
    // 每个信号对应的处理动作。
    pub signal_actions: SignalActions,
    // 任务是否已经被标记为需要退出。
    pub killed: bool,
    // 任务是否被 SIGSTOP 一类信号冻结。
    pub frozen: bool,
    pub trap_ctx_backup: Option<TrapFrame>,
    // 记录堆区边界，brk 会修改 program_brk。
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
    // 取当前任务的 trap 上下文。
    // 执行时把 trap_cx 字段地址转成可变引用；这里用 unsafe 是为了交给 trap 汇编直接读写寄存器现场。
    pub fn get_trap_cx(&self) -> &'static mut TrapFrame {
        let paddr = &self.trap_cx as *const TrapFrame as usize as *mut TrapFrame;
        // let paddr: PhysAddr = self.trap_cx.into();
        // unsafe { paddr.get_mut_ptr::<TrapFrame>().as_mut().unwrap() }
        unsafe { paddr.as_mut().unwrap() }
    }
    // 返回当前任务地址空间的页表 token。
    // 执行时转发到 MemorySet::token，系统调用翻译用户指针和调度切换都会用它。
    pub fn get_user_token(&self) -> PageTable {
        self.memory_set.token()
    }
    // 返回当前任务状态，外层判断 Zombie/Ready 时统一通过这里读取。
    // 执行时只返回 task_status 的拷贝，不改变调度队列。
    fn get_status(&self) -> TaskStatus {
        self.task_status
    }
    // 判断任务是否已经进入 Zombie 状态。
    // 执行时通过 get_status 读取当前状态，并和 Zombie 枚举比较。
    pub fn is_zombie(&self) -> bool {
        self.get_status() == TaskStatus::Zombie
    }
    // 分配一个空闲 fd 槽位，最多支持 1024 个。
    // 执行时先检查可用资源，再分配编号、页帧或缓冲区，最后记录状态并返回给调用者。
    pub fn alloc_fd(&mut self) -> usize {
        const MAX_FD: usize = 1024;
        let search_end = self.fd_table.len().min(MAX_FD);
        if let Some(fd) = (0..search_end).find(|fd| self.fd_table[*fd].is_none()) {
            self.fd_cloexec[fd] = false;
            fd
        } else if self.fd_table.len() < MAX_FD {
            self.fd_table.push(None);
            self.fd_cloexec.push(false);
            self.fd_table.len() - 1
        } else {
            MAX_FD
        }
    }
    // 关闭指定 fd，并通知文件对象减少引用。
    // 执行时先找到要关闭的对象，再释放引用或清理队列状态，最后返回关闭结果。
    pub fn close_fd(&mut self, fd: usize) -> bool {
        if fd < self.fd_table.len() {
            if let Some(file) = self.fd_table[fd].take() {
                file.close_ref();
                return true;
            }
        }
        false
    }
    // 关闭并清空整个 fd 表。
    // 执行时逐个 close_fd 让文件对象处理引用计数，然后清空 fd 和 cloexec 两张表。
    pub fn clear_fd_table(&mut self) {
        for fd in 0..self.fd_table.len() {
            self.close_fd(fd);
        }
        self.fd_table.clear();
        self.fd_cloexec.clear();
    }

    // exec 前关闭带 FD_CLOEXEC 标记的 fd。
    // 执行时先找到要关闭的对象，再释放引用或清理队列状态，最后返回关闭结果。
    pub fn close_cloexec_fds(&mut self) {
        let len = self.fd_table.len().min(self.fd_cloexec.len());
        for fd in 0..len {
            if self.fd_cloexec[fd] {
                self.close_fd(fd);
            }
        }
    }
}

// 任务第一次被调度到内核上下文后，从这里反复进入用户态。
// 执行时恢复用户浮点状态，取出 trap frame，然后交给 trap 层真正返回用户态运行。
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

// 构造一个会跳到 task_entry 的空内核上下文。
// 执行时把内核入口、内核栈顶和当前 tp 写进 KContext，调度器第一次切入任务时会从这里开始。
pub fn blank_kcontext(ksp: usize) -> KContext {
    let mut kcx = KContext::blank();
    kcx[KContextArgs::KPC] = task_entry as usize;
    kcx[KContextArgs::KSP] = ksp;
    kcx[KContextArgs::KTP] = read_current_tp();
    kcx
}

impl TaskControlBlock {
    // 独占访问 TCB 内部可变状态。
    // 执行时通过 UPSafeCell 取得 inner 的可变借用，调用方要避免嵌套重复借用。
    pub fn inner_exclusive_access(&self) -> RefMut<'_, TaskControlBlockInner> {
        self.inner.exclusive_access()
    }
    // 用已有 pid 和 inner 直接组装 TCB。
    // 执行时把传入的 inner 包进 UPSafeCell，适合 fork/exec 等已经准备好任务状态的路径。
    pub fn from_raw(pid: PidHandle, inner: TaskControlBlockInner) -> Self {
        Self {
            pid,
            inner: unsafe { UPSafeCell::new(inner) },
        }
    }
    // 尝试独占访问 inner，拿不到锁就返回 None。
    // 执行时只尝试一次可变借用，调度和 wait 扫描路径用它避免因为别处持锁而 panic。
    pub fn try_inner_exclusive_access(&self) -> Option<RefMut<'_, TaskControlBlockInner>> {
        match self.inner.try_borrow_mut() {
            Some(g) => Some(g),
            None => None,
        }
    }
    // 尝试只读借用 inner，拿不到就返回 None。
    // 执行时只尝试一次不可变借用，适合检查任务状态而不强行打断当前持锁路径。
    pub fn try_inner_borrow(&self) -> Option<core::cell::Ref<'_, TaskControlBlockInner>> {
        match self.inner.try_borrow() {
            Some(g) => Some(g),
            None => None,
        }
    }
    // trap 进内核时保存用户浮点状态。
    // 执行时如果能拿到 inner，就把当前硬件浮点寄存器保存进任务的 fp_state。
    pub fn save_user_float_state(&self) {
        if let Some(mut inner) = self.try_inner_exclusive_access() {
            inner.fp_state.save_current();
        }
    }
    // 回到用户态前恢复用户浮点状态。
    // 执行时先复制出 fp_state，放掉 inner 借用后再恢复寄存器，避免恢复过程持有任务锁。
    pub fn restore_user_float_state(&self) {
        let fp_state = {
            let inner = self.inner_exclusive_access();
            inner.fp_state
        };
        fp_state.restore_current();
    }
    // 从 initproc ELF 创建第一个用户任务。
    // 执行时装载 ELF 地址空间，分配 pid/内核栈，初始化标准 fd、cwd/root、凭据、信号和计时字段。
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
        // 给任务分配 pid 和内核栈。
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
                        task_cx: blank_kcontext(kstack.get_position().1), // task_cx 使用内核栈顶。
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
                        cwd: ROOT_DENTRY.clone(),
                        root: ROOT_DENTRY.clone(),
                        tid_ptr: 0,
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
                        // 初始化用户堆和时间统计。
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
        // 初始 trap 上下文决定新任务第一次进入用户态时的入口、栈和参数寄存器。
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
    // 不带解释器的 exec 简化入口。
    // 执行时先装载新 ELF 和可选解释器，再重建用户栈与 trap 上下文，最后替换当前任务地址空间。
    pub fn exec(&self, elf_data: &[u8], args: Vec<String>, envs: Vec<String>) {
        self.exec_with_interp(elf_data, None, args, envs);
    }

    // 替换当前任务地址空间，并按 Linux ABI 重建用户栈。
    // 执行时先装载新 ELF 和可选解释器，再重建用户栈与 trap 上下文，最后替换当前任务地址空间。
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
        let mut entry_point = loaded.entry_point;
        let program_entry = loaded.program_entry;
        let program_break = loaded.program_break;

        // 如果 ELF 需要动态链接器，就把解释器装到固定高地址并改用解释器入口。
        if let Some(id) = interp_data {
            let (interp_entry, interp_base) =
                MemorySet::load_interp_elf(&mut loaded.memory_set, id, INTERP_BASE);
            entry_point = interp_entry;
            loaded.interp_base = interp_base;
        }
        let stack_top = loaded.user_sp;
        let heap_bottom = program_break;
        let ph_addr = loaded.ph_addr;
        let ph_entsize = loaded.ph_entsize;
        let ph_num = loaded.ph_num;
        let tls_base = loaded.tls_base;
        let tls_size = loaded.tls_size;
        let interp_base = loaded.interp_base;
        let memory_set = loaded.memory_set;

        // 新地址空间建好后先切换页表，后面才能直接往用户栈地址写 argv/envp/auxv。
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

        // 用户栈从高到低依次放 platform、字符串、随机数、auxv、envp、argv 和 argc。

        let mut sp = stack_top;

        // AT_PLATFORM 指向一段架构字符串，动态链接器会从 auxv 里读取它。
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

        // argv/envp 字符串本体先写到栈顶区域，再保存每个字符串的用户地址。
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

        // 调整 sp，让最终入口看到的栈顶满足 16 字节对齐要求。
        let fixed_bytes = 16 + AUXV_COUNT * 16 + (argc + envc + 3) * 8;
        let target_mod = fixed_bytes % 16;
        sp -= (sp + 16 - target_mod) % 16;

        // AT_RANDOM 需要 16 字节随机数据，这里用时间扰动生成一份启动随机数。
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

        // auxv 按 Linux ABI 写入程序头、页大小、入口地址、解释器基址等信息。
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
        // AT_EXECFN 通常指 argv[0]；argc 为 0 时退回 platform 字符串。
        auxv_write!(
            31,
            string_addrs.first().copied().unwrap_or(platform_addr) as u64
        );
        auxv_write!(AT_NULL, 0u64);

        // envp 先写 NULL 结尾，再写每个环境变量字符串的用户地址。
        sp -= 8;
        *translated_refmut(token, sp as *mut u64) = 0;
        sp -= envc * 8;
        let envp_base = sp;
        for i in 0..envc {
            *translated_refmut(token, (envp_base + i * 8) as *mut u64) = env_addrs[i] as u64;
        }

        // argv 同样先写 NULL 结尾，保证用户态可以按 C 约定遍历。
        sp -= 8;
        *translated_refmut(token, sp as *mut u64) = 0;

        // argv 指针数组按原始参数顺序写入。
        sp -= argc * 8;
        let argv_base = sp;
        for i in 0..argc {
            *translated_refmut(token, (argv_base + i * 8) as *mut u64) = string_addrs[i] as u64;
        }

        // 最后写 argc；此时 sp 就是新程序入口看到的初始栈顶。
        sp -= 8;
        *translated_refmut(token, sp as *mut u64) = argc as u64;
        let crt_sp = sp;

        // exec 替换地址空间和寄存器现场，但保留 pid、父子关系和未标记 CLOEXEC 的 fd。
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
            // ELF 自带 TLS 段时，线程指针放到 TLS 区末尾。
            trap_cx[TrapFrameArgs::TLS] = tls_base + tls_size;
        } else {
            // 静态程序没有 TLS 段时补一页空 TLS，兼容依赖 tp 的用户态库。
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
    // fork 当前任务，地址空间使用 COW 复制，fd 和 cwd 等状态继承父进程。
    // 执行时先复制或共享父进程状态，再创建子任务控制块，最后把子任务加入调度队列。
    pub fn fork(self: &Arc<TaskControlBlock>) -> Option<Arc<TaskControlBlock>> {
        trace!("os::task::TaskControlBlock::fork");
        // fork 先复制地址空间；写映射页会通过 COW 共享，避免立即复制全部内存。
        let memory_set = {
            let parent_inner = self.inner_exclusive_access();
            MemorySet::from_existed_user(&parent_inner.memory_set)?
        };

        let (
            trap_cx_clone,
            base_size,
            fd_table,
            fd_cloexec,
            cwd,
            root,
            tid_ptr,
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
            // fd 表继承的是同一批文件对象，dup_ref 用来维护管道/socket 等引用计数。
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
                parent_inner.cwd.clone(),
                parent_inner.root.clone(),
                parent_inner.tid_ptr,
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
        let kstack = KernelStack::new();
        // 子任务继承父进程大部分执行环境，但运行时间、待处理信号和 children 重新开始。
        let task_control_block = Arc::new(TaskControlBlock {
            pid: pid_handle,
            inner: unsafe {
                UPSafeCell::new_named(
                    TaskControlBlockInner {
                        trap_cx: trap_cx_clone,
                        fp_state,
                        base_size,
                        task_cx: blank_kcontext(kstack.get_position().1), // 子任务使用自己的内核栈顶。
                        task_status: TaskStatus::Ready,
                        memory_set,
                        parent: Some(Arc::downgrade(self)),
                        children: Vec::new(),
                        exit_code: 0,
                        fd_table,
                        fd_cloexec,
                        cwd,
                        root,
                        tid_ptr,
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
        // 最后把子任务挂到父任务 children 里，供 waitpid 回收。
        {
            let mut parent_inner = self.inner_exclusive_access();
            parent_inner.children.push(task_control_block.clone());
        }
        Some(task_control_block)
    }
    // 返回任务 pid。
    // 执行时直接返回 PidHandle 里保存的编号。
    pub fn getpid(&self) -> usize {
        self.pid.0
    }
    // 从用户态进入内核时，把上一段时间记到 utime。
    // 执行时用当前时间减 last_time，累加到用户态运行时间，并刷新计时起点。
    pub fn record_time_user_to_kernel(&self) {
        let Some(mut inner) = self.try_inner_exclusive_access() else {
            return;
        };
        let now = polyhal::time::Time::now().to_nsec() as usize;
        inner.utime += now - inner.last_time;
        inner.last_time = now;
    }
    // 从内核态回用户态前，把上一段时间记到 stime。
    // 执行时把最近一段内核执行时间累加到 stime，并刷新 last_time。
    pub fn record_time_kernel_to_user(&self) {
        let Some(mut inner) = self.try_inner_exclusive_access() else {
            return;
        };
        let now = polyhal::time::Time::now().to_nsec() as usize;
        inner.stime += now - inner.last_time;
        inner.last_time = now;
    }
    // 任务挂起时，把当前内核段时间补到 stime。
    // 执行时先记录当前任务状态和等待原因，再切回调度器，等条件满足后继续运行。
    pub fn record_time_suspend(&self) {
        let Some(mut inner) = self.try_inner_exclusive_access() else {
            return;
        };
        let now = polyhal::time::Time::now().to_nsec() as usize;
        inner.stime += now - inner.last_time;
        inner.last_time = now;
    }
    // 任务恢复运行时重置计时起点。
    // 执行时把 last_time 设为当前时间，避免睡眠期间被计入任务运行时间。
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
    Futex(usize),
}

#[derive(Copy, Clone, PartialEq)]
pub enum TaskStatus {
    Ready,
    Running,
    Blocked,
    Zombie,
}
