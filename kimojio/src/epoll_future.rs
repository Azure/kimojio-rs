// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Future types for the epoll backend that perform real non-blocking I/O.
//!
//! Fds passed to these futures **must** be in non-blocking mode (`O_NONBLOCK`).
//! The futures register for readiness with the per-thread `EpollDriver` on
//! `EAGAIN`/`EWOULDBLOCK`, and retry the syscall when the fd becomes ready.

use std::future::Future;
use std::marker::PhantomData;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::FusedFuture;
use rustix::io::Errno;
use rustix::net::{RecvFlags, SendFlags};

use crate::operations::IsIoPoll;
use crate::task::TaskState;

// ---------------------------------------------------------------------------
// EpollUnitFuture – Result<(), Errno>
// ---------------------------------------------------------------------------

enum EpollUnitInner {
    /// Immediately-ready result (nop, close, shutdown, …).
    Ready(Result<(), Errno>),
    /// Timerfd-based sleep: wait until the fd becomes readable.
    TimerFd { fd: OwnedFd, registered: bool },
}

/// Future that resolves to `Result<(), Errno>`.
///
/// Used for synchronous-ish operations (nop, close, shutdown) **and** for
/// timerfd-backed sleep under the epoll backend.
pub struct EpollUnitFuture<'a> {
    inner: EpollUnitInner,
    terminated: bool,
    _marker: PhantomData<&'a ()>,
}

impl<'a> EpollUnitFuture<'a> {
    /// Return `Ok(())` immediately.
    pub fn ready_ok() -> Self {
        Self {
            inner: EpollUnitInner::Ready(Ok(())),
            terminated: false,
            _marker: PhantomData,
        }
    }

    /// Return `Err(e)` immediately.
    pub fn ready_err(e: Errno) -> Self {
        Self {
            inner: EpollUnitInner::Ready(Err(e)),
            terminated: false,
            _marker: PhantomData,
        }
    }

    /// Wrap an already-armed `timerfd` OwnedFd for use as a sleep future.
    ///
    /// When the timer fires the fd becomes readable; we read from it to consume
    /// the expiry count and resolve with `Err(Errno::TIME)` (which
    /// `map_timeout_poll` in operations.rs converts to `Ok(())`).
    pub fn timer_fd(fd: OwnedFd) -> Self {
        Self {
            inner: EpollUnitInner::TimerFd {
                fd,
                registered: false,
            },
            terminated: false,
            _marker: PhantomData,
        }
    }

    /// Stub constructor – resolves with `ENOSYS`.  Used by operations that
    /// are not yet implemented for the epoll backend.
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        Self::ready_err(Errno::NOSYS)
    }

    /// Mark this future as cancelled.
    pub fn cancel(self: Pin<&mut Self>) {
        let me = self.get_mut();
        if let EpollUnitInner::TimerFd { fd, registered } = &mut me.inner
            && *registered
        {
            let raw = fd.as_raw_fd();
            let mut ts = TaskState::get();
            ts.epoll_driver.deregister(raw);
            *registered = false;
        }
        me.terminated = true;
    }
}

impl<'a> Future for EpollUnitFuture<'a> {
    type Output = Result<(), Errno>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.inner {
            EpollUnitInner::Ready(result) => {
                let r = *result;
                self.terminated = true;
                Poll::Ready(r)
            }
            EpollUnitInner::TimerFd { fd, registered } => {
                // Deregister before retrying to avoid stale waker.
                if *registered {
                    let raw = fd.as_raw_fd();
                    let mut ts = TaskState::get();
                    ts.epoll_driver.deregister(raw);
                    drop(ts);
                    *registered = false;
                }

                let raw = fd.as_raw_fd();
                let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
                let mut buf = [0u8; 8];
                match rustix::io::read(borrowed, &mut buf) {
                    Ok(_) => {
                        // Return TIME so map_timeout_poll converts it to Ok(()).
                        self.terminated = true;
                        Poll::Ready(Err(Errno::TIME))
                    }
                    Err(e) if e == Errno::AGAIN || e == Errno::WOULDBLOCK => {
                        let mut ts = TaskState::get();
                        ts.epoll_driver.register_read(raw, cx.waker().clone());
                        drop(ts);
                        *registered = true;
                        Poll::Pending
                    }
                    Err(e) => {
                        self.terminated = true;
                        Poll::Ready(Err(e))
                    }
                }
            }
        }
    }
}

impl<'a> FusedFuture for EpollUnitFuture<'a> {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl<'a> IsIoPoll for EpollUnitFuture<'a> {
    fn is_io_poll(&self) -> bool {
        false
    }
}

impl Drop for EpollUnitFuture<'_> {
    fn drop(&mut self) {
        if let EpollUnitInner::TimerFd { fd, registered } = &mut self.inner
            && *registered
        {
            let raw = fd.as_raw_fd();
            let mut ts = TaskState::get();
            ts.epoll_driver.deregister(raw);
            *registered = false;
            // OwnedFd::drop closes the timerfd after this returns.
        }
    }
}

// ---------------------------------------------------------------------------
// EpollUsizeFuture – Result<usize, Errno>
// ---------------------------------------------------------------------------

/// Which syscall to perform on retry.
enum IoOp {
    Read {
        ptr: *mut u8,
        len: usize,
    },
    Write {
        ptr: *const u8,
        len: usize,
    },
    Recv {
        ptr: *mut u8,
        len: usize,
        flags: RecvFlags,
    },
    Send {
        ptr: *const u8,
        len: usize,
        flags: SendFlags,
    },
}

// SAFETY: the pointers are derived from &'a [u8] / &'a mut [u8] which are
// valid for the lifetime 'a.  The future itself is !Send because *mut/*const
// are not Send.
unsafe impl Send for IoOp {}

/// Future that resolves to `Result<usize, Errno>`.
///
/// Used for read, write, recv, and send operations.
pub struct EpollUsizeFuture<'a> {
    fd: RawFd,
    op: IoOp,
    registered: bool,
    terminated: bool,
    _marker: PhantomData<&'a ()>,
}

impl<'a> EpollUsizeFuture<'a> {
    /// Stub – resolves immediately with `ENOSYS`.
    pub(crate) fn new() -> Self {
        Self {
            fd: -1,
            op: IoOp::Read {
                ptr: std::ptr::null_mut(),
                len: 0,
            },
            registered: false,
            terminated: false,
            _marker: PhantomData,
        }
    }

    pub fn read(fd: &impl AsFd, buf: &'a mut [u8]) -> Self {
        Self {
            fd: fd.as_fd().as_raw_fd(),
            op: IoOp::Read {
                ptr: buf.as_mut_ptr(),
                len: buf.len(),
            },
            registered: false,
            terminated: false,
            _marker: PhantomData,
        }
    }

    pub fn write(fd: &impl AsFd, buf: &'a [u8]) -> Self {
        Self {
            fd: fd.as_fd().as_raw_fd(),
            op: IoOp::Write {
                ptr: buf.as_ptr(),
                len: buf.len(),
            },
            registered: false,
            terminated: false,
            _marker: PhantomData,
        }
    }

    pub fn recv(fd: &impl AsFd, buf: &'a mut [u8], flags: RecvFlags) -> Self {
        Self {
            fd: fd.as_fd().as_raw_fd(),
            op: IoOp::Recv {
                ptr: buf.as_mut_ptr(),
                len: buf.len(),
                flags,
            },
            registered: false,
            terminated: false,
            _marker: PhantomData,
        }
    }

    pub fn send(fd: &impl AsFd, buf: &'a [u8], flags: SendFlags) -> Self {
        Self {
            fd: fd.as_fd().as_raw_fd(),
            op: IoOp::Send {
                ptr: buf.as_ptr(),
                len: buf.len(),
                flags,
            },
            registered: false,
            terminated: false,
            _marker: PhantomData,
        }
    }

    fn try_io(&self) -> Result<usize, Errno> {
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        match &self.op {
            IoOp::Read { ptr, len } => {
                let buf = unsafe { std::slice::from_raw_parts_mut(*ptr, *len) };
                rustix::io::read(borrowed, buf)
            }
            IoOp::Write { ptr, len } => {
                let buf = unsafe { std::slice::from_raw_parts(*ptr, *len) };
                rustix::io::write(borrowed, buf)
            }
            IoOp::Recv { ptr, len, flags } => {
                let buf = unsafe { std::slice::from_raw_parts_mut(*ptr, *len) };
                rustix::net::recv(borrowed, buf, *flags).map(|(n, _)| n)
            }
            IoOp::Send { ptr, len, flags } => {
                let buf = unsafe { std::slice::from_raw_parts(*ptr, *len) };
                rustix::net::send(borrowed, buf, *flags)
            }
        }
    }

    fn is_write_op(&self) -> bool {
        matches!(self.op, IoOp::Write { .. } | IoOp::Send { .. })
    }
}

impl<'a> Future for EpollUsizeFuture<'a> {
    type Output = Result<usize, Errno>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Stub path: fd == -1 means not a real future.
        if self.fd == -1 {
            self.terminated = true;
            return Poll::Ready(Err(Errno::NOSYS));
        }

        // Deregister before retrying so we don't hold a stale waker.
        if self.registered {
            let mut ts = TaskState::get();
            ts.epoll_driver.deregister(self.fd);
            drop(ts);
            self.registered = false;
        }

        match self.try_io() {
            Ok(n) => {
                self.terminated = true;
                Poll::Ready(Ok(n))
            }
            Err(e) if e == Errno::AGAIN || e == Errno::WOULDBLOCK => {
                let is_write = self.is_write_op();
                let fd = self.fd;
                let waker = cx.waker().clone();
                let mut ts = TaskState::get();
                if is_write {
                    ts.epoll_driver.register_write(fd, waker);
                } else {
                    ts.epoll_driver.register_read(fd, waker);
                }
                drop(ts);
                self.registered = true;
                Poll::Pending
            }
            Err(e) => {
                self.terminated = true;
                Poll::Ready(Err(e))
            }
        }
    }
}

impl<'a> FusedFuture for EpollUsizeFuture<'a> {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl<'a> IsIoPoll for EpollUsizeFuture<'a> {
    fn is_io_poll(&self) -> bool {
        false
    }
}

impl Drop for EpollUsizeFuture<'_> {
    fn drop(&mut self) {
        if self.registered {
            let mut ts = TaskState::get();
            ts.epoll_driver.deregister(self.fd);
        }
    }
}

// ---------------------------------------------------------------------------
// EpollOwnedFdFuture – Result<OwnedFd, Errno>  (accept)
// ---------------------------------------------------------------------------

/// Future that resolves to `Result<OwnedFd, Errno>`.
///
/// Used by `accept`.  The listening socket **must** be in non-blocking mode.
pub struct EpollOwnedFdFuture<'a> {
    fd: RawFd,
    registered: bool,
    terminated: bool,
    _marker: PhantomData<&'a ()>,
}

impl<'a> EpollOwnedFdFuture<'a> {
    /// Create a new accept future for `fd`.
    pub fn new(fd: &impl AsFd) -> Self {
        Self {
            fd: fd.as_fd().as_raw_fd(),
            registered: false,
            terminated: false,
            _marker: PhantomData,
        }
    }

    /// Stub – resolves immediately with `ENOSYS`.
    pub(crate) fn stub() -> Self {
        Self {
            fd: -1,
            registered: false,
            terminated: false,
            _marker: PhantomData,
        }
    }
}

impl<'a> Future for EpollOwnedFdFuture<'a> {
    type Output = Result<OwnedFd, Errno>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.fd == -1 {
            self.terminated = true;
            return Poll::Ready(Err(Errno::NOSYS));
        }

        if self.registered {
            let mut ts = TaskState::get();
            ts.epoll_driver.deregister(self.fd);
            drop(ts);
            self.registered = false;
        }

        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        match rustix::net::accept(borrowed) {
            Ok(new_fd) => {
                self.terminated = true;
                Poll::Ready(Ok(new_fd))
            }
            Err(e) if e == Errno::AGAIN || e == Errno::WOULDBLOCK => {
                let fd = self.fd;
                let waker = cx.waker().clone();
                let mut ts = TaskState::get();
                ts.epoll_driver.register_read(fd, waker);
                drop(ts);
                self.registered = true;
                Poll::Pending
            }
            Err(e) => {
                self.terminated = true;
                Poll::Ready(Err(e))
            }
        }
    }
}

impl<'a> FusedFuture for EpollOwnedFdFuture<'a> {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl<'a> IsIoPoll for EpollOwnedFdFuture<'a> {
    fn is_io_poll(&self) -> bool {
        false
    }
}

impl Drop for EpollOwnedFdFuture<'_> {
    fn drop(&mut self) {
        if self.registered {
            let mut ts = TaskState::get();
            ts.epoll_driver.deregister(self.fd);
        }
    }
}
