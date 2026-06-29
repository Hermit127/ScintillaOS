// 这里负责操作系统的信号管理：定义 sigaction 相关结构和默认信号动作表。
use crate::task::{SignalFlags, MAX_SIG};

// 单个信号的处理动作，布局要和用户态 sigaction 结构对上。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SignalAction {
    pub handler: usize,
    pub flags: usize,
    pub restorer: usize,
    pub mask: SignalFlags,
}

impl Default for SignalAction {
    // 默认信号动作：没有用户 handler，也没有额外屏蔽字。
    // 执行时先定位缺页地址对应的映射区域，再分配或复制物理页，最后更新页表。
    fn default() -> Self {
        Self {
            handler: 0,
            flags: 0,
            restorer: 0,
            mask: SignalFlags::empty(),
        }
    }
}

#[derive(Clone)]
pub struct SignalActions {
    pub table: [SignalAction; MAX_SIG + 1],
}

impl Default for SignalActions {
    // 给所有信号准备默认动作表。
    // 执行时先定位缺页地址对应的映射区域，再分配或复制物理页，最后更新页表。
    fn default() -> Self {
        Self {
            table: [SignalAction::default(); MAX_SIG + 1],
        }
    }
}

impl SignalActions {
    pub fn reset_for_exec(&mut self) {
        for action in self.table.iter_mut() {
            if action.handler == 1 {
                *action = SignalAction {
                    handler: 1,
                    ..SignalAction::default()
                };
            } else if action.handler != 0 {
                *action = SignalAction::default();
            }
        }
    }
}
