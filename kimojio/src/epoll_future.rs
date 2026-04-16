// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Minimal future types for the epoll backend.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::FusedFuture;
use rustix::fd::OwnedFd;
use rustix::io::Errno;

use crate::operations::IsIoPoll;

/// Future returning `Result<(), Errno>` — epoll backend version.
pub struct EpollUnitFuture<'a> {
    pub(crate) terminated: bool,
    pub(crate) _marker: PhantomData<&'a ()>,
}

impl<'a> EpollUnitFuture<'a> {
    pub(crate) fn new() -> Self {
        Self {
            terminated: false,
            _marker: PhantomData,
        }
    }

    /// Cancel this future (no-op for the stub).
    pub fn cancel(self: Pin<&mut Self>) {
        self.get_mut().terminated = true;
    }
}

impl<'a> Future for EpollUnitFuture<'a> {
    type Output = Result<(), Errno>;
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.terminated = true;
        Poll::Ready(Err(Errno::NOSYS))
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

/// Future returning `Result<usize, Errno>` — epoll backend version.
pub struct EpollUsizeFuture<'a> {
    pub(crate) terminated: bool,
    pub(crate) _marker: PhantomData<&'a ()>,
}

impl<'a> EpollUsizeFuture<'a> {
    pub(crate) fn new() -> Self {
        Self {
            terminated: false,
            _marker: PhantomData,
        }
    }
}

impl<'a> Future for EpollUsizeFuture<'a> {
    type Output = Result<usize, Errno>;
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.terminated = true;
        Poll::Ready(Err(Errno::NOSYS))
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

/// Future returning `Result<OwnedFd, Errno>` — epoll backend version.
pub struct EpollOwnedFdFuture<'a> {
    pub(crate) terminated: bool,
    pub(crate) _marker: PhantomData<&'a ()>,
}

impl<'a> EpollOwnedFdFuture<'a> {
    pub(crate) fn new() -> Self {
        Self {
            terminated: false,
            _marker: PhantomData,
        }
    }
}

impl<'a> Future for EpollOwnedFdFuture<'a> {
    type Output = Result<OwnedFd, Errno>;
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.terminated = true;
        Poll::Ready(Err(Errno::NOSYS))
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
