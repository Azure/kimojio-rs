// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Windows I/O Completion Port (IOCP) backend skeleton.
//!
//! This module provides stub types that compile on Windows but are not yet
//! functionally implemented.  All methods panic with `todo!()` at runtime.
//! The skeleton exists so that the crate can be compiled (and cross-checked)
//! on Windows toolchains even before the full implementation is ready.

#![allow(dead_code)]

use std::os::windows::io::OwnedHandle;

/// A Windows I/O Completion Port.
pub(crate) struct IoCompletionPort {
    handle: OwnedHandle,
}

/// IOCP-based I/O driver for the kimojio runtime.
pub(crate) struct IocpDriver {
    port: IoCompletionPort,
}

impl IocpDriver {
    /// Create a new IOCP driver with the given maximum concurrency.
    pub(crate) fn new() -> Self {
        todo!("IocpDriver::new is not yet implemented")
    }

    /// Associate a handle with the completion port.
    pub(crate) fn associate(&mut self, _handle: &OwnedHandle) {
        todo!("IocpDriver::associate is not yet implemented")
    }

    /// Post a completion packet (used for wakeup / nop).
    pub(crate) fn post_nop(&self) {
        todo!("IocpDriver::post_nop is not yet implemented")
    }

    /// Dequeue completed I/O packets.  Returns the number of completions
    /// processed.  Blocks if `want > 0` and no completions are available.
    pub(crate) fn wait(&mut self, _want: usize) -> usize {
        todo!("IocpDriver::wait is not yet implemented")
    }

    /// Return a count of completions that are immediately available without
    /// blocking.
    pub(crate) fn get_completion_count(&mut self) -> usize {
        todo!("IocpDriver::get_completion_count is not yet implemented")
    }
}
