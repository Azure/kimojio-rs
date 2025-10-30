// Copyright (c) Microsoft Corporation. All rights reserved.
use crate::{AsyncStreamRead, AsyncStreamWrite};
use rustix_uring::Errno;
use std::collections::VecDeque;

fn would_block() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        "SyncBufferedStream lacks data.",
    )
}

/// A synchronous stream backed by a buffer that implements `std::io::Read` and `std::io::Write`.
/// TODO: The buffering is not optimal, consider using a more efficient buffer management strategy.
pub struct SyncBufferedStream {
    read_buff: VecDeque<u8>,
    write_buff: VecDeque<u8>,
    eof: bool,
}

impl SyncBufferedStream {
    pub fn new() -> Self {
        Self {
            read_buff: VecDeque::new(),
            write_buff: VecDeque::new(),
            eof: false,
        }
    }
}

impl std::io::Read for SyncBufferedStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.eof {
            return Ok(0);
        }
        assert_ne!(buf.len(), 0, "Buffer must not be empty");
        // We read only the first segment of the buffer, the rest will be read in the next call.
        let (slice1, slice2) = self.read_buff.as_slices();
        let slice = if slice1.is_empty() { slice2 } else { slice1 };
        // read from the read_buff
        let bytes_read = buf.len().min(slice.len());
        if bytes_read == 0 {
            // no data ready signal caller to fill the buffer
            return Err(would_block());
        } else {
            buf[..bytes_read].copy_from_slice(&slice[..bytes_read]);
            self.read_buff.drain(..bytes_read);
        }
        Ok(bytes_read)
    }
}

impl std::io::Write for SyncBufferedStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // write to the write_buff
        self.write_buff.extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // User needs to manually flush it using `flush_write_buff`
        Ok(())
    }
}

impl SyncBufferedStream {
    /// Read from stream into buffer
    pub async fn fill_read_buff(
        &mut self,
        s: &mut impl AsyncStreamRead,
        deadline: Option<std::time::Instant>,
    ) -> Result<(), Errno> {
        // TODO: reuse buffer and handle deadline.
        // VecDeque does not expose a way to get available slice, may need to use a different structure.
        let mut buf = [0u8; 1024]; // Example buffer size
        let bytes_read = s.try_read(&mut buf, deadline).await?;
        if bytes_read == 0 {
            self.eof = true; // Mark EOF if no bytes were read
        } else {
            self.read_buff.extend(&buf[..bytes_read]);
        }
        Ok(())
    }

    /// Write the buffered data to the underlying stream
    /// Currently it always flushes the entire buffer.
    pub async fn flush_write_buff(
        &mut self,
        s: &mut impl AsyncStreamWrite,
        deadline: Option<std::time::Instant>,
    ) -> Result<usize, Errno> {
        let bytes_to_write = self.write_buff.len();
        if bytes_to_write == 0 {
            return Ok(0); // Nothing to write
        }
        let (slice1, _slice2) = self.write_buff.as_slices();
        // We always flush the entire buffer so slice one should contain all data.
        assert_eq!(slice1.len(), self.write_buff.len(), "Slice length mismatch");
        s.write(slice1, deadline).await?;
        self.write_buff.clear();
        Ok(bytes_to_write)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use crate::BufferStream;

    use super::*;

    #[test]
    fn test_sync_buffered_stream() {
        crate::run_test("server_client", sync_buffered_stream())
    }

    async fn sync_buffered_stream() {
        let data = b"Hello, world!";
        let data2 = b"Hello, world!2";

        // External data source
        let mut s = BufferStream::new();
        let mut buff_s = SyncBufferedStream::new();
        let mut read_buf = [0u8; 1024];
        {
            s.write(data, None).await.unwrap();
            // copy external data to the buffer and then read it.
            buff_s.fill_read_buff(&mut s, None).await.unwrap();

            let bytes_read = buff_s.read(&mut read_buf).unwrap();
            assert_eq!(bytes_read, data.len());
            assert_eq!(&read_buf[..bytes_read], data);

            // stream is empty now, and would block on read
            let e = buff_s.read(&mut read_buf).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
        }
        // test for various sizes of writes
        for i in 1..data2.len() {
            // write to the stream and flush to external stream
            let chunk = &data2[0..i];
            buff_s.write_all(chunk).unwrap();
            buff_s.flush_write_buff(&mut s, None).await.unwrap();
            // read again.
            buff_s.fill_read_buff(&mut s, None).await.unwrap();
            let bytes_read = buff_s.read(&mut read_buf).unwrap();
            assert_eq!(bytes_read, chunk.len());
            assert_eq!(&read_buf[..bytes_read], chunk);
        }
    }
}
