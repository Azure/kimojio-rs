// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! AsyncStream is a trait that represents the ability to read
//! and write to a stream. It is useful for creating generic
//! I/O code that is agnostic to the underlying transport.

use std::{future::Future, io::IoSlice, time::Instant};

use crate::{Errno, OwnedFd, operations, try_clone_owned_fd};

/// A trait for asynchronous reading from a stream.
///
/// Implementors provide methods to read data asynchronously with optional deadlines.
pub trait AsyncStreamRead {
    /// Attempts to read data into the buffer, returning the number of bytes read.
    ///
    /// May return fewer bytes than requested. Returns 0 at end of stream.
    fn try_read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<usize, Errno>> + 'a;

    /// Reads exactly enough bytes to fill the buffer.
    ///
    /// Repeatedly reads until the buffer is full or an error occurs.
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<(), Errno>> + 'a;
}

/// A trait for asynchronous writing to a stream.
///
/// Implementors provide methods to write data asynchronously with optional deadlines.
pub trait AsyncStreamWrite {
    /// Writes all bytes from the buffer to the stream.
    fn write<'a>(
        &'a mut self,
        buffer: &'a [u8],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<(), Errno>> + 'a;

    /// Shuts down the write side of the stream.
    fn shutdown(&mut self) -> impl Future<Output = Result<(), Errno>>;

    /// Closes the stream entirely.
    fn close(&mut self) -> impl Future<Output = Result<(), Errno>>;

    /// Writes all data from multiple buffers to the stream.
    fn writev<'a>(
        &'a mut self,
        buffers: &'a mut [IoSlice<'a>],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<(), Errno>> + 'a {
        async move {
            for buffer in buffers {
                self.write(buffer, deadline).await?;
            }
            Ok(())
        }
    }
}

/// A trait for streams that can be split into separate read and write halves.
pub trait SplittableStream {
    /// The type of the read half after splitting.
    type ReadStream: AsyncStreamRead;
    /// The type of the write half after splitting.
    type WriteStream: AsyncStreamWrite;

    /// Splits the stream into independent read and write halves.
    async fn split(self) -> Result<(Self::ReadStream, Self::WriteStream), Errno>;
}

struct OwnedFdReadState {
    read_used: usize,
    read_available: usize,
    read_buffer: [u8; 16384],
}

/// The read half of an `OwnedFdStream` after splitting.
pub struct OwnedFdStreamRead {
    fd: Option<OwnedFd>,
    read_state: OwnedFdReadState,
}

impl OwnedFdStreamRead {
    /// This exists for use by tests
    pub async fn close(&self) -> Result<(), Errno> {
        if let Some(fd) = &self.fd {
            operations::close(fd.try_clone().unwrap()).await?;
        }
        Ok(())
    }
}

/// The write half of an `OwnedFdStream` after splitting.
pub struct OwnedFdStreamWrite {
    fd: Option<OwnedFd>,
}

/// A stream wrapper around an owned file descriptor.
///
/// Provides buffered reading and direct writing with async support.
pub struct OwnedFdStream {
    fd: Option<OwnedFd>,
    read_state: OwnedFdReadState,
}

impl OwnedFdStream {
    pub fn new(fd: OwnedFd) -> Self {
        let fd = Some(fd);
        Self {
            fd,
            read_state: OwnedFdReadState {
                read_used: 0,
                read_available: 0,
                read_buffer: [0; 16384],
            },
        }
    }

    /// Receive a file descriptor from the stream.
    pub fn into_inner(self) -> Option<OwnedFd> {
        self.fd
    }
}

impl SplittableStream for OwnedFdStream {
    type ReadStream = OwnedFdStreamRead;
    type WriteStream = OwnedFdStreamWrite;

    async fn split(self) -> Result<(OwnedFdStreamRead, OwnedFdStreamWrite), Errno> {
        let (read_fd, write_fd) = if let Some(fd) = self.fd {
            (Some(try_clone_owned_fd(&fd)?), Some(fd))
        } else {
            (None, None)
        };
        Ok((
            OwnedFdStreamRead {
                fd: read_fd,
                read_state: self.read_state,
            },
            OwnedFdStreamWrite { fd: write_fd },
        ))
    }
}

async fn try_read_impl(
    fd: &mut Option<OwnedFd>,
    buffer: &mut [u8],
    read_state: &mut OwnedFdReadState,
    deadline: Option<Instant>,
) -> Result<usize, Errno> {
    if let Some(fd) = fd {
        if read_state.read_available == 0 {
            let amount =
                operations::read_with_deadline(fd, &mut read_state.read_buffer, deadline).await?;
            if amount == 0 {
                return Ok(0);
            }

            read_state.read_used = 0;
            read_state.read_available = amount;
        }

        let read_used = read_state.read_used;
        let tocopy = std::cmp::min(buffer.len(), read_state.read_available);
        read_state.read_used += tocopy;
        read_state.read_available -= tocopy;

        buffer[0..tocopy].copy_from_slice(&read_state.read_buffer[read_used..read_used + tocopy]);
        Ok(tocopy)
    } else {
        Err(Errno::from_raw_os_error(crate::EPIPE))
    }
}

async fn read_impl(
    fd: &mut Option<OwnedFd>,
    mut buffer: &mut [u8],
    read_state: &mut OwnedFdReadState,
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    while !buffer.is_empty() {
        let amount = try_read_impl(fd, buffer, read_state, deadline).await?;
        if amount == 0 {
            return Err(Errno::from_raw_os_error(crate::EPIPE));
        }
        buffer = &mut buffer[amount..];
    }
    Ok(())
}

impl AsyncStreamRead for OwnedFdStreamRead {
    fn try_read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<usize, Errno>> + 'a {
        try_read_impl(&mut self.fd, buffer, &mut self.read_state, deadline)
    }

    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<(), Errno>> + 'a {
        read_impl(&mut self.fd, buffer, &mut self.read_state, deadline)
    }
}

impl AsyncStreamRead for OwnedFdStream {
    fn try_read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<usize, Errno>> + 'a {
        try_read_impl(&mut self.fd, buffer, &mut self.read_state, deadline)
    }

    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<(), Errno>> + 'a {
        read_impl(&mut self.fd, buffer, &mut self.read_state, deadline)
    }
}

async fn write_impl(
    fd: &mut Option<OwnedFd>,
    mut buffer: &[u8],
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    if let Some(fd) = fd {
        while !buffer.is_empty() {
            let amount = operations::write_with_deadline(&fd, buffer, deadline).await?;
            if amount == 0 {
                return Err(Errno::from_raw_os_error(crate::EPIPE));
            }
            buffer = &buffer[amount..];
        }
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(crate::EPIPE))
    }
}

async fn writev_impl(
    fd: &mut Option<OwnedFd>,
    mut buffers: &mut [IoSlice<'_>],
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    if let Some(fd) = fd {
        while !buffers.is_empty() {
            let result = operations::writev_with_deadline(&fd, buffers, None, deadline).await?;
            if result == 0 {
                return Err(Errno::from_raw_os_error(crate::EPIPE));
            }
            IoSlice::advance_slices(&mut buffers, result);
        }

        Ok(())
    } else {
        Err(Errno::from_raw_os_error(crate::EPIPE))
    }
}

async fn shutdown_impl(fd: &mut Option<OwnedFd>) -> Result<(), Errno> {
    if let Some(fd) = fd {
        operations::shutdown(fd, libc::SHUT_RDWR).await?;
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(crate::EPIPE))
    }
}

async fn close_impl(fd: &mut Option<OwnedFd>) -> Result<(), Errno> {
    if let Some(fd) = fd.take() {
        operations::close(fd).await?;
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(crate::EPIPE))
    }
}

impl AsyncStreamWrite for OwnedFdStreamWrite {
    fn write<'a>(
        &'a mut self,
        buffer: &'a [u8],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<(), Errno>> + 'a {
        write_impl(&mut self.fd, buffer, deadline)
    }

    fn writev<'a>(
        &'a mut self,
        buffers: &'a mut [IoSlice<'a>],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<(), Errno>> + 'a {
        writev_impl(&mut self.fd, buffers, deadline)
    }

    fn shutdown(&mut self) -> impl Future<Output = Result<(), Errno>> {
        shutdown_impl(&mut self.fd)
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Errno>> {
        close_impl(&mut self.fd)
    }
}

impl AsyncStreamWrite for OwnedFdStream {
    fn write<'a>(
        &'a mut self,
        buffer: &'a [u8],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<(), Errno>> + 'a {
        write_impl(&mut self.fd, buffer, deadline)
    }

    fn writev<'a>(
        &'a mut self,
        buffers: &'a mut [IoSlice<'a>],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<(), Errno>> + 'a {
        writev_impl(&mut self.fd, buffers, deadline)
    }

    fn shutdown(&mut self) -> impl Future<Output = Result<(), Errno>> {
        shutdown_impl(&mut self.fd)
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Errno>> {
        close_impl(&mut self.fd)
    }
}

#[cfg(test)]
mod test {
    use crate::{
        AsyncStreamRead, AsyncStreamWrite, OwnedFdStream, SplittableStream, operations,
        pipe::bipipe,
    };
    use rustix::fd::{FromRawFd, IntoRawFd, OwnedFd};
    use std::io::{IoSlice, Seek, Write};

    #[crate::test]
    async fn manual_channel_test() {
        let (client, server) = bipipe();
        let mut stream1 = OwnedFdStream::new(client);
        let mut stream2 = OwnedFdStream::new(server);

        let iters = 200usize;
        for iter in 0..iters {
            let mut buffer = [0; 128];
            // let mut buffer = BufferView::new(&mut buffer);
            // buffer.set(&iter.to_le_bytes());
            let id = 1u32;
            // buffer.prefix(&id.to_le_bytes());
            let length: u32 = 12;
            // buffer.prefix(&length.to_le_bytes());
            buffer[0..4].copy_from_slice(&length.to_le_bytes());
            buffer[4..8].copy_from_slice(&id.to_le_bytes());
            buffer[8..16].copy_from_slice(&iter.to_le_bytes());

            stream1.write(&buffer[0..16], None).await.unwrap();

            let mut response = [0; 128];
            stream2.read(&mut response[0..4], None).await.unwrap();
            let length = u32::from_le_bytes(response[0..4].try_into().unwrap()) as usize;
            stream2.read(&mut response[0..length], None).await.unwrap();
            let id = u32::from_le_bytes(response[0..4].try_into().unwrap()) as usize;
            assert_eq!(id, 1);
            let response = &response[4..length];
            assert_eq!(&iter.to_le_bytes(), response);
        }

        stream1.shutdown().await.unwrap();
        stream2.shutdown().await.unwrap();
    }

    #[crate::test]
    async fn try_read_test() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"hello world")
            .expect("Failed to write to file");
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut stream = OwnedFdStream::new(unsafe { OwnedFd::from_raw_fd(file.into_raw_fd()) });

        let mut buffer = [0; 100];
        let amount = stream.try_read(&mut buffer, None).await.unwrap();
        assert_eq!(amount, 11);
        let amount = stream.try_read(&mut buffer, None).await.unwrap();
        assert_eq!(amount, 0);
        assert_eq!(&buffer[0..11], b"hello world");
    }

    #[crate::test]
    async fn short_read_test() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"hello world")
            .expect("Failed to write to file");
        file.seek(std::io::SeekFrom::Start(0)).unwrap();

        // go into fd and back
        let stream = OwnedFdStream::new(unsafe { OwnedFd::from_raw_fd(file.into_raw_fd()) });
        let fd = stream.into_inner().unwrap();
        let mut stream = OwnedFdStream::new(fd);

        let mut buffer = [0; 11];
        stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer[0..11], b"hello world");
        let e = stream.read(&mut buffer, None).await;
        assert!(e.is_err());
    }

    #[crate::test]
    async fn read_failed_test() {
        let (client, server) = bipipe();
        operations::close(server).await.unwrap();

        let mut stream = OwnedFdStream::new(client);
        stream.read(&mut [0; 11], None).await.unwrap_err();
    }

    #[crate::test]
    async fn owned_fd_stream_split_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (_read_stream, mut write_stream) = stream.split().await.unwrap();

        // Write data using the write stream
        write_stream.write(b"hello split", None).await.unwrap();

        // Read data using the read stream from the server side
        let mut server_stream = OwnedFdStream::new(server);
        let mut buffer = [0u8; 11];
        server_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"hello split");
    }

    #[crate::test]
    async fn owned_fd_stream_read_stream_try_read_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (mut read_stream, _write_stream) = stream.split().await.unwrap();

        // Write data from server side
        let mut server_stream = OwnedFdStream::new(server);
        server_stream.write(b"test try_read", None).await.unwrap();

        // Test try_read on the read stream
        let mut buffer = [0u8; 5];
        let amount = read_stream.try_read(&mut buffer, None).await.unwrap();
        assert_eq!(amount, 5);
        assert_eq!(&buffer, b"test ");

        // Read remaining data
        let mut remaining = [0u8; 8];
        let remaining_amount = read_stream.try_read(&mut remaining, None).await.unwrap();
        assert_eq!(remaining_amount, 8);
        assert_eq!(&remaining, b"try_read");
    }

    #[crate::test]
    async fn owned_fd_stream_read_stream_read_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (mut read_stream, _write_stream) = stream.split().await.unwrap();

        // Test reading exact amount
        let write_task = {
            let mut server_stream = OwnedFdStream::new(server);
            crate::operations::spawn_task(async move {
                server_stream.write(b"exact", None).await.unwrap();
                server_stream.write(b"concat", None).await.unwrap();
                server_stream
            })
        };

        // Read exact amount first
        let mut buffer = [0u8; 5];
        read_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"exact");

        // Read concatenated data
        let mut buffer2 = [0u8; 6];
        read_stream.read(&mut buffer2, None).await.unwrap();
        assert_eq!(&buffer2, b"concat");

        write_task.await.unwrap();
    }

    #[crate::test]
    async fn owned_fd_stream_write_stream_write_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (_read_stream, mut write_stream) = stream.split().await.unwrap();

        // Test writing various sizes
        write_stream.write(b"small", None).await.unwrap();
        write_stream
            .write(b" large data chunk", None)
            .await
            .unwrap();

        // Read back from server
        let mut server_stream = OwnedFdStream::new(server);
        let mut buffer = [0u8; 22];
        server_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"small large data chunk");
    }

    #[crate::test]
    async fn owned_fd_stream_write_stream_writev_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (_read_stream, mut write_stream) = stream.split().await.unwrap();

        // Test writev with multiple buffers
        let buf1 = b"hello";
        let buf2 = b" ";
        let buf3 = b"vectored";
        let buf4 = b" write";
        let mut buffers = [
            IoSlice::new(buf1.as_slice()),
            IoSlice::new(buf2.as_slice()),
            IoSlice::new(buf3.as_slice()),
            IoSlice::new(buf4.as_slice()),
        ];

        write_stream.writev(&mut buffers, None).await.unwrap();

        // Read back from server
        let mut server_stream = OwnedFdStream::new(server);
        let mut buffer = [0u8; 20];
        server_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"hello vectored write");
    }

    #[crate::test]
    async fn owned_fd_stream_write_stream_shutdown_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (_read_stream, mut write_stream) = stream.split().await.unwrap();

        // Write some data before shutdown
        write_stream.write(b"before shutdown", None).await.unwrap();

        // Shutdown the write stream
        write_stream.shutdown().await.unwrap();

        // Server should be able to read existing data
        let mut server_stream = OwnedFdStream::new(server);
        let mut buffer = [0u8; 15];
        server_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"before shutdown");

        // Writing after shutdown should fail
        let result = write_stream.write(b"after shutdown", None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn owned_fd_stream_write_stream_close_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (_read_stream, mut write_stream) = stream.split().await.unwrap();

        // Write some data before close
        write_stream.write(b"before close", None).await.unwrap();

        // Close the write stream
        write_stream.close().await.unwrap();

        // Server should be able to read existing data
        let mut server_stream = OwnedFdStream::new(server);
        let mut buffer = [0u8; 12];
        server_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"before close");

        // Writing after close should fail
        let result = write_stream.write(b"after close", None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn owned_fd_stream_read_stream_close_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (mut read_stream, _write_stream) = stream.split().await.unwrap();

        // Write some data from server side and close server
        let mut server_stream = OwnedFdStream::new(server);
        server_stream.write(b"test data", None).await.unwrap();
        server_stream.close().await.unwrap();

        // Read some data
        let mut buffer = [0u8; 9];
        read_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"test data");

        // Close the read stream
        read_stream.close().await.unwrap();

        // Reading after close should fail
        let mut empty_buffer = [0u8; 1];
        let result = read_stream.read(&mut empty_buffer, None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn owned_fd_stream_split_concurrent_operations_test() {
        let (client, server) = bipipe();
        let stream1 = OwnedFdStream::new(client);
        let stream2 = OwnedFdStream::new(server);
        let (read1, write1) = stream1.split().await.unwrap();
        let (read2, write2) = stream2.split().await.unwrap();

        let write_task = {
            let mut write1 = write1;
            let mut write2 = write2;
            crate::operations::spawn_task(async move {
                write1.write(b"from1to2", None).await.unwrap();
                write2.write(b"from2to1", None).await.unwrap();
                write1.shutdown().await.unwrap();
                write2.shutdown().await.unwrap();
            })
        };

        let read_task = {
            let mut read1 = read1;
            let mut read2 = read2;
            crate::operations::spawn_task(async move {
                let mut buffer1 = [0u8; 8];
                let mut buffer2 = [0u8; 8];

                // read1 should receive data from write2
                read1.read(&mut buffer1, None).await.unwrap();
                // read2 should receive data from write1
                read2.read(&mut buffer2, None).await.unwrap();

                (buffer1, buffer2)
            })
        };

        let (buffer1, buffer2) = read_task.await.unwrap();
        write_task.await.unwrap();

        assert_eq!(&buffer1, b"from2to1");
        assert_eq!(&buffer2, b"from1to2");
    }

    #[crate::test]
    async fn owned_fd_stream_split_writev_large_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (_read_stream, mut write_stream) = stream.split().await.unwrap();

        // Test writev with large data that spans multiple buffers
        let buf1 = vec![65u8; 4096]; // 'A' repeated 4096 times
        let buf2 = vec![66u8; 4096]; // 'B' repeated 4096 times
        let buf3 = b"end";
        let mut buffers = [
            IoSlice::new(buf1.as_slice()),
            IoSlice::new(buf2.as_slice()),
            IoSlice::new(buf3.as_slice()),
        ];

        let read_task = {
            let mut server_stream = OwnedFdStream::new(server);
            crate::operations::spawn_task(async move {
                // Read data in chunks
                let mut result = Vec::new();
                let mut temp_buffer = [0u8; 1024];
                for _ in 0..8 {
                    server_stream.read(&mut temp_buffer, None).await.unwrap();
                    result.extend_from_slice(&temp_buffer);
                }
                let mut end_buffer = [0u8; 3];
                server_stream.read(&mut end_buffer, None).await.unwrap();
                result.extend_from_slice(&end_buffer);
                result
            })
        };

        write_stream.writev(&mut buffers, None).await.unwrap();

        let result = read_task.await.unwrap();

        // Verify the data
        assert_eq!(result.len(), 8195); // 4096 + 4096 + 3
        assert!(result[0..4096].iter().all(|&x| x == 65)); // All 'A's
        assert!(result[4096..8192].iter().all(|&x| x == 66)); // All 'B's
        assert_eq!(&result[8192..8195], b"end");
    }
}
