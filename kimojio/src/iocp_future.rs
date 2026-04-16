// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Future types for the Windows IOCP backend.
//!
//! Sockets **must** be in non-blocking mode (`FIONBIO`).  On `EAGAIN` /
//! `EWOULDBLOCK` the future registers the fd with the per-thread
//! [`SelectDriver`] and returns `Poll::Pending`; the driver wakes the task
//! once the socket is ready and the future retries the I/O.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::FusedFuture;
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use rustix::io::Errno;
use rustix::net::{RecvFlags, SendFlags};

use crate::operations::IsIoPoll;
use crate::task::TaskState;

// ─────────────────────────────────────────────────────────────────────────────
// IocpUnitFuture  –  Result<(), Errno>
// ─────────────────────────────────────────────────────────────────────────────

enum UnitInner {
    Ready(Result<(), Errno>),
    Sleep(std::time::Duration),
}

/// A simple future that resolves to `Result<(), Errno>`.
///
/// Used for `close`, `nop`, `shutdown`, and `sleep` operations on the
/// IOCP backend.
pub struct IocpUnitFuture<'a> {
    inner: UnitInner,
    terminated: bool,
    _marker: PhantomData<&'a ()>,
}

impl<'a> IocpUnitFuture<'a> {
    /// Create a future that resolves immediately to `Ok(())`.
    pub fn ready_ok() -> Self {
        Self {
            inner: UnitInner::Ready(Ok(())),
            terminated: false,
            _marker: PhantomData,
        }
    }

    /// Create a future that resolves immediately to `Err(e)`.
    pub fn ready_err(e: Errno) -> Self {
        Self {
            inner: UnitInner::Ready(Err(e)),
            terminated: false,
            _marker: PhantomData,
        }
    }

    /// Unimplemented stub — resolves to `Err(EOPNOTSUPP)`.
    pub fn new() -> Self {
        Self::ready_err(Errno::OPNOTSUPP)
    }

    /// Perform a synchronous shutdown and wrap the result.
    pub fn shutdown(fd: &impl AsFd, how: rustix::net::Shutdown) -> Self {
        let result = rustix::net::shutdown(fd.as_fd(), how).map_err(Into::into);
        Self {
            inner: UnitInner::Ready(result),
            terminated: false,
            _marker: PhantomData,
        }
    }

    /// Create a sleep future that blocks for `duration` on first poll.
    pub fn sleep(duration: std::time::Duration) -> Self {
        Self {
            inner: UnitInner::Sleep(duration),
            terminated: false,
            _marker: PhantomData,
        }
    }

    /// Cancel this future (marks it terminated without blocking).
    pub fn cancel(self: Pin<&mut Self>) {
        let me = self.get_mut();
        me.inner = UnitInner::Ready(Err(Errno::CANCELED));
        me.terminated = true;
    }

    /// Not supported on the IOCP backend — always panics.
    ///
    /// On Linux the epoll backend uses timerfd for sleeps; Windows has no
    /// equivalent timerfd and this path should never be reached.
    pub fn timer_fd(_fd: OwnedFd) -> Self {
        panic!("timer_fd is not supported on the IOCP backend")
    }
}

impl Default for IocpUnitFuture<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl Future for IocpUnitFuture<'_> {
    type Output = Result<(), Errno>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.terminated = true;
        match &self.inner {
            UnitInner::Ready(r) => Poll::Ready(*r),
            UnitInner::Sleep(d) => {
                let d = *d;
                std::thread::sleep(d);
                // Return Ok(()) directly; SleepFuture bypasses map_timeout_poll on iocp_backend.
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl FusedFuture for IocpUnitFuture<'_> {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl IsIoPoll for IocpUnitFuture<'_> {
    fn is_io_poll(&self) -> bool {
        false
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IocpUsizeFuture  –  Result<usize, Errno>
// ─────────────────────────────────────────────────────────────────────────────

/// The raw I/O operation stored inside an [`IocpUsizeFuture`].
#[allow(clippy::enum_variant_names)]
enum UsizeOp {
    Recv {
        fd: RawFd,
        ptr: *mut u8,
        len: usize,
        flags: RecvFlags,
    },
    Send {
        fd: RawFd,
        ptr: *const u8,
        len: usize,
        flags: SendFlags,
    },
    Ready(Result<usize, Errno>),
}

// SAFETY: the raw pointers come from borrowed slices that outlive the future
// (lifetime 'a), and the futures are only used on a single thread.
unsafe impl Send for UsizeOp {}
unsafe impl Sync for UsizeOp {}

/// A future that performs a non-blocking socket recv or send.
///
/// On `EAGAIN`/`EWOULDBLOCK` the fd is registered with the [`SelectDriver`]
/// and `Poll::Pending` is returned.  The waker fires once the socket is
/// ready and the operation is retried.
pub struct IocpUsizeFuture<'a> {
    op: UsizeOp,
    registered: bool,
    terminated: bool,
    _marker: PhantomData<&'a ()>,
}

impl<'a> IocpUsizeFuture<'a> {
    fn new_op(op: UsizeOp) -> Self {
        Self {
            op,
            registered: false,
            terminated: false,
            _marker: PhantomData,
        }
    }

    /// Stub that resolves immediately to `Err(EOPNOTSUPP)`.
    pub fn new() -> Self {
        Self::new_op(UsizeOp::Ready(Err(Errno::OPNOTSUPP)))
    }

    /// Non-blocking `recv` future.
    pub fn recv(fd: &'a impl AsFd, buf: &'a mut [u8], flags: RecvFlags) -> Self {
        let raw = fd.as_fd().as_raw_fd();
        Self::new_op(UsizeOp::Recv {
            fd: raw,
            ptr: buf.as_mut_ptr(),
            len: buf.len(),
            flags,
        })
    }

    /// Non-blocking `send` future.
    pub fn send(fd: &'a impl AsFd, buf: &'a [u8], flags: SendFlags) -> Self {
        let raw = fd.as_fd().as_raw_fd();
        Self::new_op(UsizeOp::Send {
            fd: raw,
            ptr: buf.as_ptr(),
            len: buf.len(),
            flags,
        })
    }

    /// Alias for `recv` (used by `operations::read`).
    pub fn read(fd: &'a impl AsFd, buf: &'a mut [u8]) -> Self {
        Self::recv(fd, buf, RecvFlags::empty())
    }

    /// Alias for `send` (used by `operations::write`).
    pub fn write(fd: &'a impl AsFd, buf: &'a [u8]) -> Self {
        Self::send(fd, buf, SendFlags::empty())
    }

    fn try_io(&self) -> Result<usize, Errno> {
        match &self.op {
            UsizeOp::Recv {
                fd,
                ptr,
                len,
                flags,
                ..
            } => {
                let borrowed = unsafe { BorrowedFd::borrow_raw(*fd) };
                let buf = unsafe { std::slice::from_raw_parts_mut(*ptr, *len) };
                rustix::net::recv(borrowed, buf, *flags).map(|(n, _)| n)
            }
            UsizeOp::Send {
                fd,
                ptr,
                len,
                flags,
                ..
            } => {
                let borrowed = unsafe { BorrowedFd::borrow_raw(*fd) };
                let buf = unsafe { std::slice::from_raw_parts(*ptr, *len) };
                rustix::net::send(borrowed, buf, *flags)
            }
            UsizeOp::Ready(r) => *r,
        }
    }

    fn raw_fd(&self) -> Option<RawFd> {
        match &self.op {
            UsizeOp::Recv { fd, .. } | UsizeOp::Send { fd, .. } => Some(*fd),
            UsizeOp::Ready(_) => None,
        }
    }

    fn is_write(&self) -> bool {
        matches!(&self.op, UsizeOp::Send { .. })
    }
}

impl Default for IocpUsizeFuture<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl Future for IocpUsizeFuture<'_> {
    type Output = Result<usize, Errno>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let UsizeOp::Ready(r) = self.op {
            self.terminated = true;
            return Poll::Ready(r);
        }

        // Deregister from the driver if we were previously registered.
        if self.registered {
            if let Some(fd) = self.raw_fd() {
                let mut ts = TaskState::get();
                ts.select_driver.deregister(fd);
                drop(ts);
            }
            self.registered = false;
        }

        match self.try_io() {
            Ok(n) => {
                self.terminated = true;
                Poll::Ready(Ok(n))
            }
            Err(e) if e == Errno::AGAIN || e == Errno::WOULDBLOCK => {
                if let Some(fd) = self.raw_fd() {
                    let waker = cx.waker().clone();
                    let mut ts = TaskState::get();
                    if self.is_write() {
                        ts.select_driver.register_write(fd, waker);
                    } else {
                        ts.select_driver.register_read(fd, waker);
                    }
                    drop(ts);
                }
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

impl FusedFuture for IocpUsizeFuture<'_> {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl IsIoPoll for IocpUsizeFuture<'_> {
    fn is_io_poll(&self) -> bool {
        false
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IocpOwnedFdFuture  –  Result<OwnedFd, Errno>
// ─────────────────────────────────────────────────────────────────────────────

/// A future that performs a non-blocking `accept` and resolves to the new fd.
pub struct IocpOwnedFdFuture<'a> {
    fd: RawFd,
    registered: bool,
    terminated: bool,
    _marker: PhantomData<&'a ()>,
}

impl<'a> IocpOwnedFdFuture<'a> {
    pub fn new(fd: &'a impl AsFd) -> Self {
        Self {
            fd: fd.as_fd().as_raw_fd(),
            registered: false,
            terminated: false,
            _marker: PhantomData,
        }
    }
}

impl Future for IocpOwnedFdFuture<'_> {
    type Output = Result<OwnedFd, Errno>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Deregister from the driver if we were previously registered.
        if self.registered {
            let mut ts = TaskState::get();
            ts.select_driver.deregister(self.fd);
            drop(ts);
            self.registered = false;
        }

        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        match rustix::net::accept(borrowed) {
            Ok(new_fd) => {
                self.terminated = true;
                // Set the accepted socket to non-blocking as well.
                set_nonblocking_raw(new_fd.as_fd().as_raw_fd());
                Poll::Ready(Ok(new_fd))
            }
            Err(e) if e == Errno::AGAIN || e == Errno::WOULDBLOCK => {
                let waker = cx.waker().clone();
                let mut ts = TaskState::get();
                ts.select_driver.register_read(self.fd, waker);
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

impl FusedFuture for IocpOwnedFdFuture<'_> {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl IsIoPoll for IocpOwnedFdFuture<'_> {
    fn is_io_poll(&self) -> bool {
        false
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper: set a socket to non-blocking mode via ioctlsocket(FIONBIO)
// ─────────────────────────────────────────────────────────────────────────────

const FIONBIO: u32 = 0x8004_667E;

/// Set `fd` to non-blocking mode via `ioctlsocket(FIONBIO, 1)`.
pub(crate) fn set_nonblocking_raw(fd: RawFd) {
    use windows_sys::Win32::Networking::WinSock::{SOCKET, ioctlsocket};
    let mut mode: u32 = 1;
    // SAFETY: fd is a valid Windows SOCKET handle; ioctlsocket is thread-safe.
    unsafe {
        ioctlsocket(fd as SOCKET, FIONBIO as i32, &mut mode);
    }
}
