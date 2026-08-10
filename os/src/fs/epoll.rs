use super::{File, Kstat, S_IFCHR};
use crate::sync::DeviceLock;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

pub const EPOLL_MAX_INTERESTS: usize = 1024;

pub const EPOLLIN: u32 = 0x0000_0001;
pub const EPOLLPRI: u32 = 0x0000_0002;
pub const EPOLLOUT: u32 = 0x0000_0004;
pub const EPOLLERR: u32 = 0x0000_0008;
pub const EPOLLHUP: u32 = 0x0000_0010;
pub const EPOLLRDNORM: u32 = 0x0000_0040;
pub const EPOLLRDBAND: u32 = 0x0000_0080;
pub const EPOLLWRNORM: u32 = 0x0000_0100;
pub const EPOLLWRBAND: u32 = 0x0000_0200;
pub const EPOLLMSG: u32 = 0x0000_0400;
pub const EPOLLRDHUP: u32 = 0x0000_2000;
pub const EPOLLEXCLUSIVE: u32 = 0x1000_0000;
pub const EPOLLWAKEUP: u32 = 0x2000_0000;
pub const EPOLLONESHOT: u32 = 0x4000_0000;
pub const EPOLLET: u32 = 0x8000_0000;

const EPOLL_SUPPORTED_EVENTS: u32 = EPOLLIN
    | EPOLLPRI
    | EPOLLOUT
    | EPOLLERR
    | EPOLLHUP
    | EPOLLRDNORM
    | EPOLLRDBAND
    | EPOLLWRNORM
    | EPOLLWRBAND
    | EPOLLMSG
    | EPOLLRDHUP
    | EPOLLEXCLUSIVE
    | EPOLLWAKEUP
    | EPOLLONESHOT
    | EPOLLET;

#[derive(Clone, Copy)]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

#[derive(Clone)]
struct EpollInterest {
    fd: usize,
    file: Weak<dyn File + Send + Sync>,
    event: EpollEvent,
    generation: u64,
    last_ready: u32,
    last_readiness_generation: u64,
    oneshot_disabled: bool,
}

struct EpollState {
    interests: Vec<EpollInterest>,
    next_generation: u64,
}

#[derive(Clone)]
struct EpollSample {
    fd: usize,
    file: Arc<dyn File + Send + Sync>,
    event: EpollEvent,
    generation: u64,
    last_ready: u32,
    last_readiness_generation: u64,
    oneshot_disabled: bool,
}

pub struct EpollFile {
    state: DeviceLock<EpollState>,
}

impl EpollFile {
    pub fn new() -> Result<Self, isize> {
        let mut interests = Vec::new();
        interests
            .try_reserve_exact(EPOLL_MAX_INTERESTS)
            .map_err(|_| -12isize)?;
        Ok(Self {
            state: DeviceLock::new_named(
                EpollState {
                    interests,
                    next_generation: 1,
                },
                "EpollState",
            ),
        })
    }

    pub fn add(
        &self,
        fd: usize,
        file: Arc<dyn File + Send + Sync>,
        event: EpollEvent,
    ) -> Result<(), isize> {
        if event.events & !EPOLL_SUPPORTED_EVENTS != 0 {
            return Err(-22);
        }

        let mut state = self.state.exclusive_access();
        state.interests.retain(|entry| entry.file.strong_count() != 0);
        if state.interests.iter().any(|entry| {
            entry.fd == fd
                && entry
                    .file
                    .upgrade()
                    .is_some_and(|watched| Arc::ptr_eq(&watched, &file))
        }) {
            Err(-17)
        } else if state.interests.len() == EPOLL_MAX_INTERESTS {
            Err(-28)
        } else {
            let generation = state.next_generation;
            state.next_generation = state.next_generation.wrapping_add(1);
            state.interests.push(EpollInterest {
                fd,
                last_readiness_generation: file.readiness_generation(),
                file: Arc::downgrade(&file),
                event,
                generation,
                last_ready: 0,
                oneshot_disabled: false,
            });
            Ok(())
        }
    }

    pub fn modify(
        &self,
        fd: usize,
        file: &Arc<dyn File + Send + Sync>,
        event: EpollEvent,
    ) -> Result<(), isize> {
        if event.events & !EPOLL_SUPPORTED_EVENTS != 0 {
            return Err(-22);
        }
        let mut state = self.state.exclusive_access();
        let generation = state.next_generation;
        state.next_generation = state.next_generation.wrapping_add(1);
        let Some(entry) = state.interests.iter_mut().find(|entry| {
            entry.fd == fd
                && entry
                    .file
                    .upgrade()
                    .is_some_and(|watched| Arc::ptr_eq(&watched, file))
        }) else {
            return Err(-2);
        };
        entry.event = event;
        entry.generation = generation;
        entry.last_ready = 0;
        entry.last_readiness_generation = file.readiness_generation();
        entry.oneshot_disabled = false;
        Ok(())
    }

    pub fn remove(
        &self,
        fd: usize,
        file: &Arc<dyn File + Send + Sync>,
    ) -> Result<(), isize> {
        let mut state = self.state.exclusive_access();
        let Some(index) = state.interests.iter().position(|entry| {
            entry.fd == fd
                && entry
                    .file
                    .upgrade()
                    .is_some_and(|watched| Arc::ptr_eq(&watched, file))
        }) else {
            return Err(-2);
        };
        state.interests.remove(index);
        Ok(())
    }

    fn snapshot(&self) -> Result<Vec<EpollSample>, isize> {
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(EPOLL_MAX_INTERESTS)
            .map_err(|_| -12isize)?;
        let mut state = self.state.exclusive_access();
        let mut index = 0;
        while index < state.interests.len() {
            let entry = &state.interests[index];
            let Some(file) = entry.file.upgrade() else {
                state.interests.remove(index);
                continue;
            };
            samples.push(EpollSample {
                fd: entry.fd,
                file,
                event: entry.event,
                generation: entry.generation,
                last_ready: entry.last_ready,
                last_readiness_generation: entry.last_readiness_generation,
                oneshot_disabled: entry.oneshot_disabled,
            });
            index += 1;
        }
        Ok(samples)
    }

    fn ready_mask(event: u32, readable: bool, writable: bool, hung_up: bool) -> u32 {
        let mut ready = 0;
        if readable {
            ready |= event & (EPOLLIN | EPOLLRDNORM | EPOLLRDBAND);
        }
        if writable {
            ready |= event & (EPOLLOUT | EPOLLWRNORM | EPOLLWRBAND);
        }
        if hung_up {
            // File::poll exposes one exceptional state.  Existing poll/select
            // users interpret it as a hangup, so keep that meaning here.
            ready |= EPOLLHUP;
        }
        ready
    }

    pub fn collect_ready(&self, maxevents: usize) -> Result<Vec<EpollEvent>, isize> {
        let samples = self.snapshot()?;
        let mut sampled_ready = Vec::new();
        sampled_ready
            .try_reserve_exact(samples.len())
            .map_err(|_| -12isize)?;
        for sample in samples {
            let (readable, writable, hung_up) = sample.file.poll();
            let ready = Self::ready_mask(sample.event.events, readable, writable, hung_up);
            let readiness_generation = sample.file.readiness_generation();
            sampled_ready.push((sample, ready, readiness_generation));
        }

        let mut ready_events = Vec::new();
        ready_events
            .try_reserve_exact(maxevents)
            .map_err(|_| -12isize)?;
        let mut state = self.state.exclusive_access();
        for (sample, ready, readiness_generation) in sampled_ready {
            let Some(entry) = state
                .interests
                .iter_mut()
                .find(|entry| entry.fd == sample.fd && entry.generation == sample.generation)
            else {
                continue;
            };

            if ready == 0 {
                entry.last_ready = 0;
                entry.last_readiness_generation = readiness_generation;
                continue;
            }
            if entry.oneshot_disabled || ready_events.len() == maxevents {
                continue;
            }

            let edge_triggered = entry.event.events & EPOLLET != 0;
            let newly_ready = ready & !entry.last_ready;
            let readiness_changed = readiness_generation != entry.last_readiness_generation;
            if !edge_triggered || newly_ready != 0 || readiness_changed {
                ready_events.push(EpollEvent {
                    events: if edge_triggered && newly_ready != 0 {
                        newly_ready
                    } else {
                        ready
                    },
                    data: entry.event.data,
                });
                if entry.event.events & EPOLLONESHOT != 0 {
                    entry.oneshot_disabled = true;
                }
            }
            entry.last_ready = ready;
            entry.last_readiness_generation = readiness_generation;
        }
        Ok(ready_events)
    }

    pub fn has_ready(&self) -> bool {
        let Ok(samples) = self.snapshot() else {
            return false;
        };
        samples.into_iter().any(|sample| {
            if sample.oneshot_disabled {
                return false;
            }
            let (readable, writable, hung_up) = sample.file.poll();
            let ready = Self::ready_mask(sample.event.events, readable, writable, hung_up);
            ready != 0
                && (sample.event.events & EPOLLET == 0
                    || ready & !sample.last_ready != 0
                    || sample.file.readiness_generation() != sample.last_readiness_generation)
        })
    }
}

impl File for EpollFile {
    fn readable(&self) -> bool {
        false
    }

    fn writable(&self) -> bool {
        false
    }

    fn read(&self, _buf: &mut [u8]) -> isize {
        -22
    }

    fn write(&self, _buf: &mut [u8]) -> isize {
        -22
    }

    fn stat(&self) -> Kstat {
        Kstat::new(S_IFCHR | 0o600)
    }

    fn poll(&self) -> (bool, bool, bool) {
        (self.has_ready(), false, false)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
