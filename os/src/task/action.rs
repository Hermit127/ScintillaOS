use crate::task::{SignalFlags, MAX_SIG};

/// Action for a signal
/// Layout matches RISC-V kernel sigaction: handler(8) | flags(8) | restorer(8) | mask(8)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SignalAction {
    pub handler: usize,
    pub flags: usize,
    pub restorer: usize,
    pub mask: SignalFlags,
}

impl Default for SignalAction {
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
    fn default() -> Self {
        Self {
            table: [SignalAction::default(); MAX_SIG + 1],
        }
    }
}
