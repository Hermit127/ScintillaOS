use super::{File, Kstat, S_IFCHR};
use crate::fs::stdio::stdout_release_if_owner;
use crate::sync::DeviceLock;
use crate::task::{
    block_current_and_run_next_if, block_current_until_if, current_task, has_interrupting_signal,
    publish_itimer_deadline, wake_poll_wait_tasks, BlockReason,
};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use lazy_static::lazy_static;

pub const CLOCK_REALTIME: i32 = 0;
pub const CLOCK_MONOTONIC: i32 = 1;
pub const CLOCK_BOOTTIME: i32 = 7;

pub const TFD_TIMER_ABSTIME: usize = 1;
pub const TFD_TIMER_CANCEL_ON_SET: usize = 2;
pub const TFD_NONBLOCK: usize = 0x800;

#[derive(Clone, Copy, Default)]
pub struct TimerFdSpec {
    pub interval_ns: u64,
    pub value_ns: u64,
}

struct TimerFdState {
    clock_id: i32,
    nonblock: bool,
    interval_ns: u64,
    deadline_ns: u64,
    expirations: u64,
}

pub struct TimerFdFile {
    state: DeviceLock<TimerFdState>,
    readiness_generation: AtomicU64,
}

lazy_static! {
    static ref TIMERFD_REGISTRY: DeviceLock<Vec<Weak<TimerFdFile>>> =
        DeviceLock::new_named(Vec::new(), "TimerFdRegistry");
}

fn monotonic_now_ns() -> u64 {
    polyhal::time::Time::now().to_nsec() as u64
}

fn realtime_now_ns() -> u64 {
    crate::time::realtime_us().saturating_mul(1_000)
}

fn clock_now_ns(clock_id: i32) -> u64 {
    if clock_id == CLOCK_REALTIME {
        realtime_now_ns()
    } else {
        monotonic_now_ns()
    }
}

impl TimerFdFile {
    pub fn new(clock_id: i32, flags: usize) -> Self {
        Self {
            state: DeviceLock::new_named(
                TimerFdState {
                    clock_id,
                    nonblock: flags & TFD_NONBLOCK != 0,
                    interval_ns: 0,
                    deadline_ns: 0,
                    expirations: 0,
                },
                "TimerFdState",
            ),
            readiness_generation: AtomicU64::new(0),
        }
    }

    pub fn register(file: &Arc<Self>) {
        let mut registry = TIMERFD_REGISTRY.exclusive_access();
        registry.retain(|entry| entry.strong_count() != 0);
        registry.push(Arc::downgrade(file));
    }

    pub fn clock_id(&self) -> i32 {
        self.state.exclusive_access().clock_id
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

    fn refresh_locked(&self, state: &mut TimerFdState, now_ns: u64) -> bool {
        if state.deadline_ns == 0 || now_ns < state.deadline_ns {
            return false;
        }
        let count = if state.interval_ns == 0 {
            1
        } else {
            now_ns.saturating_sub(state.deadline_ns) / state.interval_ns + 1
        };
        let was_empty = state.expirations == 0;
        state.expirations = state.expirations.saturating_add(count);
        if state.interval_ns == 0 {
            state.deadline_ns = 0;
        } else {
            state.deadline_ns = state
                .deadline_ns
                .saturating_add(count.saturating_mul(state.interval_ns));
        }
        if was_empty {
            self.readiness_generation.fetch_add(1, Ordering::Release);
        }
        was_empty
    }

    fn refresh(&self, now_ns: u64) -> (bool, u64) {
        let mut state = self.state.exclusive_access();
        let became_ready = self.refresh_locked(&mut state, now_ns);
        (became_ready, state.deadline_ns)
    }

    pub fn set_time(&self, flags: usize, new_value: TimerFdSpec) -> TimerFdSpec {
        let now_monotonic = monotonic_now_ns();
        let (old_value, deadline_ns) = {
            let mut state = self.state.exclusive_access();
            self.refresh_locked(&mut state, now_monotonic);
            let old_value = TimerFdSpec {
                interval_ns: state.interval_ns,
                value_ns: state.deadline_ns.saturating_sub(now_monotonic),
            };
            let deadline_ns = if new_value.value_ns == 0 {
                0
            } else if flags & TFD_TIMER_ABSTIME == 0 {
                now_monotonic.saturating_add(new_value.value_ns)
            } else {
                let clock_now = clock_now_ns(state.clock_id);
                now_monotonic.saturating_add(new_value.value_ns.saturating_sub(clock_now))
            };
            let was_ready = state.expirations != 0;
            state.interval_ns = new_value.interval_ns;
            state.deadline_ns = deadline_ns;
            state.expirations = 0;
            self.refresh_locked(&mut state, now_monotonic);
            if was_ready && state.expirations == 0 {
                self.readiness_generation.fetch_add(1, Ordering::Release);
            }
            (old_value, state.deadline_ns)
        };
        if deadline_ns != 0 {
            publish_itimer_deadline(deadline_ns.saturating_add(999) / 1_000);
        }
        // A setter may arm, disarm, or move the deadline while another thread
        // is blocked in read/epoll.  Waking poll waiters makes each of those
        // transitions recheck the authoritative state without a lost wakeup.
        wake_poll_wait_tasks();
        old_value
    }

    pub fn get_time(&self) -> TimerFdSpec {
        let now = monotonic_now_ns();
        let (became_ready, _) = self.refresh(now);
        if became_ready {
            wake_poll_wait_tasks();
        }
        let state = self.state.exclusive_access();
        TimerFdSpec {
            interval_ns: state.interval_ns,
            value_ns: state.deadline_ns.saturating_sub(now),
        }
    }
}

/// Advance all live timerfds from the timer-interrupt path.
///
/// The process timer deadline hint is shared deliberately: it gives timerfd
/// poll/epoll waiters the same race-free scheduler wakeup mechanism as POSIX
/// timers without introducing a second hardware timer queue.
pub fn check_timerfds(now_us: u64) {
    let now_ns = now_us.saturating_mul(1_000);
    let (became_ready, next_deadline_ns) = {
        let mut registry = TIMERFD_REGISTRY.exclusive_access();
        let mut became_ready = false;
        let mut next_deadline_ns = u64::MAX;
        let mut index = 0;
        while index < registry.len() {
            let Some(timerfd) = registry[index].upgrade() else {
                registry.remove(index);
                continue;
            };
            let (ready, deadline) = timerfd.refresh(now_ns);
            became_ready |= ready;
            if deadline != 0 {
                next_deadline_ns = next_deadline_ns.min(deadline);
            }
            index += 1;
        }
        (became_ready, next_deadline_ns)
    };
    if next_deadline_ns != u64::MAX {
        publish_itimer_deadline(next_deadline_ns.saturating_add(999) / 1_000);
    }
    if became_ready {
        wake_poll_wait_tasks();
    }
}

impl File for TimerFdFile {
    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        false
    }

    fn read(&self, buf: &mut [u8]) -> isize {
        if buf.len() < core::mem::size_of::<u64>() {
            return -22;
        }
        loop {
            if Self::interrupted() {
                return -4;
            }
            let now = monotonic_now_ns();
            let (value, nonblock, deadline) = {
                let mut state = self.state.exclusive_access();
                self.refresh_locked(&mut state, now);
                let value = state.expirations;
                if value != 0 {
                    state.expirations = 0;
                    self.readiness_generation.fetch_add(1, Ordering::Release);
                }
                (value, state.nonblock, state.deadline_ns)
            };
            if value != 0 {
                buf[..core::mem::size_of::<u64>()].copy_from_slice(&value.to_ne_bytes());
                wake_poll_wait_tasks();
                return core::mem::size_of::<u64>() as isize;
            }
            if nonblock {
                return -11;
            }
            stdout_release_if_owner();
            if deadline == 0 {
                block_current_and_run_next_if(BlockReason::PollWait, || {
                    self.state.exclusive_access().expirations == 0
                });
            } else {
                block_current_until_if(BlockReason::PollWait, deadline as usize, || {
                    let now = monotonic_now_ns();
                    let mut state = self.state.exclusive_access();
                    self.refresh_locked(&mut state, now);
                    state.expirations == 0
                });
            }
        }
    }

    fn write(&self, _buf: &mut [u8]) -> isize {
        -22
    }

    fn stat(&self) -> Kstat {
        Kstat::new(S_IFCHR | 0o600)
    }

    fn get_flags(&self) -> usize {
        let state = self.state.exclusive_access();
        0x8000 | if state.nonblock { TFD_NONBLOCK } else { 0 }
    }

    fn set_flags(&self, flags: usize) -> bool {
        let mut state = self.state.exclusive_access();
        state.nonblock = flags & TFD_NONBLOCK != 0;
        true
    }

    fn poll(&self) -> (bool, bool, bool) {
        let now = monotonic_now_ns();
        let mut state = self.state.exclusive_access();
        self.refresh_locked(&mut state, now);
        (state.expirations != 0, false, false)
    }

    fn readiness_generation(&self) -> u64 {
        self.readiness_generation.load(Ordering::Acquire)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
