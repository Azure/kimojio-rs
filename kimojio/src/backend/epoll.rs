// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! epoll backend for the kimojio runtime.

use std::collections::HashMap;
use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::task::Waker;

use rustix::buffer::spare_capacity;
use rustix::event::{EventfdFlags, Timespec, epoll, eventfd};

const EPOLL_EVENT_BATCH: usize = 64;
const EVENTFD_TOKEN: u64 = u64::MAX;

struct FdWakers {
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
}

pub struct EpollDriver {
    epoll_fd: OwnedFd,
    event_fd: OwnedFd,
    wakers: HashMap<RawFd, FdWakers>,
}

impl EpollDriver {
    pub fn new() -> Self {
        let epoll_fd = epoll::create(epoll::CreateFlags::CLOEXEC).unwrap();
        let event_fd = eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC).unwrap();
        epoll::add(
            &epoll_fd,
            &event_fd,
            epoll::EventData::new_u64(EVENTFD_TOKEN),
            epoll::EventFlags::IN,
        )
        .unwrap();
        EpollDriver {
            epoll_fd,
            event_fd,
            wakers: HashMap::new(),
        }
    }

    pub fn register_read(&mut self, fd: RawFd, waker: Waker) {
        self.update_waker(fd, Some(waker), None);
    }

    pub fn register_write(&mut self, fd: RawFd, waker: Waker) {
        self.update_waker(fd, None, Some(waker));
    }

    fn update_waker(&mut self, fd: RawFd, read: Option<Waker>, write: Option<Waker>) {
        // SAFETY: fd is a valid raw fd while it's registered with us
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let entry = self.wakers.entry(fd);
        match entry {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let w = e.get_mut();
                if let Some(r) = read {
                    w.read_waker = Some(r);
                }
                if let Some(w2) = write {
                    w.write_waker = Some(w2);
                }
                let flags = Self::compute_flags_for(e.get());
                let _ = epoll::modify(
                    &self.epoll_fd,
                    borrowed,
                    epoll::EventData::new_u64(fd as u64),
                    flags,
                );
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                let entry = e.insert(FdWakers {
                    read_waker: read,
                    write_waker: write,
                });
                let flags = Self::compute_flags_for(entry);
                let _ = epoll::add(
                    &self.epoll_fd,
                    borrowed,
                    epoll::EventData::new_u64(fd as u64),
                    flags,
                );
            }
        }
    }

    fn compute_flags_for(w: &FdWakers) -> epoll::EventFlags {
        let mut flags = epoll::EventFlags::empty();
        if w.read_waker.is_some() {
            flags |= epoll::EventFlags::IN;
        }
        if w.write_waker.is_some() {
            flags |= epoll::EventFlags::OUT;
        }
        flags
    }

    pub fn deregister(&mut self, fd: RawFd) {
        if self.wakers.remove(&fd).is_some() {
            // SAFETY: fd is valid since it was registered
            let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
            let _ = epoll::delete(&self.epoll_fd, borrowed);
        }
    }

    pub fn trigger_nop(&self) {
        let _ = rustix::io::write(&self.event_fd, &1u64.to_ne_bytes());
    }

    /// Wait for I/O readiness events and return wakers to fire.
    /// `want == 0`: non-blocking; `want > 0`: block up to 10ms.
    pub fn collect_ready(&mut self, want: usize) -> Vec<Waker> {
        let timeout = if want == 0 {
            Some(Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            })
        } else {
            Some(Timespec {
                tv_sec: 0,
                tv_nsec: 10_000_000,
            }) // 10ms
        };

        let mut event_list = Vec::<epoll::Event>::with_capacity(EPOLL_EVENT_BATCH);
        if epoll::wait(
            &self.epoll_fd,
            spare_capacity(&mut event_list),
            timeout.as_ref(),
        )
        .is_err()
        {
            return Vec::new();
        }

        let mut wakers = Vec::new();
        for event in event_list.drain(..) {
            let token = event.data.u64();
            if token == EVENTFD_TOKEN {
                let mut buf = [0u8; 8];
                let _ = rustix::io::read(&self.event_fd, &mut buf);
                continue;
            }
            let fd = token as RawFd;
            if let Some(w) = self.wakers.get_mut(&fd) {
                // Copy flags to avoid creating a reference to a packed struct field
                let flags = event.flags;
                if flags.intersects(
                    epoll::EventFlags::IN | epoll::EventFlags::HUP | epoll::EventFlags::ERR,
                ) {
                    if let Some(waker) = w.read_waker.take() {
                        wakers.push(waker);
                    }
                }
                if flags.intersects(
                    epoll::EventFlags::OUT | epoll::EventFlags::HUP | epoll::EventFlags::ERR,
                ) {
                    if let Some(waker) = w.write_waker.take() {
                        wakers.push(waker);
                    }
                }
                if w.read_waker.is_none() && w.write_waker.is_none() {
                    self.wakers.remove(&fd);
                    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
                    let _ = epoll::delete(&self.epoll_fd, borrowed);
                }
            }
        }
        wakers
    }
}

impl Default for EpollDriver {
    fn default() -> Self {
        Self::new()
    }
}
