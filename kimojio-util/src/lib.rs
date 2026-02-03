// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Utility types for working with Kimojio async streams.
//!
//! This crate provides adapters and utilities that bridge Kimojio's async stream
//! traits with other async I/O ecosystems.
//!
//! # Stream Adaptor
//!
//! The [`stream_adapt`] module provides types to adapt Kimojio streams to
//! `futures_util::io` traits:
//!
//! - [`SyncStream`] - Synchronous wrapper with internal buffering
//! - [`AsyncStream`] - Implements `futures_util::io::AsyncRead`/`AsyncWrite`
//!
//! # Example
//!
//! ```no_run
//! use kimojio::OwnedFdStream;
//! use kimojio_util::AsyncStream;
//! use futures_util::io::{AsyncReadExt, AsyncWriteExt};
//!
//! async fn example(stream: OwnedFdStream) {
//!     let mut async_stream = AsyncStream::new(stream);
//!     async_stream.write_all(b"Hello").await.unwrap();
//!     async_stream.flush().await.unwrap();
//!
//!     let mut buf = [0u8; 1024];
//!     let n = async_stream.read(&mut buf).await.unwrap();
//! }
//! ```

pub mod stream_adapt;

pub use stream_adapt::{AsyncStream, SyncStream};
