// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stream adaptor that bridges Kimojio's async stream traits with futures_util I/O traits.
//!
//! This module provides two main types:
//! - [`SyncStream`] - Wraps a Kimojio stream with buffers and provides sync `std::io::Read`/`Write`
//! - [`AsyncStream`] - Wraps `SyncStream` to implement `futures_util::io::AsyncRead`/`AsyncWrite`
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

use futures_util::io::{AsyncRead as FutAsyncRead, AsyncWrite as FutAsyncWrite};
use kimojio::{AsyncStreamRead, AsyncStreamWrite};
use pin_project_lite::pin_project;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

type PinBoxFuture<T> = Pin<Box<dyn std::future::Future<Output = T>>>;

/// Default buffer capacity for stream operations.
const DEFAULT_BUFFER_CAPACITY: usize = 8192;

fn would_block(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::WouldBlock, msg)
}

/// Wraps a Kimojio async stream into a sync stream with internal buffering.
///
/// The sync methods (`std::io::Read`, `std::io::Write`) will return
/// [`std::io::ErrorKind::WouldBlock`] when the internal buffers need
/// async operations to fill or flush.
///
/// Use [`fill_read_buf`](Self::fill_read_buf) and [`flush_write_buf`](Self::flush_write_buf)
/// to perform the async buffer operations.
pub struct SyncStream<S> {
    inner: S,
    read_buffer: VecDeque<u8>,
    write_buffer: Vec<u8>,
    eof: bool,
}

impl<S> SyncStream<S> {
    /// Create a new `SyncStream` with default buffer capacity.
    pub fn new(stream: S) -> Self {
        Self::with_capacity(DEFAULT_BUFFER_CAPACITY, stream)
    }

    /// Create a new `SyncStream` with specified buffer capacity.
    pub fn with_capacity(capacity: usize, stream: S) -> Self {
        Self {
            inner: stream,
            read_buffer: VecDeque::with_capacity(capacity),
            write_buffer: Vec::with_capacity(capacity),
            eof: false,
        }
    }

    /// Returns `true` if the stream has reached EOF.
    pub fn is_eof(&self) -> bool {
        self.eof
    }

    /// Get a reference to the inner stream.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// Get a mutable reference to the inner stream.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    /// Consume this wrapper and return the inner stream.
    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: AsyncStreamRead> SyncStream<S> {
    /// Fill the read buffer by reading from the async stream.
    ///
    /// Returns the number of bytes read, or 0 if EOF is reached.
    pub async fn fill_read_buf(&mut self) -> std::io::Result<usize> {
        if self.eof {
            return Ok(0);
        }

        // Create a buffer to read into
        let mut read_buf = vec![0u8; DEFAULT_BUFFER_CAPACITY];

        // Read from the async stream (no deadline)
        let bytes_read = self
            .inner
            .try_read(&mut read_buf, None)
            .await
            .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;

        if bytes_read == 0 {
            self.eof = true;
            return Ok(0);
        }

        // Add the data to our read buffer
        for &byte in &read_buf[..bytes_read] {
            self.read_buffer.push_back(byte);
        }

        Ok(bytes_read)
    }
}

impl<S: AsyncStreamWrite> SyncStream<S> {
    /// Flush the write buffer to the async stream.
    ///
    /// Returns the number of bytes written.
    pub async fn flush_write_buf(&mut self) -> std::io::Result<usize> {
        if self.write_buffer.is_empty() {
            return Ok(0);
        }

        // Take the data from the write buffer, preserving capacity
        let capacity = self.write_buffer.capacity();
        let data = std::mem::take(&mut self.write_buffer);
        let len = data.len();

        // Restore the capacity of the write buffer
        self.write_buffer.reserve_exact(capacity);

        // Write to the async stream (no deadline)
        self.inner
            .write(&data, None)
            .await
            .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;

        Ok(len)
    }

    /// Shutdown the stream - flush pending writes and close the connection.
    pub async fn shutdown(&mut self) -> std::io::Result<()> {
        // First flush any pending write data
        self.flush_write_buf().await?;
        // Then shutdown the connection
        self.inner
            .shutdown()
            .await
            .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))
    }
}

impl<S> std::io::Read for SyncStream<S>
where
    S: AsyncStreamRead + AsyncStreamWrite,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // If we have buffered data, use it first
        if !self.read_buffer.is_empty() {
            let to_copy = buf.len().min(self.read_buffer.len());
            for item in buf.iter_mut().take(to_copy) {
                *item = self.read_buffer.pop_front().unwrap();
            }
            return Ok(to_copy);
        }

        // If we've reached EOF and have no buffered data, return 0
        if self.eof {
            return Ok(0);
        }

        // No buffered data and not EOF - need to fill buffer
        Err(would_block("need to fill the read buffer"))
    }
}

impl<S> std::io::Write for SyncStream<S>
where
    S: AsyncStreamRead + AsyncStreamWrite,
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Check if we need to flush existing buffer first
        if self.write_buffer.len() + buf.len() > self.write_buffer.capacity()
            && !self.write_buffer.is_empty()
        {
            return Err(would_block("need to flush the write buffer"));
        }

        // Buffer the data
        let to_write = buf
            .len()
            .min(self.write_buffer.capacity() - self.write_buffer.len());
        self.write_buffer.extend_from_slice(&buf[..to_write]);
        Ok(to_write)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.write_buffer.is_empty() {
            Err(would_block("need to flush the write buffer"))
        } else {
            Ok(())
        }
    }
}

pin_project! {
    /// Async stream adaptor for `futures_util::io::AsyncRead` and `AsyncWrite`.
    ///
    /// This wraps a [`SyncStream`] and provides implementations of the futures-util
    /// async I/O traits, handling the async buffer operations internally.
    pub struct AsyncStream<S> {
        #[pin]
        inner: SyncStream<S>,
        read_future: Option<PinBoxFuture<std::io::Result<usize>>>,
        write_future: Option<PinBoxFuture<std::io::Result<usize>>>,
        shutdown_future: Option<PinBoxFuture<std::io::Result<()>>>,
    }
}

impl<S> AsyncStream<S> {
    /// Create a new `AsyncStream` with default buffer capacity.
    pub fn new(stream: S) -> Self {
        Self::new_impl(SyncStream::new(stream))
    }

    /// Create a new `AsyncStream` with specified buffer capacity.
    pub fn with_capacity(capacity: usize, stream: S) -> Self {
        Self::new_impl(SyncStream::with_capacity(capacity, stream))
    }

    fn new_impl(inner: SyncStream<S>) -> Self {
        Self {
            inner,
            read_future: None,
            write_future: None,
            shutdown_future: None,
        }
    }

    /// Get a reference to the inner stream.
    pub fn get_ref(&self) -> &S {
        self.inner.get_ref()
    }

    /// Get a mutable reference to the inner stream.
    pub fn get_mut(&mut self) -> &mut S {
        self.inner.get_mut()
    }
}

impl<S: AsyncStreamRead + AsyncStreamWrite + 'static> FutAsyncRead for AsyncStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.project();

        // Safety: We're extending the lifetime of the SyncStream reference.
        // This is safe because:
        // 1. The boxed future is stored alongside the SyncStream in the same struct
        // 2. We control the lifetime and ensure the stream isn't moved
        // 3. The future is dropped before or with the struct
        let inner: &'static mut SyncStream<S> =
            unsafe { &mut *(this.inner.get_unchecked_mut() as *mut _) };

        // Check if we have an ongoing read future
        if let Some(mut f) = this.read_future.take()
            && f.as_mut().poll(cx).is_pending()
        {
            // Future is still pending, put it back and return
            this.read_future.replace(f);
            return Poll::Pending;
        }
        // Future completed, continue to try sync read

        // Try the sync read operation
        match std::io::Read::read(inner, buf) {
            Ok(len) => Poll::Ready(Ok(len)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Need to fill the read buffer asynchronously
                this.read_future.replace(Box::pin(inner.fill_read_buf()));
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl<S: AsyncStreamRead + AsyncStreamWrite + 'static> FutAsyncWrite for AsyncStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.project();

        let inner: &'static mut SyncStream<S> =
            unsafe { &mut *(this.inner.get_unchecked_mut() as *mut _) };

        // Check if we have an ongoing write future
        if let Some(mut f) = this.write_future.take()
            && f.as_mut().poll(cx).is_pending()
        {
            // Future is still pending, put it back and return
            this.write_future.replace(f);
            return Poll::Pending;
        }
        // Future completed, continue to try sync write

        // Try the sync write operation
        match std::io::Write::write(inner, buf) {
            Ok(len) => Poll::Ready(Ok(len)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Need to flush the write buffer asynchronously
                this.write_future.replace(Box::pin(inner.flush_write_buf()));
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.project();

        // If shutdown is in progress, we can't flush anymore since shutdown
        // includes flushing. Just report that flush is complete.
        if this.shutdown_future.is_some() {
            return Poll::Ready(Ok(()));
        }

        let inner: &'static mut SyncStream<S> =
            unsafe { &mut *(this.inner.get_unchecked_mut() as *mut _) };

        // Check if we have an ongoing write operation
        let res = if let Some(mut f) = this.write_future.take() {
            // Continue polling the existing write future
            match f.as_mut().poll(cx) {
                Poll::Pending => {
                    // Still in progress, put the future back and return
                    this.write_future.replace(f);
                    return Poll::Pending;
                }
                Poll::Ready(res) => res, // Write completed
            }
        } else {
            // Try the sync flush operation first
            match std::io::Write::flush(inner) {
                Ok(()) => return Poll::Ready(Ok(())), // Nothing to flush
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Need to flush the write buffer asynchronously
                    this.write_future.replace(Box::pin(inner.flush_write_buf()));
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Err(e) => return Poll::Ready(Err(e)), // Other error
            }
        };

        // Convert the flush result (usize) to () for the flush operation
        Poll::Ready(res.map(|_| ()))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // First ensure any pending writes are flushed
        let flush_result = self.as_mut().poll_flush(cx);
        match flush_result {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {
                // Flush completed, proceed with shutdown
            }
        }

        let this = self.project();
        let inner: &'static mut SyncStream<S> =
            unsafe { &mut *(this.inner.get_unchecked_mut() as *mut _) };

        // Check if we have an ongoing shutdown operation
        if let Some(mut f) = this.shutdown_future.take() {
            match f.as_mut().poll(cx) {
                Poll::Pending => {
                    this.shutdown_future.replace(f);
                    return Poll::Pending;
                }
                Poll::Ready(res) => return Poll::Ready(res),
            }
        }

        // Start shutdown operation
        this.shutdown_future.replace(Box::pin(inner.shutdown()));
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::io::{AsyncReadExt, AsyncWriteExt};
    use kimojio::BufferPipe;

    #[kimojio::test]
    async fn test_sync_stream_basic_read_write() {
        let (client, server) = BufferPipe::new();

        let mut client_sync = SyncStream::new(client);
        let mut server_sync = SyncStream::new(server);

        // Write some data
        let written = std::io::Write::write(&mut client_sync, b"hello").unwrap();
        assert_eq!(written, 5);

        // Flush the write buffer
        client_sync.flush_write_buf().await.unwrap();

        // Fill server's read buffer
        server_sync.fill_read_buf().await.unwrap();

        // Read the data
        let mut buf = [0u8; 10];
        let read = std::io::Read::read(&mut server_sync, &mut buf).unwrap();
        assert_eq!(read, 5);
        assert_eq!(&buf[..5], b"hello");
    }

    #[kimojio::test]
    async fn test_sync_stream_would_block_on_empty_read() {
        let (client, _server) = BufferPipe::new();
        let mut sync_stream = SyncStream::new(client);

        // Try to read from empty stream - should get WouldBlock
        let mut buf = [0u8; 10];
        let result = std::io::Read::read(&mut sync_stream, &mut buf);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[kimojio::test]
    async fn test_async_stream_read_write() {
        let (client, server) = BufferPipe::new();

        let mut client_async = AsyncStream::new(client);
        let mut server_async = AsyncStream::new(server);

        // Write data from client
        client_async.write_all(b"async hello").await.unwrap();
        client_async.flush().await.unwrap();

        // Read data on server
        let mut buf = [0u8; 11];
        server_async.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"async hello");
    }

    #[kimojio::test]
    async fn test_async_stream_partial_read() {
        let (client, server) = BufferPipe::new();

        let mut client_async = AsyncStream::new(client);
        let mut server_async = AsyncStream::new(server);

        // Write data
        client_async.write_all(b"hello world").await.unwrap();
        client_async.flush().await.unwrap();

        // Read in parts
        let mut buf1 = [0u8; 5];
        server_async.read_exact(&mut buf1).await.unwrap();
        assert_eq!(&buf1, b"hello");

        let mut buf2 = [0u8; 6];
        server_async.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b" world");
    }

    #[kimojio::test]
    async fn test_async_stream_large_data() {
        use kimojio::operations::spawn_task;

        let (client, server) = BufferPipe::new();

        let mut client_async = AsyncStream::new(client);
        let mut server_async = AsyncStream::new(server);

        // Large data that exceeds BufferPipe's internal buffer (1024 bytes)
        let large_data: Vec<u8> = (0..255u8).cycle().take(4000).collect();
        let expected_data = large_data.clone();

        // Need concurrent read/write since BufferPipe has limited buffer
        let write_task = spawn_task(async move {
            client_async.write_all(&large_data).await.unwrap();
            client_async.flush().await.unwrap();
        });

        // Read it back
        let mut read_buf = vec![0u8; 4000];
        server_async.read_exact(&mut read_buf).await.unwrap();

        write_task.await.unwrap();
        assert_eq!(read_buf, expected_data);
    }

    #[kimojio::test]
    async fn test_sync_stream_eof() {
        let (client, server) = BufferPipe::new();

        let mut client_sync = SyncStream::new(client);
        let mut server_sync = SyncStream::new(server);

        // Write and close
        std::io::Write::write(&mut client_sync, b"final").unwrap();
        client_sync.flush_write_buf().await.unwrap();
        client_sync.shutdown().await.unwrap();

        // Read on server
        server_sync.fill_read_buf().await.unwrap();
        let mut buf = [0u8; 10];
        let read = std::io::Read::read(&mut server_sync, &mut buf).unwrap();
        assert_eq!(read, 5);
        assert_eq!(&buf[..5], b"final");
    }

    #[kimojio::test]
    async fn test_async_stream_close() {
        let (client, server) = BufferPipe::new();

        let mut client_async = AsyncStream::new(client);
        let mut server_async = AsyncStream::new(server);

        // Write data and close
        client_async.write_all(b"goodbye").await.unwrap();
        client_async.close().await.unwrap();

        // Read data on server
        let mut buf = [0u8; 7];
        server_async.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"goodbye");
    }
}
