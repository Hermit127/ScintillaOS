// 这里负责操作系统的文件管理和设备文件：实现 stdin/stdout 以及 /dev/null、/dev/zero、/dev/urandom 这类字符设备。
use polyhal::debug_console::DebugConsole;

use super::{File, Kstat, S_IFCHR};
use crate::task::{current_task, suspend_current_and_run_next};
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static STDOUT_BUSY: AtomicBool = AtomicBool::new(false);
static STDOUT_OWNER: AtomicI32 = AtomicI32::new(0);

// 让同一个进程连续写 stdout 时独占输出，减少 printf 被打散后互相穿插。
// 执行时读取当前 pid，若 stdout 空闲就用原子 compare_exchange 抢占，否则主动让出 CPU 等待。
fn stdout_acquire() {
    let pid = current_task().unwrap().getpid() as i32;
    loop {
        let owner = STDOUT_OWNER.load(Ordering::Acquire);
        if owner == pid {
            return;
        }
        if owner == 0
            && STDOUT_OWNER
                .compare_exchange(0, pid, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            return;
        }
        suspend_current_and_run_next();
    }
}

// 当前进程让出 CPU 前释放 stdout 所有权，避免别的进程一直等着。
// 执行时确认当前任务 pid 是否是 stdout owner，是的话用原子操作把 owner 清回 0。
pub fn stdout_release_if_owner() {
    if let Some(task) = current_task() {
        let pid = task.getpid() as i32;
        STDOUT_OWNER
            .compare_exchange(pid, 0, Ordering::Release, Ordering::Relaxed)
            .ok();
    }
}

pub struct Stdin;

pub struct Stdout;

impl File for Stdin {
    // 标准输入可以读。
    // 执行时直接返回 true，表示 read 可以从调试控制台等待输入。
    fn readable(&self) -> bool {
        true
    }
    // 标准输入不能写。
    // 执行时直接返回 false，禁止把 stdin 当输出设备使用。
    fn writable(&self) -> bool {
        false
    }
    // 从调试串口读一个字节；暂时没输入就主动切出去等。
    // 执行时先处理空缓冲区，再循环查询 DebugConsole；没字符时释放 stdout 并让出 CPU。
    fn read(&self, user_buf: &mut [u8]) -> isize {
        if user_buf.is_empty() {
            return 0;
        }
        loop {
            if let Some(c) = DebugConsole::getchar() {
                user_buf[0] = c;
                return 1;
            }
            stdout_release_if_owner();
            suspend_current_and_run_next();
        }
    }
    // 往 stdin 写是内核逻辑错误，直接暴露出来。
    // 执行时直接 panic，因为内核不应该把标准输入当成可写文件使用。
    fn write(&self, _user_buf: &mut [u8]) -> isize {
        panic!("Cannot write to stdin!");
    }
    // stdin 在用户态表现为字符设备。
    // 执行时构造字符设备类型的 Kstat，不需要访问真实设备元数据。
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFCHR)
    }
    // 给 fd 层做具体类型判断。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl File for Stdout {
    // 标准输出不能读。
    // 执行时直接返回 false，禁止从 stdout 读取。
    fn readable(&self) -> bool {
        false
    }
    // 标准输出可以写。
    // 执行时直接返回 true，表示 write 会输出到调试控制台。
    fn writable(&self) -> bool {
        true
    }
    // 从 stdout 读是内核逻辑错误，直接暴露出来。
    // 执行时直接 panic，因为标准输出不应该走 read 路径。
    fn read(&self, _user_buf: &mut [u8]) -> isize {
        panic!("Cannot read from stdout!");
    }
    // 把用户缓冲区逐字节写到调试串口。
    // 执行时先取得 stdout 所有权，再用 STDOUT_BUSY 自旋保护逐字节输出，最后返回输入长度。
    fn write(&self, user_buf: &mut [u8]) -> isize {
        stdout_acquire();
        while STDOUT_BUSY.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        for byte in user_buf.iter() {
            DebugConsole::putchar(*byte);
        }
        STDOUT_BUSY.store(false, Ordering::Release);
        // stdout 所有权保留到进程让出 CPU，减少多次 write 组成的输出被打断。
        user_buf.len() as isize
    }
    // stdout 在用户态表现为字符设备。
    // 执行时构造字符设备类型的 Kstat，不需要访问真实设备元数据。
    fn stat(&self) -> Kstat {
        Kstat::new(S_IFCHR)
    }
    // 给 fd 层做具体类型判断。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct Urandom;

// 生成字符设备通用的 stat，并填好 rdev。
// 执行时用传入权限构造字符设备 Kstat，再把 rdev 写成调用者指定的设备号。
fn char_device_stat(mode: u32, rdev: u64) -> Kstat {
    let mut st = Kstat::new(S_IFCHR | mode);
    st.st_rdev = rdev;
    st
}

impl File for Urandom {
    // /dev/urandom 可以读。
    // 执行时直接返回 true，让读取路径进入伪随机填充逻辑。
    fn readable(&self) -> bool {
        true
    }
    // /dev/urandom 这里不接收写入。
    // 执行时直接返回 false，当前实现不把用户写入混入随机状态。
    fn writable(&self) -> bool {
        false
    }
    // 生成一串够测例使用的伪随机字节。
    // 执行时用当前时间扰动初始状态，再用 xorshift/LCG 混合生成每个字节。
    fn read(&self, buf: &mut [u8]) -> isize {
        let mut state: u32 =
            0x92f4a3b7u32.wrapping_add(polyhal::time::Time::now().to_msec() as u32);
        for b in buf.iter_mut() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *b = state as u8;
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
        }
        buf.len() as isize
    }
    // 写 urandom 直接丢弃，按成功但没有实际消费处理。
    // 执行时不读取用户数据，直接返回 0，表示当前没有采集熵输入。
    fn write(&self, _buf: &mut [u8]) -> isize {
        0
    }
    // 返回 /dev/urandom 的字符设备 stat。
    // 执行时调用字符设备 stat helper，并填入 urandom 对应的 rdev。
    fn stat(&self) -> Kstat {
        char_device_stat(0o444, 0x0109)
    }
    // 给 fd 层做具体类型判断。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct DevNull;

impl File for DevNull {
    // /dev/null 可读，读出来就是 EOF。
    // 执行时直接返回 true，让 read 路径可以返回 EOF。
    fn readable(&self) -> bool {
        true
    }
    // /dev/null 可写，写入内容会被丢掉。
    // 执行时直接返回 true，让 write 路径接受并丢弃所有字节。
    fn writable(&self) -> bool {
        true
    }
    // 从 /dev/null 读取永远返回 0。
    // 执行时不填充缓冲区，直接返回 0，表示 EOF。
    fn read(&self, _buf: &mut [u8]) -> isize {
        0
    }
    // 写 /dev/null 直接报告全部写入。
    // 执行时只返回缓冲区长度，不保存也不输出任何内容。
    fn write(&self, buf: &mut [u8]) -> isize {
        buf.len() as isize
    }
    // 返回 /dev/null 的字符设备 stat。
    // 执行时调用字符设备 stat helper，并填入 null 对应的 rdev。
    fn stat(&self) -> Kstat {
        char_device_stat(0o666, 0x0103)
    }
    // 给 fd 层做具体类型判断。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

pub struct DevZero;

impl File for DevZero {
    // /dev/zero 可读。
    // 执行时直接返回 true，让 read 路径把用户缓冲区清零。
    fn readable(&self) -> bool {
        true
    }
    // /dev/zero 也允许写，写入内容会被丢掉。
    // 执行时直接返回 true，让 write 路径接受并丢弃所有字节。
    fn writable(&self) -> bool {
        true
    }
    // 从 /dev/zero 读取时把缓冲区填成 0。
    // 执行时遍历 buf 并写入 0，最后返回填充的字节数。
    fn read(&self, buf: &mut [u8]) -> isize {
        for b in buf.iter_mut() {
            *b = 0;
        }
        buf.len() as isize
    }
    // 写 /dev/zero 直接报告全部写入。
    // 执行时只返回缓冲区长度，不保存也不输出任何内容。
    fn write(&self, buf: &mut [u8]) -> isize {
        buf.len() as isize
    }
    // 返回 /dev/zero 的字符设备 stat。
    // 执行时调用字符设备 stat helper，并填入 zero 对应的 rdev。
    fn stat(&self) -> Kstat {
        char_device_stat(0o666, 0x0105)
    }
    // 给 fd 层做具体类型判断。
    // 执行时直接把当前对象转成 Any trait 引用，方便文件描述符层做具体类型判断。
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
