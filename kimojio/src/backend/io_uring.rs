// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! io_uring backend for the kimojio runtime.
//!
//! This module wraps `rustix_uring::IoUring` and exposes a thin driver layer
//! used by [`super::IoUringDriver`].  All types here are gated behind the
//! `io_uring` cargo feature.

use crate::{EAGAIN, EBUSY, EINTR};

#[cfg(feature = "io_uring_cmd")]
pub use rustix_uring::{cqueue::Entry32 as Cqe, squeue::Entry128 as Sqe};

#[cfg(not(feature = "io_uring_cmd"))]
pub use rustix_uring::{cqueue::Entry as Cqe, squeue::Entry as Sqe};

type IoUring = rustix_uring::IoUring<Sqe, Cqe>;

/// Maximum number of submission queue entries per ring.
pub const IO_URING_SUBMISSION_ENTRIES: usize = 128;

/// A thin wrapper around a single `io_uring` instance.
pub struct Ring(IoUring);

impl Ring {
    pub fn new(iopoll: bool) -> Self {
        let mut ring_builder = rustix_uring::IoUring::builder();
        if cfg!(feature = "setup_single_issuer") {
            ring_builder.setup_single_issuer();
        }
        if iopoll {
            ring_builder.setup_iopoll();
        }
        let entries = IO_URING_SUBMISSION_ENTRIES as u32;
        Self(ring_builder.build(entries).unwrap())
    }

    /// Submit queued SQEs and optionally wait for `want` CQEs.
    ///
    /// Returns the number of SQEs submitted.  `want` = 0 means non-blocking.
    pub fn submit_and_wait(&self, want: usize) -> usize {
        match self.0.submitter().submit_and_wait(want) {
            Ok(submitted) => submitted,
            Err(e) => match e.raw_os_error() {
                EAGAIN | EBUSY | EINTR => 0,
                _ => panic!("Error submitting or waiting for IO: {e:?}"),
            },
        }
    }

    /// Register an io_uring probe to check which opcodes the kernel supports.
    pub fn register_probe(&self) -> rustix_uring::Probe {
        let mut probe = rustix_uring::Probe::new();
        self.0.submitter().register_probe(&mut probe).unwrap();
        probe
    }

    /// Return the number of CQEs currently available without consuming them.
    pub fn get_completion_count(&mut self) -> usize {
        self.0.completion().len()
    }

    /// Pop the next CQE, or `None` if the completion queue is empty.
    #[inline(always)]
    pub fn get_next_cqe(&mut self) -> Option<Cqe> {
        self.0.completion().next()
    }

    /// Push SQEs into the submission queue, flushing first if it is full.
    #[inline(always)]
    pub fn submit(&mut self, entries: &[Sqe]) {
        // SAFETY: the pointers in entries must remain valid until the
        // corresponding CQE is processed.  Callers (RingFuture) uphold this
        // by keeping the backing memory alive via Rc<Completion>.
        let result = unsafe { self.0.submission().push_multiple(entries) };
        if result.is_err() {
            self.submit_and_wait(0);
            unsafe { self.0.submission().push_multiple(entries) }
                .expect("Cannot push SQE after flush");
        }
    }
}
