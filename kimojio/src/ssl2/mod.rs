// Copyright (c) Microsoft Corporation. All rights reserved.

//! New SSL implementation.
//! This is experimental and not yet ready for production use.
//! It will take more iterations to get it right.

// The goal is to implement a ssl stream that wraps around OwnedFdStream
// and implements AsyncStreamWrite and AsyncStreamRead traits.
// The way this works is that we trick openssl::ssl::SslStream into thinking that it is reading and writing to a
// synchronous stream, and if data is not available or ready, we fill and flush the buffers using the underlying stream.

use std::io::{Read, Write};

use openssl::ssl::ShutdownResult;
use rustix_uring::Errno;

use crate::{AsyncStreamRead, AsyncStreamWrite};

#[cfg(test)]
mod e2e_tests;
mod io;

/// Openssl stream.
pub struct SslStream<S> {
    inner_s: openssl::ssl::SslStream<io::SyncBufferedStream>,
    tcp: S,
}

impl<S> SslStream<S> {
    /// Creates a new SslStream with the given OpenSSL Ssl and underlying stream.
    /// Typically the underlying stream will be an OwnedFdStream.
    pub fn new(ssl: openssl::ssl::Ssl, tcp: S) -> Result<Self, openssl::error::ErrorStack> {
        let inner_s = openssl::ssl::SslStream::new(ssl, io::SyncBufferedStream::new())?;
        Ok(Self { inner_s, tcp })
    }
}

impl<S: AsyncStreamRead + AsyncStreamWrite> SslStream<S> {
    /// Corresponds to the OpenSSL [openssl:ssl::SslStream::connect] method.
    pub async fn connect(
        &mut self,
        deadline: Option<std::time::Instant>,
    ) -> Result<(), std::io::Error> {
        loop {
            match self.inner_s.connect() {
                Ok(_) => {
                    self.flush_write_buffer(deadline).await?;
                    return Ok(());
                }
                Err(e) => match e.into_io_error() {
                    Ok(io_e) => {
                        if io_e.kind() == std::io::ErrorKind::WouldBlock {
                            // Get more data from the underlying stream
                            if self.flush_write_buffer(deadline).await? == 0 {
                                self.fill_read_buff(deadline).await?;
                            };
                            continue;
                        }
                        return Err(io_e);
                    }
                    Err(other_e) => {
                        return Err(std::io::Error::other(other_e));
                    }
                },
            }
        }
    }

    pub async fn accept(
        &mut self,
        deadline: Option<std::time::Instant>,
    ) -> Result<(), std::io::Error> {
        loop {
            match self.inner_s.accept() {
                Ok(_) => {
                    self.flush_write_buffer(deadline).await?;
                    return Ok(());
                }
                Err(e) => match e.into_io_error() {
                    Ok(io_e) => {
                        if io_e.kind() == std::io::ErrorKind::WouldBlock {
                            // Get more data from the underlying stream
                            if self.flush_write_buffer(deadline).await? == 0 {
                                self.fill_read_buff(deadline).await?;
                            };
                            continue;
                        }
                        return Err(io_e);
                    }
                    Err(other_e) => {
                        return Err(std::io::Error::other(other_e));
                    }
                },
            }
        }
    }

    pub(crate) async fn shutdown_internal(&mut self) -> Result<ShutdownResult, std::io::Error> {
        // Flush the write buffer to the underlying stream
        self.flush_write_buffer(None).await?;
        // Shutdown the SSL stream
        // It invokes sending and then receiving.
        loop {
            match self.inner_s.shutdown() {
                Ok(state) => {
                    // No more bytes needed to be read or written
                    return Ok(state);
                }
                Err(e) => {
                    match e.into_io_error() {
                        Ok(io_e) => {
                            if io_e.kind() == std::io::ErrorKind::WouldBlock {
                                // Get more data from the underlying stream
                                if self.flush_write_buffer(None).await? == 0 {
                                    self.fill_read_buff(None).await?;
                                };
                                continue;
                            }
                            return Err(io_e);
                        }
                        Err(other_e) => {
                            return Err(std::io::Error::other(other_e));
                        }
                    }
                }
            }
        }
    }
}

impl<S: AsyncStreamRead> SslStream<S> {
    async fn fill_read_buff(&mut self, deadline: Option<std::time::Instant>) -> Result<(), Errno> {
        self.inner_s
            .get_mut()
            .fill_read_buff(&mut self.tcp, deadline)
            .await
    }

    /// Corresponds to the OpenSSL [openssl:ssl::SslStream::read] method.
    pub(crate) async fn try_read_internal(
        &mut self,
        buf: &mut [u8],
        deadline: Option<std::time::Instant>,
    ) -> Result<usize, Errno> {
        loop {
            match self.inner_s.read(buf) {
                Ok(bytes_read) => return Ok(bytes_read),
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        self.fill_read_buff(deadline).await?;
                        continue;
                    }
                    return Err(Errno::from_io_error(&e).unwrap());
                }
            }
        }
    }
}

impl<S: AsyncStreamWrite> SslStream<S> {
    async fn flush_write_buffer(
        &mut self,
        deadline: Option<std::time::Instant>,
    ) -> Result<usize, Errno> {
        // Flush the write buffer to the underlying stream
        self.inner_s
            .get_mut()
            .flush_write_buff(&mut self.tcp, deadline)
            .await
    }

    /// Corresponds to the OpenSSL [openssl:ssl::SslStream::write] method.
    pub async fn write_internal(
        &mut self,
        buf: &[u8],
        deadline: Option<std::time::Instant>,
    ) -> Result<usize, Errno> {
        loop {
            match self.inner_s.write(buf) {
                Ok(bytes_written) => {
                    // buffer flush currently always flush all the data written.
                    self.flush_write_buffer(deadline).await?;
                    return Ok(bytes_written);
                }
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        self.flush_write_buffer(deadline).await?;
                        continue;
                    }
                    return Err(Errno::from_io_error(&e).unwrap());
                }
            }
        }
    }
}

impl<S: AsyncStreamWrite + AsyncStreamRead> AsyncStreamWrite for SslStream<S> {
    async fn write<'a>(
        &'a mut self,
        buffer: &'a [u8],
        deadline: Option<std::time::Instant>,
    ) -> Result<(), Errno> {
        self.write_internal(buffer, deadline).await?;
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        self.shutdown_internal()
            .await
            .map_err(|io_err: std::io::Error| Errno::from_io_error(&io_err).unwrap())?;
        Ok(())
    }

    async fn close(&mut self) -> Result<(), Errno> {
        // Close the underlying stream
        self.tcp.close().await
    }
}

impl<S: AsyncStreamWrite + AsyncStreamRead> AsyncStreamRead for SslStream<S> {
    async fn try_read(
        &mut self,
        buffer: &mut [u8],
        deadline: Option<std::time::Instant>,
    ) -> Result<usize, Errno> {
        self.try_read_internal(buffer, deadline).await
    }

    async fn read(
        &mut self,
        buffer: &mut [u8],
        deadline: Option<std::time::Instant>,
    ) -> Result<(), Errno> {
        // fill the buffer until it is full.
        let mut total_read = 0;
        while total_read < buffer.len() {
            let bytes_read = self.try_read(&mut buffer[total_read..], deadline).await?;
            if bytes_read == 0 {
                break;
            }
            total_read += bytes_read;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::CString, os::fd::OwnedFd, time::Instant};

    use openssl::ssl::{ShutdownResult, SslAcceptor, SslConnector, SslVersion};
    use rustix_uring::Errno;

    use crate::{
        AsyncStreamRead, AsyncStreamWrite, OwnedFdStream,
        operations::spawn_task,
        pipe::bipipe,
        ssl2::SslStream,
        tlscontext::test::test_utils::{CertAndKeyFileNames, DEFAULT_SERVER_NAME},
    };

    async fn c_server(
        server_fd: rustix::fd::OwnedFd,
        cert_name: CString,
        key_name: CString,
        ca_cert_name: CString,
        crl_path: Option<CString>,
        deadline: Option<Instant>,
    ) -> Result<(), Errno> {
        crate::tlscontext::test::server(
            server_fd,
            cert_name,
            key_name,
            ca_cert_name,
            crl_path,
            deadline,
        )
        .await
    }
    async fn c_client(
        client_fd: OwnedFd,
        cert_name: CString,
        key_name: CString,
        ca_cert_name: CString,
        crl_path: Option<CString>,
        deadline: Option<Instant>,
    ) -> Result<(), Errno> {
        crate::tlscontext::test::client(
            client_fd,
            cert_name,
            key_name,
            ca_cert_name,
            crl_path,
            deadline,
        )
        .await
    }

    pub fn create_openssl_connector(certs: &CertAndKeyFileNames) -> SslConnector {
        let mut connector =
            openssl::ssl::SslConnector::builder(openssl::ssl::SslMethod::tls()).unwrap();
        connector
            .set_certificate_file(
                certs.client_cert_name.to_string_lossy().as_ref(),
                openssl::ssl::SslFiletype::PEM,
            )
            .unwrap();
        connector
            .set_private_key_file(
                certs.client_key_name.to_string_lossy().as_ref(),
                openssl::ssl::SslFiletype::PEM,
            )
            .unwrap();
        connector
            .set_ca_file(certs.ca_cert_name.to_string_lossy().as_ref())
            .unwrap();
        connector.set_verify_callback(openssl::ssl::SslVerifyMode::NONE, |ok, ctx| {
            if !ok {
                let e = ctx.error();
                println!("verify failed : {e}");
            }
            ok
        });
        connector
            .set_min_proto_version(Some(SslVersion::TLS1_2))
            .unwrap();
        connector.build()
    }

    pub fn create_openssl_acceptor(certs: &CertAndKeyFileNames) -> SslAcceptor {
        let mut acceptor =
            openssl::ssl::SslAcceptor::mozilla_intermediate_v5(openssl::ssl::SslMethod::tls())
                .unwrap();
        acceptor
            .set_certificate_file(
                certs.server_cert_name.to_string_lossy().as_ref(),
                openssl::ssl::SslFiletype::PEM,
            )
            .unwrap();
        acceptor
            .set_private_key_file(
                certs.server_key_name.to_string_lossy().as_ref(),
                openssl::ssl::SslFiletype::PEM,
            )
            .unwrap();
        acceptor
            .set_ca_file(certs.ca_cert_name.to_string_lossy().as_ref())
            .unwrap();
        acceptor.set_verify_callback(openssl::ssl::SslVerifyMode::NONE, |ok, ctx| {
            if !ok {
                let e = ctx.error();
                println!("verify failed : {e}");
            }
            ok
        });
        acceptor
            .set_min_proto_version(Some(SslVersion::TLS1_2))
            .unwrap();
        acceptor.build()
    }

    async fn rs_client(cert_and_key_file_names: &CertAndKeyFileNames, client_fd: OwnedFd) {
        let connector = create_openssl_connector(cert_and_key_file_names)
            .configure()
            .unwrap();
        let ssl = connector.into_ssl(DEFAULT_SERVER_NAME).unwrap();
        let tcp = OwnedFdStream::new(client_fd);
        let mut ssl_s = SslStream::new(ssl, tcp).unwrap();
        ssl_s.connect(None).await.unwrap();
        ssl_s.write("hello".as_bytes(), None).await.unwrap();
        let mut buf = [0; 7];
        let bytes_read = ssl_s.try_read(&mut buf, None).await.unwrap();
        assert_eq!(bytes_read, 7);
        assert_eq!(&buf[..7], "goodbye".as_bytes());

        // TODO: c impl might not have implemented receive shutdown on its end.
        assert_eq!(
            ssl_s.shutdown_internal().await.unwrap(),
            ShutdownResult::Sent
        );
        assert_eq!(
            ssl_s.shutdown_internal().await.unwrap(),
            ShutdownResult::Received
        );
    }

    async fn rs_server(
        cert_and_key_file_names: &CertAndKeyFileNames,
        server_fd: rustix::fd::OwnedFd,
    ) {
        let acceptor = create_openssl_acceptor(cert_and_key_file_names);
        let tcp = OwnedFdStream::new(server_fd);
        let ssl = openssl::ssl::Ssl::new(acceptor.context()).unwrap();
        let mut ssl_s = SslStream::new(ssl, tcp).unwrap();
        ssl_s.accept(None).await.unwrap();
        let mut message = [0; 5];
        ssl_s.try_read(&mut message, None).await.unwrap();
        assert_eq!(message, "hello".as_bytes());
        ssl_s.write("goodbye".as_bytes(), None).await.unwrap();

        let shutdown_result = ssl_s.shutdown_internal().await.unwrap();
        assert_eq!(shutdown_result, ShutdownResult::Sent);
        let shutdown_result = ssl_s.shutdown_internal().await.unwrap();
        assert_eq!(shutdown_result, ShutdownResult::Received);
    }

    async fn c_server_rs_client() {
        let cert_and_key_file_names =
            crate::tlscontext::test::test_utils::setup_default_certs().unwrap();
        let (client_fd, server_fd) = bipipe();
        let ca_cert_name_clone = cert_and_key_file_names.ca_cert_name.clone();
        let server_cert_name = cert_and_key_file_names.server_cert_name.clone();
        let server_key_name = cert_and_key_file_names.server_key_name.clone();
        spawn_task(async move {
            c_server(
                server_fd,
                server_cert_name,
                server_key_name,
                ca_cert_name_clone,
                None,
                None,
            )
            .await
            .unwrap();
        });

        rs_client(&cert_and_key_file_names, client_fd).await;
    }

    #[test]
    fn c_server_rs_client_test() {
        crate::run_test("server_client", c_server_rs_client())
    }

    async fn rs_server_rs_client() {
        let cert_and_key_file_names =
            crate::tlscontext::test::test_utils::setup_default_certs().unwrap();
        let (client_fd, server_fd) = bipipe();
        let cert_and_key_file_names_clone = cert_and_key_file_names.clone();
        spawn_task(async move {
            rs_server(&cert_and_key_file_names, server_fd).await;
        });
        rs_client(&cert_and_key_file_names_clone, client_fd).await;
    }

    #[test]
    fn rs_server_rs_client_test() {
        crate::run_test("rs_server_rs_client", rs_server_rs_client())
    }

    async fn c_client_rs_server() {
        let cert_and_key_file_names =
            crate::tlscontext::test::test_utils::setup_default_certs().unwrap();
        let (client_fd, server_fd) = bipipe();
        let ca_cert_name_clone = cert_and_key_file_names.ca_cert_name.clone();
        let client_cert_name = cert_and_key_file_names.client_cert_name.clone();
        let client_key_name = cert_and_key_file_names.client_key_name.clone();
        spawn_task(async move {
            c_client(
                client_fd,
                client_cert_name,
                client_key_name,
                ca_cert_name_clone,
                None,
                None,
            )
            .await
            .unwrap();
        });

        rs_server(&cert_and_key_file_names, server_fd).await;
    }

    #[test]
    fn c_client_rs_server_test() {
        crate::run_test("c_client_rs_server", c_client_rs_server())
    }
}
