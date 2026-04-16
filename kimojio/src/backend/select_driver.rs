// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Minimal socket-readiness driver for the Windows IOCP backend.
//!
//! Uses Winsock's `WSAPoll()` to wait for socket readiness, mirroring the role
//! that epoll plays in the epoll backend.  Sockets **must** be in
//! non-blocking mode (`FIONBIO`).

use std::collections::HashMap;
use std::task::Waker;
use std::time::Duration;

use rustix::fd::RawFd;
use windows_sys::Win32::Networking::WinSock::{POLLOUT, POLLRDNORM, SOCKET, WSAPOLLFD, WSAPoll};

// On 64-bit Windows: RawFd (= RawSocket = u64) == SOCKET (usize, also 8 bytes).

pub struct SelectDriver {
    read_wakers: HashMap<RawFd, Waker>,
    write_wakers: HashMap<RawFd, Waker>,
}

impl SelectDriver {
    pub fn new() -> Self {
        Self {
            read_wakers: HashMap::new(),
            write_wakers: HashMap::new(),
        }
    }

    pub fn register_read(&mut self, fd: RawFd, waker: Waker) {
        self.read_wakers.insert(fd, waker);
    }

    pub fn register_write(&mut self, fd: RawFd, waker: Waker) {
        self.write_wakers.insert(fd, waker);
    }

    pub fn deregister(&mut self, fd: RawFd) {
        self.read_wakers.remove(&fd);
        self.write_wakers.remove(&fd);
    }

    /// Collect ready sockets and return their wakers.
    ///
    /// `want > 0`: block up to 10 ms; `want == 0`: non-blocking poll.
    pub fn collect_ready(&mut self, want: usize) -> Vec<Waker> {
        if self.read_wakers.is_empty() && self.write_wakers.is_empty() {
            if want > 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
            return Vec::new();
        }

        // Build a WSAPOLLFD array — one entry per registered fd.
        let mut pollfds: Vec<WSAPOLLFD> = Vec::new();

        for &fd in self.read_wakers.keys() {
            pollfds.push(WSAPOLLFD {
                fd: fd as SOCKET,
                events: POLLRDNORM,
                revents: 0,
            });
        }
        for &fd in self.write_wakers.keys() {
            pollfds.push(WSAPOLLFD {
                fd: fd as SOCKET,
                events: POLLOUT,
                revents: 0,
            });
        }

        let timeout_ms: i32 = if want == 0 { 0 } else { 10 };

        // SAFETY: pollfds contains valid SOCKET handles.
        let result = unsafe { WSAPoll(pollfds.as_mut_ptr(), pollfds.len() as u32, timeout_ms) };

        if result <= 0 {
            return Vec::new();
        }

        let mut wakers = Vec::new();

        let read_fds: Vec<RawFd> = self.read_wakers.keys().copied().collect();
        let write_fds: Vec<RawFd> = self.write_wakers.keys().copied().collect();

        for fd in read_fds {
            if pollfds
                .iter()
                .any(|p| p.fd == fd as SOCKET && p.revents != 0)
            {
                if let Some(w) = self.read_wakers.remove(&fd) {
                    wakers.push(w);
                }
            }
        }
        for fd in write_fds {
            if pollfds
                .iter()
                .any(|p| p.fd == fd as SOCKET && p.revents != 0)
            {
                if let Some(w) = self.write_wakers.remove(&fd) {
                    wakers.push(w);
                }
            }
        }

        wakers
    }
}

impl Default for SelectDriver {
    fn default() -> Self {
        Self::new()
    }
}
