// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Backend abstraction layer for the kimojio runtime.
//!
//! The runtime supports multiple I/O backends selected at compile time via
//! cargo features:
//!
//! | Feature        | Platform | Notes                                         |
//! |----------------|----------|-----------------------------------------------|
//! | `io_uring`     | Linux    | Default; requires kernel ≥ 5.15               |
//! | `epoll`        | Linux    | For containers that disable io_uring           |
//! | `windows-iocp` | Windows  | Skeleton; full implementation is future work   |
//!
//! Exactly one backend must be enabled at a time (enforced by `build.rs`).

#[cfg(feature = "io_uring")]
pub(crate) mod io_uring;

#[cfg(feature = "epoll")]
pub(crate) mod epoll;

#[cfg(feature = "windows-iocp")]
pub(crate) mod iocp;

// Re-export the backend-specific ring and associated types so the rest of the
// crate can import them from a single location without knowing which backend
// is active.
#[cfg(feature = "io_uring")]
pub(crate) use io_uring::{Cqe, IO_URING_SUBMISSION_ENTRIES, Ring, Sqe};
