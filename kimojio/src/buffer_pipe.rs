// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{AsyncEvent, AsyncLock, AsyncStreamRead, AsyncStreamWrite, Errno, SplittableStream};
use std::{cell::Cell, rc::Rc, time::Instant};

/// A simple in-memory pipe implementation for testing purposes.
#[derive(Clone, Default)]
pub struct BufferPipe {
    read: Rc<BufferStream>,
    write: Rc<BufferStream>,
}

impl BufferPipe {
    /// Creates a pair of connected buffer pipes for bidirectional communication.
    ///
    /// Each pipe can read from one buffer and write to the other.
    pub fn new() -> (Self, Self) {
        let a = Rc::new(BufferStream::new());
        let b = Rc::new(BufferStream::new());
        (
            Self {
                read: a.clone(),
                write: b.clone(),
            },
            Self { read: b, write: a },
        )
    }
}

impl AsyncStreamRead for BufferPipe {
    async fn try_read(
        &mut self,
        buffer: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<usize, Errno> {
        self.read.try_read(buffer, deadline).await
    }

    async fn read(&mut self, buffer: &mut [u8], deadline: Option<Instant>) -> Result<(), Errno> {
        self.read.read(buffer, deadline).await
    }
}

impl AsyncStreamWrite for BufferPipe {
    async fn write(&mut self, buffer: &[u8], deadline: Option<Instant>) -> Result<(), Errno> {
        self.write.write(buffer, deadline).await
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        self.write.shutdown().await
    }

    async fn close(&mut self) -> Result<(), Errno> {
        self.read.close().await?;
        self.write.close().await
    }
}

impl SplittableStream for BufferPipe {
    type ReadStream = BufferReadStream;
    type WriteStream = BufferWriteStream;

    async fn split(self) -> Result<(Self::ReadStream, Self::WriteStream), Errno> {
        let read = self.read;
        let write = self.write;
        Ok((BufferReadStream(read), BufferWriteStream(write)))
    }
}

struct BufferStreamState {
    buffer: [u8; 1024],
    head: usize,
    amount: usize,
}

/// A single-direction in-memory buffer stream.
///
/// Provides async read and write operations backed by an in-memory buffer.
pub struct BufferStream {
    state: AsyncLock<BufferStreamState>,
    available: AsyncEvent,
    not_full: AsyncEvent,
    closed: Cell<bool>,
}

impl BufferStream {
    /// Creates a new empty buffer stream.
    pub fn new() -> Self {
        let not_full = AsyncEvent::new();
        not_full.set();
        Self {
            state: AsyncLock::new(BufferStreamState {
                buffer: [0; 1024],
                head: 0,
                amount: 0,
            }),
            available: AsyncEvent::new(),
            not_full,
            closed: Cell::new(false),
        }
    }

    fn update_events(&self, state: &BufferStreamState) {
        let available = state.amount > 0;
        let full = state.amount == state.buffer.len();

        if available {
            self.available.set();
        } else {
            self.available.reset();
        }

        if full {
            self.not_full.reset();
        } else {
            self.not_full.set();
        }
    }

    async fn try_read(&self, buffer: &mut [u8], deadline: Option<Instant>) -> Result<usize, Errno> {
        if self.closed.get() && !self.available.is_set() {
            return Err(Errno::from_raw_os_error(crate::EPIPE));
        }

        self.available.wait().await?;

        let mut state = self.state.lock_with_deadline(deadline).await?;

        let end = std::cmp::min(state.head + state.amount, state.buffer.len());
        let amount = std::cmp::min(end - state.head, buffer.len());

        if amount > 0 {
            buffer[0..amount].copy_from_slice(&state.buffer[state.head..state.head + amount]);
            state.head = (state.head + amount) % state.buffer.len();
            state.amount -= amount;
        }

        self.update_events(&state);

        Ok(amount)
    }

    async fn read(&self, buffer: &mut [u8], deadline: Option<Instant>) -> Result<(), Errno> {
        if self.closed.get() && !self.available.is_set() {
            return Err(Errno::from_raw_os_error(crate::EPIPE));
        }

        let mut read_len = 0;
        while read_len < buffer.len() {
            self.available.wait_with_deadline(deadline).await?;

            let len = self.try_read(&mut buffer[read_len..], deadline).await?;
            if len == 0 {
                return Err(Errno::from_raw_os_error(crate::EPIPE));
            }
            read_len += len;
        }
        Ok(())
    }

    async fn write(&self, mut buffer: &[u8], deadline: Option<Instant>) -> Result<(), Errno> {
        if self.closed.get() {
            return Err(Errno::from_raw_os_error(crate::EPIPE));
        }

        while !buffer.is_empty() {
            // TODO: timeout
            self.not_full.wait_with_deadline(deadline).await?;

            let mut state = self.state.lock().await?;
            let free_head = (state.head + state.amount) % state.buffer.len();
            let free_amount = state.buffer.len() - state.amount;
            let free_end = std::cmp::min(free_head + free_amount, state.buffer.len());
            let amount = std::cmp::min(buffer.len(), free_end - free_head);

            state.buffer[free_head..free_head + amount].copy_from_slice(&buffer[0..amount]);
            state.amount += amount;
            buffer = &buffer[amount..];

            self.update_events(&state);
        }

        Ok(())
    }

    async fn shutdown(&self) -> Result<(), Errno> {
        self.closed.set(true);
        self.available.set();
        self.not_full.set();
        Ok(())
    }

    async fn close(&self) -> Result<(), Errno> {
        self.closed.set(true);
        self.available.set();
        self.not_full.set();
        Ok(())
    }
}

impl Default for BufferStream {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncStreamRead for BufferStream {
    async fn try_read(
        &mut self,
        buffer: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<usize, Errno> {
        BufferStream::try_read(self, buffer, deadline).await
    }

    async fn read(&mut self, buffer: &mut [u8], deadline: Option<Instant>) -> Result<(), Errno> {
        BufferStream::read(self, buffer, deadline).await
    }
}

impl AsyncStreamWrite for BufferStream {
    async fn write(&mut self, buffer: &[u8], deadline: Option<Instant>) -> Result<(), Errno> {
        BufferStream::write(self, buffer, deadline).await
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        BufferStream::shutdown(self).await
    }

    async fn close(&mut self) -> Result<(), Errno> {
        BufferStream::close(self).await
    }
}

/// The read half of a `BufferPipe` after splitting.
pub struct BufferReadStream(Rc<BufferStream>);

/// The write half of a `BufferPipe` after splitting.
pub struct BufferWriteStream(Rc<BufferStream>);

impl AsyncStreamRead for BufferReadStream {
    async fn try_read(
        &mut self,
        buffer: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<usize, Errno> {
        self.0.try_read(buffer, deadline).await
    }
    async fn read(&mut self, buffer: &mut [u8], deadline: Option<Instant>) -> Result<(), Errno> {
        self.0.read(buffer, deadline).await
    }
}

impl AsyncStreamWrite for BufferWriteStream {
    async fn write(&mut self, buffer: &[u8], deadline: Option<Instant>) -> Result<(), Errno> {
        self.0.write(buffer, deadline).await
    }
    async fn shutdown(&mut self) -> Result<(), Errno> {
        self.0.shutdown().await
    }
    async fn close(&mut self) -> Result<(), Errno> {
        self.0.close().await
    }
}

impl SplittableStream for BufferStream {
    type ReadStream = BufferReadStream;
    type WriteStream = BufferWriteStream;

    async fn split(self) -> Result<(Self::ReadStream, Self::WriteStream), Errno> {
        let write = Rc::new(self);
        let read = write.clone();

        Ok((BufferReadStream(read), BufferWriteStream(write)))
    }
}

#[cfg(test)]
mod test {
    use super::{BufferPipe, BufferStream};
    use crate::{AsyncStreamRead, AsyncStreamWrite, SplittableStream, operations};
    use std::rc::Rc;

    #[crate::test]
    async fn buffer_stream_test_write_larger_than_internal_buffer() {
        let mut data = Vec::new();
        let stream = Rc::new(BufferStream::new());

        let task = {
            let stream = stream.clone();
            operations::spawn_task(async move {
                loop {
                    let mut buffer = [0; 1024];
                    if stream.read(&mut buffer, None).await.is_err() {
                        return data;
                    }
                    data.extend_from_slice(&buffer);
                }
            })
        };

        stream.write(&[42u8; 4096], None).await.unwrap();
        stream.shutdown().await.unwrap();
        let data = task.await.unwrap();
        assert_eq!(&[42u8; 4096], &data[0..]);
    }

    #[crate::test]
    async fn buffer_stream_contiguous_full_buffer_test() {
        let stream = BufferStream::new();
        // Fill the stream completely.
        let data = [7u8; 1024];
        stream.write(&data, None).await.unwrap();

        // Read back all data.
        let mut read_buf = [0u8; 1024];
        stream.read(&mut read_buf, None).await.unwrap();
        assert_eq!(data, read_buf);
    }

    #[crate::test]
    async fn buffer_stream_interleaved_read_write_test() {
        let stream = BufferStream::new();
        // Write part of the data.
        stream.write(b"abc", None).await.unwrap();
        // Perform a partial read.
        let mut buf = [0u8; 2];
        let amount = stream.try_read(&mut buf, None).await.unwrap();
        assert_eq!(amount, 2);
        assert_eq!(&buf, b"ab");

        // Now write additional data.
        stream.write(b"defgh", None).await.unwrap();

        // Read the remaining data.
        let mut buf2 = [0u8; 6];
        stream.read(&mut buf2, None).await.unwrap();
        // Expected concatenation: "cdefgh"
        assert_eq!(&buf2, b"cdefgh");
    }

    #[crate::test]
    async fn buffer_stream_close_and_error_test() {
        let stream = BufferStream::new();
        stream.write(b"data", None).await.unwrap();

        // Shut down the stream.
        stream.shutdown().await.unwrap();

        // Subsequent operations should error.
        let res2 = stream.write(b"more", None).await;
        assert!(res2.is_err());

        let mut buf = [0u8; 4];
        stream.read(&mut buf, None).await.unwrap();
        assert_eq!(&buf, b"data");

        let res = stream.read(&mut buf, None).await;
        assert!(res.is_err());
        let res3 = stream.try_read(&mut buf, None).await;
        assert!(res3.is_err());
    }

    #[crate::test]
    async fn buffer_stream_multiple_small_writes_test() {
        let stream = BufferStream::new();

        // Write several small chunks.
        let data: [&'static [u8]; 4] = [b"hello", b" ", b"world", b"!"];
        for chunk in data {
            stream.write(chunk, None).await.unwrap();
        }

        // Read back the entire message.
        let mut result = vec![0u8; 12];
        stream.read(&mut result, None).await.unwrap();
        assert_eq!(&result, b"hello world!");
    }

    #[crate::test]
    async fn buffer_stream_tests() {
        let mut buffer_stream = BufferStream::new();
        let mut buffer1 = [1; 1024];

        async fn expect(stream: &mut BufferStream, buffer: &[u8]) {
            let mut result_buffer = [0; 8192];
            let result_buffer = &mut result_buffer[0..buffer.len()];
            stream.read(result_buffer, None).await.unwrap();
            assert_eq!(buffer, result_buffer);
        }

        async fn write_read(stream: &mut BufferStream, buffer: &[u8]) {
            stream.write(buffer, None).await.unwrap();
            expect(stream, buffer).await;
        }

        write_read(&mut buffer_stream, &[1; 1024]).await;
        write_read(&mut buffer_stream, &[2; 512]).await;
        write_read(&mut buffer_stream, &[3; 512]).await;

        write_read(&mut buffer_stream, &[4; 512]).await;
        write_read(&mut buffer_stream, &[5; 1024]).await;

        // 2. try_read returns some data when the buffer has some
        buffer_stream.write(b"hello", None).await.unwrap();
        let amount = buffer_stream.try_read(&mut buffer1, None).await.unwrap();
        assert_eq!(amount, 5);
        assert_eq!(&buffer1[0..5], b"hello");

        // 3. try_read returns a short read when the buffer has partly wrapped
        buffer_stream.write(b"world", None).await.unwrap();
        let amount = buffer_stream.try_read(&mut buffer1, None).await.unwrap();
        assert_eq!(amount, 5);
        assert_eq!(&buffer1[0..5], b"world");

        // 4. read does not return until all the data has been read (need to cover partly wrapped case too)
        buffer_stream.write(b"hello", None).await.unwrap();
        buffer_stream.write(b"world", None).await.unwrap();
        let mut buffer = [0; 10];
        buffer_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"helloworld");

        // 5. try_read, read, and write to a closed stream returns an error
        buffer_stream.shutdown().await.unwrap();
        let result = buffer_stream.read(&mut buffer, None).await;
        assert!(result.is_err());
        let result = buffer_stream.write(b"test", None).await;
        assert!(result.is_err());

        async fn shutdown_and_close(mut write_stream: impl AsyncStreamWrite) {
            write_stream.shutdown().await.unwrap();
            write_stream.close().await.unwrap();
        }

        shutdown_and_close(buffer_stream).await;
    }

    #[crate::test]
    async fn buffer_stream_split_test() {
        let buffer_stream = BufferStream::new();
        let (mut read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        // Test basic write to write stream and read from read stream
        write_stream.write(b"hello world", None).await.unwrap();

        let mut buffer = [0u8; 11];
        read_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"hello world");
    }

    #[crate::test]
    async fn buffer_read_stream_try_read_test() {
        let buffer_stream = BufferStream::new();
        let (mut read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        // Write data to write stream
        write_stream.write(b"test data", None).await.unwrap();

        // Test try_read on read stream
        let mut buffer = [0u8; 5];
        let amount = read_stream.try_read(&mut buffer, None).await.unwrap();
        assert_eq!(amount, 5);
        assert_eq!(&buffer, b"test ");

        // Read remaining data
        let mut remaining = [0u8; 4];
        let remaining_amount = read_stream.try_read(&mut remaining, None).await.unwrap();
        assert_eq!(remaining_amount, 4);
        assert_eq!(&remaining, b"data");
    }

    #[crate::test]
    async fn buffer_read_stream_read_test() {
        let buffer_stream = BufferStream::new();
        let (mut read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        // Test reading exact amount
        write_stream.write(b"exact", None).await.unwrap();
        let mut buffer = [0u8; 5];
        read_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"exact");

        // Test reading with concurrent writes
        let write_task = {
            let mut write_stream = write_stream;
            crate::operations::spawn_task(async move {
                write_stream.write(b"part1", None).await.unwrap();
                write_stream.write(b"part2", None).await.unwrap();
                write_stream
            })
        };

        let mut large_buffer = [0u8; 10];
        read_stream.read(&mut large_buffer, None).await.unwrap();
        assert_eq!(&large_buffer, b"part1part2");

        write_task.await.unwrap();
    }

    #[crate::test]
    async fn buffer_write_stream_write_test() {
        let buffer_stream = BufferStream::new();
        let (read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        let read_task = {
            let mut read_stream = read_stream;
            crate::operations::spawn_task(async move {
                // Read back small and medium text
                let mut buffer1 = [0u8; 24];
                read_stream.read(&mut buffer1, None).await.unwrap();

                // Read back large data in chunks
                let mut large_buffer = Vec::new();
                let mut temp_buffer = [0u8; 1024];
                for _ in 0..2 {
                    read_stream.read(&mut temp_buffer, None).await.unwrap();
                    large_buffer.extend_from_slice(&temp_buffer);
                }
                (buffer1, large_buffer)
            })
        };

        // Test writing various sizes
        write_stream.write(b"small", None).await.unwrap();
        write_stream
            .write(b" medium sized text ", None)
            .await
            .unwrap();

        // Test writing large data that exceeds internal buffer
        let large_data = [42u8; 2048];
        write_stream.write(&large_data, None).await.unwrap();

        let (buffer1, buffer2) = read_task.await.unwrap();
        assert_eq!(&buffer1, b"small medium sized text ");
        assert_eq!(&buffer2, &large_data);
    }

    #[crate::test]
    async fn buffer_write_stream_shutdown_test() {
        let buffer_stream = BufferStream::new();
        let (mut read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        // Write some data before shutdown
        write_stream.write(b"before shutdown", None).await.unwrap();

        // Shutdown write stream
        write_stream.shutdown().await.unwrap();

        // Should be able to read existing data
        let mut buffer = [0u8; 15];
        read_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"before shutdown");

        // Writing after shutdown should fail
        let result = write_stream.write(b"after shutdown", None).await;
        assert!(result.is_err());

        // Reading after shutdown when no data is available should fail
        let mut empty_buffer = [0u8; 1];
        let result = read_stream.read(&mut empty_buffer, None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn buffer_write_stream_close_test() {
        let buffer_stream = BufferStream::new();
        let (mut read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        // Write some data before close
        write_stream.write(b"before close", None).await.unwrap();

        // Close write stream
        write_stream.close().await.unwrap();

        // Should be able to read existing data
        let mut buffer = [0u8; 12];
        read_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"before close");

        // Writing after close should fail
        let result = write_stream.write(b"after close", None).await;
        assert!(result.is_err());

        // Reading after close when no data is available should fail
        let mut empty_buffer = [0u8; 1];
        let result = read_stream.read(&mut empty_buffer, None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn buffer_split_streams_concurrent_operations_test() {
        let buffer_stream = BufferStream::new();
        let (read_stream, write_stream) = buffer_stream.split().await.unwrap();

        let read_task = {
            let mut read_stream = read_stream;
            crate::operations::spawn_task(async move {
                let mut result = Vec::new();
                let mut buffer = [0u8; 10];

                // Read three chunks
                for _ in 0..3 {
                    read_stream.read(&mut buffer, None).await.unwrap();
                    result.extend_from_slice(&buffer);
                }
                result
            })
        };

        let write_task = {
            let mut write_stream = write_stream;
            crate::operations::spawn_task(async move {
                write_stream.write(b"chunk1____", None).await.unwrap();
                write_stream.write(b"chunk2____", None).await.unwrap();
                write_stream.write(b"chunk3____", None).await.unwrap();
                write_stream.shutdown().await.unwrap();
            })
        };

        let read_result = read_task.await.unwrap();
        write_task.await.unwrap();

        assert_eq!(&read_result, b"chunk1____chunk2____chunk3____");
    }

    #[crate::test]
    async fn buffer_split_streams_partial_reads_test() {
        let buffer_stream = BufferStream::new();
        let (mut read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        // Write data in chunks
        write_stream.write(b"abcdefgh", None).await.unwrap();

        // Read partial data with try_read
        let mut buffer1 = [0u8; 3];
        let amount1 = read_stream.try_read(&mut buffer1, None).await.unwrap();
        assert_eq!(amount1, 3);
        assert_eq!(&buffer1, b"abc");

        let mut buffer2 = [0u8; 2];
        let amount2 = read_stream.try_read(&mut buffer2, None).await.unwrap();
        assert_eq!(amount2, 2);
        assert_eq!(&buffer2, b"de");

        // Read remaining with exact read
        let mut buffer3 = [0u8; 3];
        read_stream.read(&mut buffer3, None).await.unwrap();
        assert_eq!(&buffer3, b"fgh");
    }

    #[crate::test]
    async fn buffer_pipe_split_test() {
        let (pipe1, pipe2) = BufferPipe::new();
        let (mut read_stream, mut write_stream) = pipe1.split().await.unwrap();

        // Write data using the write stream from split
        write_stream.write(b"pipe data", None).await.unwrap();

        // The other pipe should be able to read this data
        let mut buffer = [0u8; 9];
        let (mut other_read, mut other_write) = pipe2.split().await.unwrap();
        other_read.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"pipe data");

        // Write from the other direction
        other_write.write(b"reverse", None).await.unwrap();

        // Read from the original read stream
        let mut reverse_buffer = [0u8; 7];
        read_stream.read(&mut reverse_buffer, None).await.unwrap();
        assert_eq!(&reverse_buffer, b"reverse");
    }

    #[crate::test]
    async fn buffer_pipe_try_read_test() {
        let (mut pipe1, mut pipe2) = BufferPipe::new();

        // Write data to pipe2
        pipe2.write(b"test data for try_read", None).await.unwrap();

        // Use try_read on pipe1 to read partial data
        let mut buffer1 = [0u8; 4];
        let amount1 = pipe1.try_read(&mut buffer1, None).await.unwrap();
        assert_eq!(amount1, 4);
        assert_eq!(&buffer1, b"test");

        // Read remaining data with another try_read
        let mut buffer2 = [0u8; 18];
        let amount2 = pipe1.try_read(&mut buffer2, None).await.unwrap();
        assert_eq!(amount2, 18);
        assert_eq!(&buffer2, b" data for try_read");
    }

    #[crate::test]
    async fn buffer_pipe_read_test() {
        let (mut pipe1, mut pipe2) = BufferPipe::new();

        // Test reading exact amount
        pipe2.write(b"exact", None).await.unwrap();
        let mut buffer = [0u8; 5];
        pipe1.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"exact");

        // Test reading with concurrent writes
        let write_task = {
            let mut pipe2 = pipe2;
            crate::operations::spawn_task(async move {
                pipe2.write(b"hello", None).await.unwrap();
                pipe2.write(b"world", None).await.unwrap();
                pipe2
            })
        };

        let mut large_buffer = [0u8; 10];
        pipe1.read(&mut large_buffer, None).await.unwrap();
        assert_eq!(&large_buffer, b"helloworld");

        write_task.await.unwrap();
    }

    #[crate::test]
    async fn buffer_pipe_write_test() {
        let (mut pipe1, pipe2) = BufferPipe::new();

        // Test writing various sizes
        pipe1.write(b"small", None).await.unwrap();
        pipe1.write(b" medium sized text ", None).await.unwrap();

        // Test writing large data that exceeds internal buffer
        let large_data = [42u8; 2048];

        let read_task = {
            let mut pipe2 = pipe2;
            crate::operations::spawn_task(async move {
                // Read back small and medium text
                let mut buffer1 = [0u8; 24];
                pipe2.read(&mut buffer1, None).await.unwrap();

                // Read back large data in chunks
                let mut large_buffer = Vec::new();
                let mut temp_buffer = [0u8; 1024];
                for _ in 0..2 {
                    pipe2.read(&mut temp_buffer, None).await.unwrap();
                    large_buffer.extend_from_slice(&temp_buffer);
                }
                (buffer1, large_buffer)
            })
        };

        pipe1.write(&large_data, None).await.unwrap();

        let (buffer1, buffer2) = read_task.await.unwrap();
        assert_eq!(&buffer1, b"small medium sized text ");
        assert_eq!(&buffer2, &large_data);
    }

    #[crate::test]
    async fn buffer_pipe_shutdown_test() {
        let (mut pipe1, mut pipe2) = BufferPipe::new();

        // Write some data before shutdown
        pipe1.write(b"before shutdown", None).await.unwrap();

        // Shutdown pipe1
        pipe1.shutdown().await.unwrap();

        // Should be able to read existing data from pipe2
        let mut buffer = [0u8; 15];
        pipe2.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"before shutdown");

        // Writing after shutdown should fail
        let result = pipe1.write(b"after shutdown", None).await;
        assert!(result.is_err());

        // Reading after shutdown when no data is available should fail
        let mut empty_buffer = [0u8; 1];
        let result = pipe2.read(&mut empty_buffer, None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn buffer_pipe_close_test() {
        let (mut pipe1, mut pipe2) = BufferPipe::new();

        // Write some data before close
        pipe1.write(b"before close", None).await.unwrap();

        // Close pipe1
        pipe1.close().await.unwrap();

        // Should be able to read existing data from pipe2
        let mut buffer = [0u8; 12];
        pipe2.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"before close");

        // Writing after close should fail
        let result = pipe1.write(b"after close", None).await;
        assert!(result.is_err());

        // Reading after close when no data is available should fail
        let mut empty_buffer = [0u8; 1];
        let result = pipe2.read(&mut empty_buffer, None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn buffer_stream_default_test() {
        let stream = BufferStream::default();

        // Test that default stream works like a new stream
        stream.write(b"default test", None).await.unwrap();

        let mut buffer = [0u8; 12];
        stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"default test");
    }

    #[crate::test]
    async fn buffer_stream_shutdown_test() {
        let buffer_stream = BufferStream::new();

        // Write some data before shutdown
        buffer_stream.write(b"before shutdown", None).await.unwrap();

        // Shutdown the stream
        buffer_stream.shutdown().await.unwrap();

        // Should be able to read existing data
        let mut buffer = [0u8; 15];
        buffer_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"before shutdown");

        // Writing after shutdown should fail
        let result = buffer_stream.write(b"after shutdown", None).await;
        assert!(result.is_err());

        // Reading after shutdown when no data is available should fail
        let mut empty_buffer = [0u8; 1];
        let result = buffer_stream.read(&mut empty_buffer, None).await;
        assert!(result.is_err());

        // try_read should also fail after shutdown with no data
        let result = buffer_stream.try_read(&mut empty_buffer, None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn buffer_stream_close_test() {
        let buffer_stream = BufferStream::new();

        // Write some data before close
        buffer_stream.write(b"before close", None).await.unwrap();

        // Close the stream
        buffer_stream.close().await.unwrap();

        // Should be able to read existing data
        let mut buffer = [0u8; 12];
        buffer_stream.read(&mut buffer, None).await.unwrap();
        assert_eq!(&buffer, b"before close");

        // Writing after close should fail
        let result = buffer_stream.write(b"after close", None).await;
        assert!(result.is_err());

        // Reading after close when no data is available should fail
        let mut empty_buffer = [0u8; 1];
        let result = buffer_stream.read(&mut empty_buffer, None).await;
        assert!(result.is_err());

        // try_read should also fail after close with no data
        let result = buffer_stream.try_read(&mut empty_buffer, None).await;
        assert!(result.is_err());
    }
}
