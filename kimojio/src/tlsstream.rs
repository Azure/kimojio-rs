// Copyright (c) Microsoft Corporation. All rights reserved.
//! TlsStream brings together uringtls library and an
//! asynchronous stream to implement TLS over the stream.
//!
//! `TlsStream::server` creates a TLS "server" socket.
//! `TlsStream::client` creates a TLS "client" socket.
//!
//! TODO: implement client authentication
//!
use crate::tlscontext::as_io_error;
use crate::{
    AsyncLock, AsyncStreamRead, AsyncStreamWrite, CanceledError, Errno, OwnedFd, SplittableStream,
    operations, try_clone_owned_fd,
};
use foreign_types_shared::ForeignTypeRef;
use futures::TryFutureExt;
use kimojio_tls::TlsServer;
use std::borrow::Borrow;
use std::io::IoSlice;
use std::rc::Rc;
use std::time::Instant;

pub struct TlsStream {
    ssl: TlsServer,
    socket: Option<OwnedFd>,
}

trait SocketPair {
    fn read_socket(
        &self,
    ) -> impl Future<Output = Result<impl Borrow<Option<OwnedFd>>, CanceledError>>;
    fn write_socket(
        &self,
    ) -> impl Future<Output = Result<impl Borrow<Option<OwnedFd>>, CanceledError>>;
    // async fn close(&mut self) -> Result<(), Errno>;
}

impl SocketPair for Option<OwnedFd> {
    async fn read_socket(&self) -> Result<impl Borrow<Option<OwnedFd>>, CanceledError> {
        Ok(self)
    }

    async fn write_socket(&self) -> Result<impl Borrow<Option<OwnedFd>>, CanceledError> {
        Ok(self)
    }
}

struct SharedSocketPair {
    read_socket: AsyncLock<Option<OwnedFd>>,
    write_socket: AsyncLock<Option<OwnedFd>>,
}

impl SocketPair for Rc<SharedSocketPair> {
    fn read_socket(
        &self,
    ) -> impl Future<Output = Result<impl Borrow<Option<OwnedFd>>, CanceledError>> {
        self.read_socket.lock()
    }

    fn write_socket(
        &self,
    ) -> impl Future<Output = Result<impl Borrow<Option<OwnedFd>>, CanceledError>> {
        self.write_socket.lock()
    }
}

pub struct TlsReadStream {
    ssl: TlsServer,
    socket: Rc<SharedSocketPair>,
}

pub struct TlsWriteStream {
    ssl: TlsServer,
    socket: Rc<SharedSocketPair>,
}

impl TlsStream {
    /// Creates an instance of TlsStream.
    pub fn new_tlsstream(ssl: TlsServer, socket: OwnedFd) -> TlsStream {
        let socket = Some(socket);
        TlsStream { ssl, socket }
    }

    /// Performs the client side of TLS handshake.
    pub async fn client_side_handshake(&mut self, deadline: Option<Instant>) -> Result<(), Errno> {
        loop {
            let response = self.ssl.client_side_handshake();
            match response {
                kimojio_tls::Response::Success(_) => return Ok(()),
                kimojio_tls::Response::Fail(e) => {
                    handle_tls_error(&self.socket, "client_side_handshake", e).await?
                }
                kimojio_tls::Response::WantRead => {
                    try_read(&mut self.ssl, &self.socket, deadline).await?
                }
                kimojio_tls::Response::WantWrite => {
                    try_write(&mut self.ssl, &self.socket, deadline).await?
                }
                kimojio_tls::Response::Eof => return Ok(()),
            }
        }
    }

    /// Performs the server side of TLS handshake.
    pub async fn server_side_handshake(&mut self, deadline: Option<Instant>) -> Result<(), Errno> {
        loop {
            let response = self.ssl.server_side_handshake();
            match response {
                kimojio_tls::Response::Success(_) => return Ok(()),
                kimojio_tls::Response::Fail(e) => {
                    handle_tls_error(&self.socket, "server_side_handshake", e).await?
                }
                kimojio_tls::Response::WantRead => {
                    try_read(&mut self.ssl, &self.socket, deadline).await?
                }
                kimojio_tls::Response::WantWrite => {
                    try_write(&mut self.ssl, &self.socket, deadline).await?
                }
                kimojio_tls::Response::Eof => {
                    return Err(Errno::from_raw_os_error(crate::EPROTO));
                }
            }
        }
    }

    /// Gets the SSL object reference.
    pub fn get_ssl(&self) -> &openssl::ssl::SslRef {
        let raw_ssl = self.ssl.get_ssl_raw();
        // Safety: The raw ptr in SslRef is valid for lifetime as self,
        // and SslRef does not own the underlying SSL object.
        unsafe { openssl::ssl::SslRef::from_ptr(raw_ssl as *mut _) }
    }
}

impl SplittableStream for TlsStream {
    type ReadStream = TlsReadStream;
    type WriteStream = TlsWriteStream;

    async fn split(self) -> Result<(TlsReadStream, TlsWriteStream), Errno> {
        let read_socket = if let Some(socket) = self.socket {
            let read_socket = try_clone_owned_fd(&socket)?;
            Rc::new(SharedSocketPair {
                read_socket: AsyncLock::new(Some(read_socket)),
                write_socket: AsyncLock::new(Some(socket)),
            })
        } else {
            Rc::new(SharedSocketPair {
                read_socket: AsyncLock::new(None),
                write_socket: AsyncLock::new(None),
            })
        };
        let write_socket = read_socket.clone();
        let read_ssl = self.ssl;
        let write_ssl = read_ssl.clone();
        Ok((
            TlsReadStream {
                ssl: read_ssl,
                socket: read_socket,
            },
            TlsWriteStream {
                ssl: write_ssl,
                socket: write_socket,
            },
        ))
    }
}

async fn try_read(
    ssl: &mut TlsServer,
    socket: &impl SocketPair,
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    let socket = socket.read_socket().await?;
    if let Some(socket) = socket.borrow() {
        if let Some(buffer) = ssl.get_push_buffer() {
            let amount = operations::read_with_deadline(socket, buffer, deadline).await?;
            if amount == 0 {
                return Err(Errno::from_raw_os_error(libc::EPIPE));
            }
            ssl.use_push_buffer(amount);
        }
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(libc::EPIPE))
    }
}

async fn try_write(
    ssl: &mut TlsServer,
    socket: &impl SocketPair,
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    let socket = socket.write_socket().await?;
    if let Some(socket) = socket.borrow() {
        // Keep writing until the pull buffer is exhausted. write_internal() advances
        // buffer by the length of the full buffer copied into the BIO rather than
        // the amount of data written to the socket here. Need to fully exhaust
        // the BIO here to keep the bookkeeping in sync
        while let Some(buffer) = ssl.get_pull_buffer() {
            let amount = operations::write_with_deadline(socket, buffer, deadline).await?;
            if amount == 0 {
                return Err(Errno::from_raw_os_error(libc::EPIPE));
            }
            ssl.use_pull_buffer(amount);
        }
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(libc::EPIPE))
    }
}

// write_internal does not flush the encrypted buffer.  This allows
// multiple write_internal calls to accumulate data for a single write
// to the wire.
async fn write_internal(
    ssl: &mut TlsServer,
    socket: &impl SocketPair,
    mut buffer: &[u8],
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    while !buffer.is_empty() {
        match ssl.write(buffer) {
            kimojio_tls::Response::Success(amount) => {
                buffer = &buffer[amount..];
            }
            kimojio_tls::Response::Fail(e) => handle_tls_error(socket, "write_internal", e).await?,
            kimojio_tls::Response::WantRead => try_read(ssl, socket, deadline).await?,
            kimojio_tls::Response::WantWrite => try_write(ssl, socket, deadline).await?,
            kimojio_tls::Response::Eof => return Err(Errno::from_raw_os_error(libc::EPIPE)),
        }
    }
    Ok(())
}

pub fn tls_overhead(buffer_size: usize) -> usize {
    // The minimum frame size is zero, and maximum is 16k. Observed value
    // seems to average 6k in some cases. We will use 1024 as a likely lower
    // bound as a tradeoff between keeping the buffer trim and avoiding
    // needing to split writes.
    const TLS_FRAME_LENGTH: usize = 1024;
    const MAX_TLS_HEADER_LENGTH: usize = 40;
    buffer_size.div_ceil(TLS_FRAME_LENGTH) * MAX_TLS_HEADER_LENGTH
}

async fn handle_tls_error(
    socket: &impl SocketPair,
    _message: &str,
    e: kimojio_tls::TlsServerError,
) -> Result<(), Errno> {
    let read_socket = socket.read_socket().await?;
    let write_socket = socket.write_socket().await?;

    if read_socket.borrow().is_some() && write_socket.borrow().is_some() {
        Err(as_io_error(e))
    } else {
        Err(Errno::from_raw_os_error(libc::EPIPE))
    }
}

async fn try_read_impl(
    ssl: &mut TlsServer,
    socket: &impl SocketPair,
    buffer: &mut [u8],
    deadline: Option<Instant>,
) -> Result<usize, Errno> {
    loop {
        match ssl.read(buffer) {
            kimojio_tls::Response::Success(amount) => return Ok(amount),
            kimojio_tls::Response::Fail(e) => handle_tls_error(socket, "try_read", e).await?,
            kimojio_tls::Response::WantRead => try_read(ssl, socket, deadline).await?,
            kimojio_tls::Response::WantWrite => try_write(ssl, socket, deadline).await?,
            kimojio_tls::Response::Eof => return Ok(0),
        }
    }
}

async fn read_impl(
    ssl: &mut TlsServer,
    socket: &impl SocketPair,
    mut buffer: &mut [u8],
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    while !buffer.is_empty() {
        match ssl.read(buffer) {
            kimojio_tls::Response::Success(amount) => {
                buffer = &mut buffer[amount..];
            }
            kimojio_tls::Response::Fail(e) => handle_tls_error(socket, "try_read", e).await?,
            kimojio_tls::Response::WantRead => try_read(ssl, socket, deadline).await?,
            kimojio_tls::Response::WantWrite => try_write(ssl, socket, deadline).await?,
            kimojio_tls::Response::Eof => return Err(Errno::from_raw_os_error(libc::EPIPE)),
        }
    }
    Ok(())
}

async fn writev_impl(
    ssl: &mut TlsServer,
    socket: &impl SocketPair,
    buffers: &mut [IoSlice<'_>],
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    for buffer in buffers {
        write_internal(ssl, socket, buffer, deadline).await?;
    }
    // flush
    try_write(ssl, socket, deadline).await
}

async fn write_impl(
    ssl: &mut TlsServer,
    socket: &impl SocketPair,
    buffer: &[u8],
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    write_internal(ssl, socket, buffer, deadline).await?;
    // flush
    try_write(ssl, socket, deadline).await
}

async fn shutdown_impl(ssl: &mut TlsServer, socket: &impl SocketPair) -> Result<(), Errno> {
    loop {
        match ssl.shutdown() {
            kimojio_tls::Response::Success(_) => return Ok(()),
            kimojio_tls::Response::Fail(e) => handle_tls_error(socket, "read", e).await?,
            kimojio_tls::Response::WantRead => {
                // Graceful shutdown returns 0 byte read on EOF.
                if let Err(Errno::PIPE) = try_read(ssl, socket, None).await {
                    return Ok(());
                }
            }
            kimojio_tls::Response::WantWrite => try_write(ssl, socket, None).await?,
            kimojio_tls::Response::Eof => return Ok(()),
        }
    }
}

async fn close_impl(socket: &mut Option<OwnedFd>, cause: Result<(), Errno>) -> Result<(), Errno> {
    if let Some(socket) = socket.take() {
        operations::close(socket).await?;
        cause
    } else {
        Err(Errno::from_raw_os_error(libc::EPIPE))
    }
}

impl AsyncStreamRead for TlsStream {
    async fn try_read(
        &mut self,
        buffer: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<usize, Errno> {
        match try_read_impl(&mut self.ssl, &self.socket, buffer, deadline).await {
            Ok(amount) => Ok(amount),
            Err(e) => {
                close_impl(&mut self.socket, Err(e)).await?;
                Err(e)
            }
        }
    }

    async fn read(&mut self, buffer: &mut [u8], deadline: Option<Instant>) -> Result<(), Errno> {
        match read_impl(&mut self.ssl, &self.socket, buffer, deadline).await {
            Ok(()) => Ok(()),
            Err(e) => {
                close_impl(&mut self.socket, Err(e)).await?;
                Err(e)
            }
        }
    }
}

impl AsyncStreamRead for TlsReadStream {
    fn try_read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<usize, Errno>> + 'a {
        try_read_impl(&mut self.ssl, &self.socket, buffer, deadline)
            .or_else(|x| close_read_because(&self.socket, Err(x)))
    }

    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<(), Errno>> + 'a {
        read_impl(&mut self.ssl, &self.socket, buffer, deadline)
            .or_else(|x| close_read_because(&self.socket, Err(x)))
    }
}

impl AsyncStreamWrite for TlsStream {
    async fn writev(
        &mut self,
        buffers: &mut [IoSlice<'_>],
        deadline: Option<Instant>,
    ) -> Result<(), Errno> {
        if let Err(e) = writev_impl(&mut self.ssl, &self.socket, buffers, deadline).await {
            close_impl(&mut self.socket, Err(e)).await?;
            Err(e)
        } else {
            Ok(())
        }
    }

    async fn write(&mut self, buffer: &[u8], deadline: Option<Instant>) -> Result<(), Errno> {
        if let Err(e) = write_impl(&mut self.ssl, &self.socket, buffer, deadline).await {
            close_impl(&mut self.socket, Err(e)).await?;
            Err(e)
        } else {
            Ok(())
        }
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        if let Err(e) = shutdown_impl(&mut self.ssl, &self.socket).await {
            close_impl(&mut self.socket, Err(e)).await?;
            Err(e)
        } else {
            Ok(())
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Errno>> {
        close_impl(&mut self.socket, Ok(()))
    }
}

async fn close_read_because<T>(
    socket: &SharedSocketPair,
    cause: Result<T, Errno>,
) -> Result<T, Errno> {
    let read_socket = socket.read_socket.lock().await?.take();

    if let Some(read_socket) = read_socket {
        operations::close(read_socket).await?;
        cause
    } else {
        Err(Errno::from_raw_os_error(libc::EPIPE))
    }
}

async fn close_write_because<T>(
    socket: &SharedSocketPair,
    cause: Result<T, Errno>,
) -> Result<T, Errno> {
    let write_socket = socket.write_socket.lock().await?.take();

    if let Some(write_socket) = write_socket {
        operations::close(write_socket).await?;
        cause
    } else {
        Err(Errno::from_raw_os_error(libc::EPIPE))
    }
}

impl AsyncStreamWrite for TlsWriteStream {
    fn writev<'a>(
        &'a mut self,
        buffers: &'a mut [IoSlice<'_>],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<(), Errno>> + 'a {
        writev_impl(&mut self.ssl, &self.socket, buffers, deadline)
            .or_else(|x| close_write_because(&self.socket, Err(x)))
    }

    fn write<'a>(
        &'a mut self,
        buffer: &'a [u8],
        deadline: Option<Instant>,
    ) -> impl Future<Output = Result<(), Errno>> + 'a {
        write_impl(&mut self.ssl, &self.socket, buffer, deadline)
            .or_else(|x| close_write_because(&self.socket, Err(x)))
    }

    fn shutdown(&mut self) -> impl Future<Output = Result<(), Errno>> {
        shutdown_impl(&mut self.ssl, &self.socket)
            .or_else(|x| close_write_because(&self.socket, Err(x)))
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Errno>> {
        close_write_because(&self.socket, Ok(()))
    }
}
