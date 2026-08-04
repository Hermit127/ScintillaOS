use super::{File, Kstat, S_IFCHR};
use crate::fs::stdio::stdout_release_if_owner;
use crate::sync::DeviceLock;
use crate::task::{
    block_current_and_run_next_if, current_task, has_interrupting_signal, wake_poll_wait_tasks,
    BlockReason,
};

const EFD_COUNTER_MAX: u64 = u64::MAX - 1;
const EFD_NONBLOCK: usize = 0x800;
const EFD_SEMAPHORE: usize = 0x1;

struct EventFdState {
    counter: u64,
    nonblock: bool,
    semaphore: bool,
}

pub struct EventFdFile {
    state: DeviceLock<EventFdState>,
}

impl EventFdFile {
    pub fn new(initval: u32, flags: usize) -> Self {
        Self {
            state: DeviceLock::new_named(
                EventFdState {
                    counter: initval as u64,
                    nonblock: flags & EFD_NONBLOCK != 0,
                    semaphore: flags & EFD_SEMAPHORE != 0,
                },
                "EventFdState",
            ),
        }
    }

    fn interrupted() -> bool {
        let Some(task) = current_task() else {
            return false;
        };
        let Some(inner) = task.try_inner_borrow() else {
            return false;
        };
        has_interrupting_signal(&inner)
    }
}

impl File for EventFdFile {
    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, buf: &mut [u8]) -> isize {
        if buf.len() != core::mem::size_of::<u64>() {
            return -22;
        }
        loop {
            if Self::interrupted() {
                return -4;
            }
            let value = {
                let mut state = self.state.exclusive_access();
                if state.counter == 0 {
                    None
                } else if state.semaphore {
                    state.counter -= 1;
                    Some(1)
                } else {
                    let value = state.counter;
                    state.counter = 0;
                    Some(value)
                }
            };
            if let Some(value) = value {
                buf.copy_from_slice(&value.to_ne_bytes());
                wake_poll_wait_tasks();
                return buf.len() as isize;
            }
            if self.state.exclusive_access().nonblock {
                return -11;
            }
            stdout_release_if_owner();
            block_current_and_run_next_if(BlockReason::PollWait, || {
                self.state.exclusive_access().counter == 0
            });
        }
    }

    fn write(&self, buf: &mut [u8]) -> isize {
        if buf.len() != core::mem::size_of::<u64>() {
            return -22;
        }
        let value = u64::from_ne_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        if value == u64::MAX {
            return -22;
        }

        loop {
            if Self::interrupted() {
                return -4;
            }
            let written = {
                let mut state = self.state.exclusive_access();
                if state.counter <= EFD_COUNTER_MAX - value {
                    state.counter += value;
                    true
                } else {
                    false
                }
            };
            if written {
                wake_poll_wait_tasks();
                return buf.len() as isize;
            }
            if self.state.exclusive_access().nonblock {
                return -11;
            }
            stdout_release_if_owner();
            block_current_and_run_next_if(BlockReason::PollWait, || {
                self.state.exclusive_access().counter > EFD_COUNTER_MAX - value
            });
        }
    }

    fn stat(&self) -> Kstat {
        Kstat::new(S_IFCHR | 0o600)
    }

    fn get_flags(&self) -> usize {
        let state = self.state.exclusive_access();
        0x8000 | 2 | if state.nonblock { EFD_NONBLOCK } else { 0 }
    }

    fn set_flags(&self, flags: usize) -> bool {
        let mut state = self.state.exclusive_access();
        state.nonblock = flags & EFD_NONBLOCK != 0;
        true
    }

    fn poll(&self) -> (bool, bool, bool) {
        let state = self.state.exclusive_access();
        (state.counter != 0, state.counter != EFD_COUNTER_MAX, false)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
