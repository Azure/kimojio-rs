// Copyright (c) Microsoft Corporation. All rights reserved.
//! TlsContext provides an option to create server context and
//! then provides an asynchronous stream which implement TLS
//! over the stream
//!
//! `TlsContext::server_ctx` creates a TLS "server" context.
//! `TlsContext::client_ctx` creates a TLS "client" context.
//! `server` creates a TLS "server" socket.
//! `client` creates a TLS "client" socket.
//!
//! TODO: implement client authentication
//!
use crate::{Errno, OwnedFd, operations, tlsstream::TlsStream, tracing::Events};
use kimojio_tls::{TlsServerContext, TlsServerError};
use std::time::Instant;

pub struct TlsContext {
    ssl_ctx: TlsServerContext,
}

impl TlsContext {
    /// Creates a TlsContext from an OpenSSL crate SslContext.
    /// This will replace the client and server context creation methods in the future.
    /// TODO: KTLS option is not set compared to the original c wrapper implementation.
    pub fn from_openssl(ctx: openssl::ssl::SslContext) -> Self {
        use foreign_types_shared::ForeignType;
        let ssl_ctx = TlsServerContext::from_raw(ctx.as_ptr() as *mut std::ffi::c_void);
        // Transfer the ownership of the openssl wrapper to TlsContext
        std::mem::forget(ctx);
        Self { ssl_ctx }
    }

    /// Creates a TLS "server" socket
    pub async fn server(
        &self,
        bufsize: usize,
        socket: OwnedFd,
        deadline: Option<Instant>,
    ) -> Result<TlsStream, Errno> {
        let ssl = self.ssl_ctx.server(bufsize).map_err(as_io_error)?;
        let mut server = TlsStream::new_tlsstream(ssl, socket);
        server.server_side_handshake(deadline).await?;
        Ok(server)
    }

    /// Creates a TLS "client" socket.
    pub async fn client(
        &self,
        bufsize: usize,
        socket: OwnedFd,
        deadline: Option<Instant>,
    ) -> Result<TlsStream, Errno> {
        let ssl = self.ssl_ctx.client(bufsize).map_err(as_io_error)?;
        let mut client = TlsStream::new_tlsstream(ssl, socket);
        client.client_side_handshake(deadline).await?;
        Ok(client)
    }
}

pub(crate) fn as_io_error(result: TlsServerError) -> Errno {
    let activity_id = operations::get_activity_id();
    match result {
        TlsServerError::Errno(code) => {
            let raw_code: i32 = code.raw_os_error();
            let raw_code = if raw_code < 0 {
                (-raw_code) as u64
            } else {
                raw_code as u64
            };
            operations::write_event(Events::TlsError {
                code: raw_code,
                activity_id,
            });
            code
        }
        TlsServerError::TlsError(codes) => {
            for code in codes {
                operations::write_event(Events::TlsError { code, activity_id })
            }
            Errno::from_raw_os_error(crate::EPROTO)
        }
    }
}

#[cfg(test)]
pub(crate) mod test {
    use std::ffi::CString;
    use std::io::IoSlice;
    use std::net::ToSocketAddrs;
    use std::rc::Rc;
    use std::time::Duration;
    use std::time::Instant;

    use crate::AsyncEvent;
    use crate::Errno;
    use crate::OwnedFd;
    use crate::SplittableStream;
    use crate::operations;
    use crate::operations::spawn_task;
    use crate::pipe::bipipe;
    use crate::socket_helpers::create_client_socket;
    use crate::socket_helpers::create_server_socket;
    use crate::socket_helpers::update_accept_socket;
    use crate::tlscontext::test::test_utils::CertAndKeyFileNames;
    use crate::tlsstream::TlsStream;
    use crate::{AsyncStreamRead, AsyncStreamWrite};
    use futures::select;
    use test_utils::create_certs;
    use test_utils::create_crl_for_cert;
    use test_utils::create_self_signed_cert;
    use test_utils::default_admin_name;
    use test_utils::default_server_name;
    use test_utils::delete_cert_and_key_files;
    use test_utils::setup_default_certs;

    #[test]
    fn test_tls_overhead() {
        assert!(crate::tlsstream::tls_overhead(16384) > 0);
    }

    #[test]
    fn test_tls_timeout_on_write_with_pending_read() {
        crate::run_test("test_tls_timeout_on_write_with_pending_read", async {
            let cert_and_key_file_names = setup_default_certs().unwrap();

            let done = Rc::new(AsyncEvent::new());
            let server_socket = create_server_socket(9001).await.unwrap();
            let server_task = spawn_task(server_listener(
                server_socket,
                cert_and_key_file_names.clone(),
                done.clone(),
            ));

            let addr = ("localhost", 9001)
                .to_socket_addrs()
                .unwrap()
                .next()
                .unwrap();
            let client_socket = create_client_socket(&addr).await.unwrap();
            let client_ctx = test_utils::make_test_client_ctx(
                cert_and_key_file_names.client_cert_name.as_c_str(),
                cert_and_key_file_names.client_key_name.as_c_str(),
                cert_and_key_file_names.ca_cert_name.as_c_str(),
                None,
            )
            .unwrap();
            let client = client_ctx.client(16384, client_socket, None).await.unwrap();
            let (mut read, mut write) = client.split().await.unwrap();

            let pending_read = operations::spawn_task(async move {
                let mut read_buf = [0u8; 5];
                read.read(&mut read_buf, None).await
            });

            // ensure previous task has a chance to run and block on read
            operations::yield_io().await;

            let one_second_earlier = Instant::now() - Duration::from_secs(1);
            let result = write.write(b"hello", Some(one_second_earlier)).await;
            assert!(result.is_err(), "Expecting timeout error");
            done.set();
            pending_read.abort();
            server_task.abort();
        })
    }

    #[test]
    fn test_echo_example() {
        crate::run_test("test_echo_example", test());
    }

    async fn test() {
        let cert_and_key_file_names = setup_default_certs().unwrap();

        let done = Rc::new(AsyncEvent::new());
        let server_socket = create_server_socket(8999).await.unwrap();
        let server_task = spawn_task(server_listener(
            server_socket,
            cert_and_key_file_names.clone(),
            done.clone(),
        ));
        let client_task1 = spawn_task(client_connector(
            cert_and_key_file_names.clone(),
            client_loop_split,
        ));
        let client_task2 = spawn_task(client_connector(cert_and_key_file_names, client_loop));
        client_task1.await.unwrap();
        client_task2.await.unwrap();
        done.set();
        server_task.await.unwrap();
    }

    async fn client_connector(
        cert_and_key_file_names: CertAndKeyFileNames,
        client_loop: impl AsyncFnOnce(TlsStream),
    ) {
        let addr = ("localhost", 8999)
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap();
        let client_socket = create_client_socket(&addr).await.unwrap();
        let client_ctx = test_utils::make_test_client_ctx(
            cert_and_key_file_names.client_cert_name.as_c_str(),
            cert_and_key_file_names.client_key_name.as_c_str(),
            cert_and_key_file_names.ca_cert_name.as_c_str(),
            None,
        )
        .unwrap();
        let client_socket = client_ctx.client(16384, client_socket, None).await.unwrap();
        client_loop(client_socket).await;
    }

    async fn client_loop_split(stream: TlsStream) {
        let (mut read, mut write) = stream.split().await.unwrap();

        // large
        let message = vec![42u8; 1024 * 1024];
        write.write(&message, None).await.unwrap();
        let mut buffer = vec![0u8; 1024 * 1024];
        read.read(&mut buffer, None).await.unwrap();

        // try
        let buf1 = [42u8; 512];
        let buf2 = [43u8; 512];
        write
            .writev(&mut [IoSlice::new(&buf1), IoSlice::new(&buf2)], None)
            .await
            .unwrap();
        let mut response = [0u8; 1024];
        let mut amount_read = 0;
        while amount_read < 512 {
            amount_read += read
                .try_read(&mut response[amount_read..], None)
                .await
                .unwrap();
        }
        read.read(&mut response[512..], None).await.unwrap();
        assert_eq!(&response[..512], [42u8; 512]);
        assert_eq!(&response[512..], [43u8; 512]);

        write.shutdown().await.unwrap();
        write.close().await.unwrap();
    }

    async fn client_loop(mut stream: TlsStream) {
        // large
        let message = vec![42u8; 1024 * 1024];
        stream.write(&message, None).await.unwrap();
        let mut buffer = vec![0u8; 1024 * 1024];
        stream.read(&mut buffer, None).await.unwrap();

        // try
        let buf1 = [42u8; 512];
        let buf2 = [43u8; 512];
        stream
            .writev(&mut [IoSlice::new(&buf1), IoSlice::new(&buf2)], None)
            .await
            .unwrap();
        let mut response = [0u8; 1024];
        let mut amount_read = 0;
        while amount_read < 512 {
            amount_read += stream
                .try_read(&mut response[amount_read..], None)
                .await
                .unwrap();
        }
        stream.read(&mut response[512..], None).await.unwrap();
        assert_eq!(&response[..512], [42u8; 512]);
        assert_eq!(&response[512..], [43u8; 512]);

        stream.shutdown().await.unwrap();
        stream.close().await.unwrap();
    }

    async fn server_listener(
        server_socket: OwnedFd,
        cert_and_key_file_names: CertAndKeyFileNames,
        done: Rc<AsyncEvent>,
    ) {
        let mut tasks = vec![];
        loop {
            let client_socket = select! {
                client_socket = crate::operations::accept(&server_socket) => client_socket.unwrap(),
                _ = done.wait() => break,
            };
            update_accept_socket(&client_socket).unwrap();
            tasks.push(spawn_task(server_loop(
                client_socket,
                cert_and_key_file_names.clone(),
            )));
        }

        for task in tasks {
            task.await.unwrap();
        }
    }

    async fn server_loop(client_socket: OwnedFd, cert_and_key_file_names: CertAndKeyFileNames) {
        let server_ctx = test_utils::make_test_server_ctx(
            cert_and_key_file_names.server_cert_name.as_c_str(),
            cert_and_key_file_names.server_key_name.as_c_str(),
            cert_and_key_file_names.ca_cert_name.as_c_str(),
            None,
        )
        .unwrap();

        let server = server_ctx.server(16384, client_socket, None).await.unwrap();
        let (mut read, mut write) = server.split().await.unwrap();

        let mut buffer = [0; 8192];
        loop {
            let amount = read.try_read(&mut buffer, None).await.unwrap();
            if amount == 0 {
                break; // EOF
            }
            write.write(&buffer[..amount], None).await.unwrap();
        }
        write.shutdown().await.unwrap();
        write.close().await.unwrap();
    }

    #[test]
    fn test_tls_version_restriction() {
        crate::run_test(
            "test_tls_version_restriction",
            test_tls_version_restriction_async(),
        )
    }

    async fn test_tls_version_restriction_async() {
        let cert_and_key_file_names = setup_default_certs().unwrap();

        // Test server context
        let server_ctx = test_utils::make_test_server_ctx(
            cert_and_key_file_names.server_cert_name.as_c_str(),
            cert_and_key_file_names.server_key_name.as_c_str(),
            cert_and_key_file_names.ca_cert_name.as_c_str(),
            None,
        )
        .unwrap();

        // TLS 1.3 version is 0x0304
        let min_version = server_ctx.ssl_ctx.get_min_proto_version();
        assert_eq!(
            min_version, 0x0304,
            "Server context minimum TLS version should be TLS 1.3 (0x0304)"
        );

        // Test client context
        let client_ctx = test_utils::make_test_client_ctx(
            cert_and_key_file_names.client_cert_name.as_c_str(),
            cert_and_key_file_names.client_key_name.as_c_str(),
            cert_and_key_file_names.ca_cert_name.as_c_str(),
            None,
        )
        .unwrap();

        let min_version = client_ctx.ssl_ctx.get_min_proto_version();
        assert_eq!(
            min_version, 0x0304,
            "Client context minimum TLS version should be TLS 1.3 (0x0304)"
        );
    }

    #[test]
    fn client_server_test() {
        crate::run_test("client_server_test", client_server_test_async())
    }
    #[test]
    fn client_handshake_crl_test() {
        crate::run_test(
            "client_handshake_crl_test_async",
            client_handshake_crl_test_async(),
        )
    }
    #[test]
    fn server_handshake_crl_test() {
        crate::run_test(
            "server_handshake_crl_test_async",
            server_handshake_crl_test_async(),
        )
    }

    async fn client_handshake_crl_test_async() {
        use test_utils::{delete_client_crl_directory, setup_client_crl_directory};

        let (client_fd, server_fd) = bipipe();
        let cert_and_key_file_names = create_certs(
            "client_handshake_crl_test",
            &default_server_name(),
            &default_admin_name(),
        )
        .expect("error creating cert and key files");
        let client_crl_dir = setup_client_crl_directory().unwrap();

        // Set up cleanup that will run even if the test panics
        let _guard = scopeguard::guard((), move |_| {
            // Clean up in reverse order of creation
            let _ = delete_client_crl_directory();
        });

        // Extract certificate paths for CRL creation
        let ca_cert_file = cert_and_key_file_names
            .ca_cert_name
            .to_str()
            .expect("Invalid CA cert filename");
        let ca_key_file = cert_and_key_file_names
            .ca_key_name
            .to_str()
            .expect("Invalid CA key filename");
        let server_cert_file = cert_and_key_file_names
            .server_cert_name
            .to_str()
            .expect("Invalid server cert filename");
        let crl_dir_str = client_crl_dir
            .to_str()
            .expect("Invalid CRL directory")
            .to_string();

        // Create CRL revoking the server certificate
        create_crl_for_cert(ca_cert_file, ca_key_file, server_cert_file, &crl_dir_str).unwrap();

        // Get the CRL file path based on CA certificate hash
        let expected_crl_path = format!("{crl_dir_str}/crl.pem");

        // Verify CRL file exists
        assert!(
            std::path::Path::new(&expected_crl_path).exists(),
            "CRL file not found at expected location: {expected_crl_path}"
        );

        // Clone necessary certificate paths
        let server_cert_name = cert_and_key_file_names.server_cert_name.clone();
        let server_key_name = cert_and_key_file_names.server_key_name.clone();
        let ca_cert_name = cert_and_key_file_names.ca_cert_name.clone();
        let crl_path = expected_crl_path.clone();
        let server_cert_name_clone = cert_and_key_file_names.server_cert_name.clone();

        spawn_task(async move {
            server(
                server_fd,
                server_cert_name,
                server_key_name,
                ca_cert_name,
                None, // CRL checking is not enabled on the server side
                None,
            )
            .await
            .expect_err("Expecting protocol error");
        });

        let result = client(
            client_fd,
            cert_and_key_file_names.client_cert_name,
            cert_and_key_file_names.client_key_name,
            server_cert_name_clone,
            Some(CString::new(crl_path).unwrap()),
            None,
        )
        .await;
        assert!(
            result.is_err(),
            "Expected client handshake to fail due to revoked server certificate"
        );
    }

    async fn server_handshake_crl_test_async() {
        use test_utils::{delete_server_crl_directory, setup_server_crl_directory};

        let (client_fd, server_fd) = bipipe();
        let cert_and_key_file_names = create_certs(
            "server_handshake_crl_test",
            &default_server_name(),
            &default_admin_name(),
        )
        .expect("error creating cert and key files");
        let server_crl_dir = setup_server_crl_directory().unwrap();

        // Set up cleanup that will run even if the test panics
        let _guard = scopeguard::guard((), move |_| {
            // Clean up in reverse order of creation
            let _ = delete_server_crl_directory();
        });

        // Extract certificate paths for CRL creation
        let ca_cert_file = cert_and_key_file_names
            .ca_cert_name
            .to_str()
            .expect("Invalid CA cert filename");
        let ca_key_file = cert_and_key_file_names
            .ca_key_name
            .to_str()
            .expect("Invalid CA key filename");
        let client_cert_file = cert_and_key_file_names
            .client_cert_name
            .to_str()
            .expect("Invalid client cert filename");
        let crl_dir_str = server_crl_dir
            .to_str()
            .expect("Invalid CRL directory")
            .to_string();

        // Create CRL revoking the client certificate
        create_crl_for_cert(ca_cert_file, ca_key_file, client_cert_file, &crl_dir_str).unwrap();
        let expected_crl_path = format!("{crl_dir_str}/crl.pem");

        // Verify CRL file exists
        assert!(
            std::path::Path::new(&expected_crl_path).exists(),
            "CRL file not found at expected location: {expected_crl_path}"
        );

        // Clone necessary certificate paths
        let server_cert_name = cert_and_key_file_names.server_cert_name.clone();
        let server_key_name = cert_and_key_file_names.server_key_name.clone();
        let ca_cert_name = cert_and_key_file_names.ca_cert_name.clone();
        let crl_path = expected_crl_path.clone();
        let server_cert_name_clone = cert_and_key_file_names.server_cert_name.clone();

        // Start server with CRL checking enabled
        spawn_task(async move {
            server(
                server_fd,
                server_cert_name,
                server_key_name,
                ca_cert_name,
                Some(CString::new(crl_path).unwrap()),
                None,
            )
            .await
            .unwrap();
            //TODO: crl test has protocol error here needs fixing.
        });

        client(
            client_fd,
            cert_and_key_file_names.client_cert_name,
            cert_and_key_file_names.client_key_name,
            server_cert_name_clone,
            None,
            None,
        )
        .await
        .expect_err("Expecting protocol error");
    }

    pub(crate) async fn client(
        client_fd: OwnedFd,
        cert_name: CString,
        key_name: CString,
        ca_cert_name: CString,
        crl_path: Option<CString>,
        deadline: Option<Instant>,
    ) -> Result<(), Errno> {
        let bufsize = 16384;
        let tls_context = test_utils::make_test_client_ctx(
            cert_name.as_c_str(),
            key_name.as_c_str(),
            ca_cert_name.as_c_str(),
            crl_path.as_deref(),
        )
        .map_err(|e| {
            eprintln!("Error creating client context: {e:?}");
            Errno::INVAL
        })?;
        let mut stream = tls_context.client(bufsize, client_fd, deadline).await?;

        stream.write("hello".as_bytes(), None).await.unwrap();
        let mut response = [0; 7];
        stream.read(&mut response, deadline).await?;
        assert_eq!(response, "goodbye".as_bytes());

        stream.shutdown().await.unwrap();

        Ok(())
    }

    pub(crate) async fn server(
        server_fd: rustix::fd::OwnedFd,
        cert_name: CString,
        key_name: CString,
        ca_cert_name: CString,
        crl_path: Option<CString>,
        deadline: Option<Instant>,
    ) -> Result<(), Errno> {
        let bufsize = 16384;
        let tls_context = test_utils::make_test_server_ctx(
            cert_name.as_c_str(),
            key_name.as_c_str(),
            ca_cert_name.as_c_str(),
            crl_path.as_deref(),
        )
        .map_err(|e| {
            eprintln!("Error creating server context: {e:?}");
            Errno::INVAL
        })?;
        let mut stream = tls_context.server(bufsize, server_fd, deadline).await?;

        let mut message = [0; 5];
        stream.read(&mut message, deadline).await.unwrap();
        assert_eq!(message, "hello".as_bytes());
        stream.write("goodbye".as_bytes(), None).await.unwrap();

        stream.shutdown().await.unwrap();

        Ok(())
    }

    async fn client_server_test_async() {
        let cert_and_key_file_names = setup_default_certs().unwrap();
        let (client_fd, server_fd) = bipipe();
        let ca_cert_name_clone = cert_and_key_file_names.ca_cert_name.clone();
        spawn_task(async {
            server(
                server_fd,
                cert_and_key_file_names.server_cert_name,
                cert_and_key_file_names.server_key_name,
                cert_and_key_file_names.ca_cert_name,
                None,
                None,
            )
            .await
            .unwrap()
        });
        client(
            client_fd,
            cert_and_key_file_names.client_cert_name,
            cert_and_key_file_names.client_key_name,
            ca_cert_name_clone,
            None,
            None,
        )
        .await
        .unwrap();
    }

    #[test]
    fn test_incorrect_ca_cert_for_server_authentication() {
        crate::run_test(
            "use_incorrect_ca_cert_for_server_authentication",
            use_incorrect_ca_cert_for_server_authentication(),
        )
    }

    async fn use_incorrect_ca_cert_for_server_authentication() {
        let cert_and_key_file_names = setup_default_certs().unwrap();
        let (client_fd, server_fd) = bipipe();
        let server_cert_name_clone = cert_and_key_file_names.server_cert_name.clone();
        spawn_task(async {
            server(
                server_fd,
                cert_and_key_file_names.server_cert_name,
                cert_and_key_file_names.server_key_name,
                cert_and_key_file_names.ca_cert_name,
                None,
                None,
            )
            .await
            .unwrap()
        });

        // start client with an incorrect ca cert.
        client(
            client_fd,
            cert_and_key_file_names.client_cert_name,
            cert_and_key_file_names.client_key_name,
            server_cert_name_clone,
            None,
            None,
        )
        .await
        .expect_err("Expecting protocol error");
    }

    #[test]
    fn test_incorrect_ca_cert_for_client_authentication() {
        crate::run_test(
            "use_incorrect_ca_cert_for_client_authentication",
            use_incorrect_ca_cert_for_client_authentication(),
        )
    }

    async fn use_incorrect_ca_cert_for_client_authentication() {
        let cert_and_key_file_names = setup_default_certs().unwrap();
        let (client_fd, server_fd) = bipipe();
        let server_cert_name_clone = cert_and_key_file_names.server_cert_name.clone();
        // start server with an incorrect ca cert.
        spawn_task(async {
            server(
                server_fd,
                cert_and_key_file_names.server_cert_name,
                cert_and_key_file_names.server_key_name,
                server_cert_name_clone,
                None,
                None,
            )
            .await
            .expect_err("Expecting protocol error");
        });

        client(
            client_fd,
            cert_and_key_file_names.client_cert_name,
            cert_and_key_file_names.client_key_name,
            cert_and_key_file_names.ca_cert_name,
            None,
            None,
        )
        .await
        .expect_err("Expecting broken pipe");
    }

    #[test]
    fn test_server_certificate_not_signed_by_ca() {
        crate::run_test(
            "use_server_certificate_not_signed_by_ca",
            use_server_certificate_not_signed_by_ca(),
        )
    }

    async fn use_server_certificate_not_signed_by_ca() {
        let cert_and_key_file_names = setup_default_certs().unwrap();
        let (client_fd, server_fd) = bipipe();
        let (server_cert_name, server_key_name) = create_self_signed_cert(
            "use_server_certificate_not_signed_by_ca".to_string(),
            Vec::new(),
        )
        .unwrap();

        let server_cert_name_clone = server_cert_name.clone();
        let server_key_name_clone = server_key_name.clone();
        let ca_cert_name_clone = cert_and_key_file_names.ca_cert_name.clone();
        // use self signed server cert instead of that signed by the CA.
        spawn_task(async {
            server(
                server_fd,
                server_cert_name,
                server_key_name,
                cert_and_key_file_names.ca_cert_name,
                None,
                None,
            )
            .await
            .unwrap();
        });

        match client(
            client_fd,
            cert_and_key_file_names.client_cert_name,
            cert_and_key_file_names.client_key_name,
            ca_cert_name_clone,
            None,
            None,
        )
        .await
        {
            Ok(_) => {
                delete_cert_and_key_files(server_cert_name_clone, server_key_name_clone);
                panic!("Expecting protocol error")
            }
            Err(_) => delete_cert_and_key_files(server_cert_name_clone, server_key_name_clone),
        }
    }

    #[test]
    fn test_client_certificate_not_signed_by_ca() {
        crate::run_test(
            "use_client_certificate_not_signed_by_ca",
            use_client_certificate_not_signed_by_ca(),
        )
    }

    async fn use_client_certificate_not_signed_by_ca() {
        let cert_and_key_file_names = setup_default_certs().unwrap();
        let (client_fd, server_fd) = bipipe();
        let ca_cert_name_clone = cert_and_key_file_names.ca_cert_name.clone();
        spawn_task(async {
            server(
                server_fd,
                cert_and_key_file_names.server_cert_name,
                cert_and_key_file_names.server_key_name,
                cert_and_key_file_names.ca_cert_name,
                None,
                None,
            )
            .await
            .expect_err("Expecting protocol error");
        });
        let (client_cert_name, client_key_name) = create_self_signed_cert(
            "use_client_certificate_not_signed_by_ca".to_string(),
            Vec::new(),
        )
        .unwrap();
        // use self signed client cert instead of that signed by the CA.
        match client(
            client_fd,
            client_cert_name.clone(),
            client_key_name.clone(),
            ca_cert_name_clone,
            None,
            None,
        )
        .await
        {
            Ok(_) => {
                delete_cert_and_key_files(client_cert_name, client_key_name);
                panic!("Expecting broken pipe")
            }
            Err(_) => delete_cert_and_key_files(client_cert_name, client_key_name),
        }
    }

    #[test]
    fn test_iterate_certificate_san() {
        crate::run_test("test_iterate_certificate_san", async {
            let (client_fd, server_fd) = bipipe();
            let cert_and_key_file_names = create_certs(
                "use_valid_admin_name_at_the_client",
                // Use the server name at the server
                &default_server_name(),
                // Use the admin name at the client. That is, the client is an
                // admin entity, say the gateway.
                &default_admin_name(),
            )
            .expect("error creating cert and key files");

            spawn_task({
                let ca_cert_name = cert_and_key_file_names.ca_cert_name.clone();
                let cert_name = cert_and_key_file_names.server_cert_name.clone();
                let key_name = cert_and_key_file_names.server_key_name.clone();

                async move {
                    let bufsize = 16384;
                    let tls_context = test_utils::make_test_server_ctx(
                        cert_name.as_c_str(),
                        key_name.as_c_str(),
                        ca_cert_name.as_c_str(),
                        None,
                    )
                    .unwrap();
                    let mut stream = tls_context.server(bufsize, server_fd, None).await.unwrap();

                    // Use openssl crate
                    {
                        let ssl = stream.get_ssl();
                        let certs = ssl.peer_certificate().unwrap();
                        let sans: Vec<String> = certs
                            .subject_alt_names()
                            .unwrap()
                            .into_iter()
                            .filter_map(|name| name.dnsname().map(String::from))
                            .collect();
                        assert_eq!(&sans, &["admin.unit.tests".to_string()]);
                    }

                    let mut message = [0; 5];
                    stream.read(&mut message, None).await.unwrap();
                    assert_eq!(message, "hello".as_bytes());
                    stream.write("goodbye".as_bytes(), None).await.unwrap();

                    stream.shutdown().await.unwrap();
                }
            });

            client(
                client_fd,
                cert_and_key_file_names.client_cert_name,
                cert_and_key_file_names.client_key_name,
                cert_and_key_file_names.ca_cert_name,
                None,
                None,
            )
            .await
            .unwrap();
        })
    }

    pub(crate) mod test_utils {

        use std::ffi::CStr;
        use std::{ffi::CString, fs::File, io::Write, process::Command};

        use openssl::error::ErrorStack;
        use rcgen::{
            BasicConstraints, Certificate, CertificateParams, DnType, IsCa, Issuer, KeyPair,
            KeyUsagePurpose,
        };
        use rustix::io_uring::{Mode, OFlags};
        use std::fs;
        use std::path::Path;
        use uuid::{Uuid, fmt::Hyphenated};

        use crate::tlscontext::TlsContext;

        const DEFAULT_CERT_LOCATION: &str = "/tmp/";
        const DEFAULT_CRL_SERVER_DIR: &str = "/tmp/kimoji_server_crl";
        const DEFAULT_CRL_CLIENT_DIR: &str = "/tmp/kimoji_client_crl";
        const DEFAULT_CLIENT_NAME_SUFFIX: &str = "client.unit.tests";
        pub(crate) const DEFAULT_SERVER_NAME: &str = "server.unit.tests";
        const DEFAULT_ADMIN_NAME: &str = "admin.unit.tests";
        const DEFAULT_SERVER_CERT_FILE: &str = "/tmp/server_kimojiotests.crt";
        const DEFAULT_SERVER_KEY_FILE: &str = "/tmp/server_kimojiotests.key";
        const DEFAULT_CLIENT_CERT_FILE: &str = "/tmp/client_kimojiotests.crt";
        const DEFAULT_CLIENT_KEY_FILE: &str = "/tmp/client_kimojiotests.key";
        const DEFAULT_CA_CERT_FILE: &str = "/tmp/ca_kimojiotests.crt";
        const DEFAULT_CA_KEY_FILE: &str = "/tmp/ca_kimojiotests.key";
        const DEFAULT_LOCK_FILE: &str = "/tmp/kimojiotests.lock";

        /// Given SSL certificate, creates a client context.
        /// If `crl_path` is provided, it enables certificate revocation checking
        /// using CRLs stored in the specified directory.
        /// This function is kept for backward compatibility. The building ctx code
        /// will be moved to application layer.
        pub fn make_test_client_ctx(
            cert_name: &CStr,
            key_name: &CStr,
            ca_cert_name: &CStr,
            crl_path: Option<&CStr>,
        ) -> Result<TlsContext, ErrorStack> {
            let mut connector =
                openssl::ssl::SslConnector::builder(openssl::ssl::SslMethod::tls_client())?;
            connector.set_certificate_file(
                cert_name.to_str().unwrap(),
                openssl::ssl::SslFiletype::PEM,
            )?;
            connector
                .set_private_key_file(key_name.to_str().unwrap(), openssl::ssl::SslFiletype::PEM)?;
            connector.set_ca_file(ca_cert_name.to_str().unwrap())?;
            connector.set_verify_callback(
                openssl::ssl::SslVerifyMode::PEER
                    | openssl::ssl::SslVerifyMode::FAIL_IF_NO_PEER_CERT,
                |ok, _ctx| ok,
            );
            connector.set_min_proto_version(Some(openssl::ssl::SslVersion::TLS1_3))?;
            if let Some(crl_path) = crl_path {
                let store_builder = connector.cert_store_mut();
                let lookup_ref =
                    store_builder.add_lookup(openssl::x509::store::X509Lookup::file())?;
                lookup_ref
                    .load_crl_file(crl_path.to_str().unwrap(), openssl::ssl::SslFiletype::PEM)?;
                store_builder.set_flags(
                    openssl::x509::verify::X509VerifyFlags::CRL_CHECK
                        | openssl::x509::verify::X509VerifyFlags::CRL_CHECK_ALL,
                )?;
                // TODO: add ca list.
            }
            connector.check_private_key()?;

            let ctx = connector.build().into_context();
            Ok(TlsContext::from_openssl(ctx))
        }

        /// Given SSL certificate and key, creates a server context.
        ///
        /// If `crl_path` is provided, it enables certificate revocation checking
        /// using CRLs stored in the specified directory.
        /// This function is kept for backward compatibility. The building ctx code
        /// will be moved to application layer.
        pub fn make_test_server_ctx(
            cert_name: &CStr,
            key_name: &CStr,
            ca_cert_name: &CStr,
            crl_path: Option<&CStr>,
        ) -> Result<TlsContext, ErrorStack> {
            let mut acceptor = openssl::ssl::SslAcceptor::mozilla_intermediate_v5(
                openssl::ssl::SslMethod::tls_server(),
            )?;
            acceptor.set_certificate_file(
                cert_name.to_str().unwrap(),
                openssl::ssl::SslFiletype::PEM,
            )?;
            acceptor
                .set_private_key_file(key_name.to_str().unwrap(), openssl::ssl::SslFiletype::PEM)?;
            acceptor.set_ca_file(ca_cert_name.to_str().unwrap())?;
            acceptor.set_verify_callback(
                openssl::ssl::SslVerifyMode::PEER
                    | openssl::ssl::SslVerifyMode::FAIL_IF_NO_PEER_CERT,
                |ok, _ctx| ok,
            );
            acceptor.set_min_proto_version(Some(openssl::ssl::SslVersion::TLS1_3))?;
            if let Some(crl_path) = crl_path {
                let store_builder = acceptor.cert_store_mut();
                let lookup_ref =
                    store_builder.add_lookup(openssl::x509::store::X509Lookup::file())?;
                lookup_ref
                    .load_crl_file(crl_path.to_str().unwrap(), openssl::ssl::SslFiletype::PEM)?;
                store_builder.set_flags(
                    openssl::x509::verify::X509VerifyFlags::CRL_CHECK
                        | openssl::x509::verify::X509VerifyFlags::CRL_CHECK_ALL,
                )?;
                // TODO: add ca list.
            }
            acceptor.check_private_key()?;

            let ctx = acceptor.build().into_context();
            Ok(TlsContext::from_openssl(ctx))
        }

        pub fn setup_server_crl_directory() -> std::io::Result<CString> {
            let dir_path = Path::new(DEFAULT_CRL_SERVER_DIR);
            if !dir_path.exists() {
                fs::create_dir_all(dir_path)?;
            }
            Ok(CString::new(DEFAULT_CRL_SERVER_DIR).unwrap())
        }

        pub fn setup_client_crl_directory() -> std::io::Result<CString> {
            let dir_path = Path::new(DEFAULT_CRL_CLIENT_DIR);
            if !dir_path.exists() {
                fs::create_dir_all(dir_path)?;
            }
            Ok(CString::new(DEFAULT_CRL_CLIENT_DIR).unwrap())
        }

        pub fn delete_server_crl_directory() -> std::io::Result<()> {
            let dir_path = Path::new(DEFAULT_CRL_SERVER_DIR);
            if dir_path.exists() {
                fs::remove_dir_all(dir_path)?;
            }
            Ok(())
        }

        pub fn delete_client_crl_directory() -> std::io::Result<()> {
            let dir_path = Path::new(DEFAULT_CRL_CLIENT_DIR);
            if dir_path.exists() {
                fs::remove_dir_all(dir_path)?;
            }
            Ok(())
        }

        pub fn create_crl_for_cert(
            ca_cert_file: &str,
            ca_key_file: &str,
            revoked_cert_file: &str,
            crl_dir_str: &str,
        ) -> std::io::Result<()> {
            // Prepare CRL directory and files
            let index_file = format!("{crl_dir_str}/index.txt");
            let serial_file = format!("{crl_dir_str}/serial");
            let crlnumber_file = format!("{crl_dir_str}/crlnumber");
            let openssl_conf = format!("{crl_dir_str}/openssl.cnf");

            // Create necessary files
            File::create(&index_file)?;
            let mut serial = File::create(&serial_file)?;
            serial.write_all(b"01")?;
            let mut crlnumber = File::create(&crlnumber_file)?;
            crlnumber.write_all(b"01")?;

            // Write minimal openssl.cnf
            let conf = format!(
                r#"
[ ca ]
default_ca = CA_default

[ CA_default ]
database = {index_file}
serial = {serial_file}
crlnumber = {crlnumber_file}
default_md = sha256
default_crl_days = 30
private_key = {ca_key_file}
certificate = {ca_cert_file}
"#
            );
            let mut conf_file = File::create(&openssl_conf)?;
            conf_file.write_all(conf.as_bytes())?;

            if !std::path::Path::new(revoked_cert_file).exists() {
                return Err(std::io::Error::other(format!(
                    "Certificate file does not exist: {revoked_cert_file}"
                )));
            }

            // Use openssl ca -revoke to revoke the certificate and update index.txt
            let status = Command::new("openssl")
                .args([
                    "ca",
                    "-revoke",
                    revoked_cert_file,
                    "-keyfile",
                    ca_key_file,
                    "-cert",
                    ca_cert_file,
                    "-config",
                    &openssl_conf,
                ])
                .current_dir(crl_dir_str)
                .status()?;
            if !status.success() {
                return Err(std::io::Error::other("Failed to revoke certificate"));
            }

            // Generate CRL
            let crl_file = format!("{crl_dir_str}/crl.pem");
            let status = Command::new("openssl")
                .args([
                    "ca",
                    "-gencrl",
                    "-keyfile",
                    ca_key_file,
                    "-cert",
                    ca_cert_file,
                    "-out",
                    &crl_file,
                    "-config",
                    &openssl_conf,
                ])
                .current_dir(crl_dir_str)
                .status()?;
            if !status.success() {
                return Err(std::io::Error::other("Failed to generate CRL"));
            }

            // validate the CRL has entry of revoked certificate
            let output = Command::new("openssl")
                .args(["crl", "-in", &crl_file, "-noout", "-text"])
                .output()?;
            let crl_text = String::from_utf8_lossy(&output.stdout);
            assert!(
                crl_text.contains("Revoked Certificates"),
                "CRL should contain revoked certificates"
            );

            Ok(())
        }

        #[derive(Clone)]
        pub struct CertAndKeyFileNames {
            pub server_cert_name: CString,
            pub server_key_name: CString,
            pub client_cert_name: CString,
            pub client_key_name: CString,
            pub ca_cert_name: CString,
            pub ca_key_name: CString,
        }

        impl Default for CertAndKeyFileNames {
            fn default() -> Self {
                CertAndKeyFileNames {
                    server_cert_name: CString::new(DEFAULT_SERVER_CERT_FILE).unwrap(),
                    server_key_name: CString::new(DEFAULT_SERVER_KEY_FILE).unwrap(),
                    client_cert_name: CString::new(DEFAULT_CLIENT_CERT_FILE).unwrap(),
                    client_key_name: CString::new(DEFAULT_CLIENT_KEY_FILE).unwrap(),
                    ca_cert_name: CString::new(DEFAULT_CA_CERT_FILE).unwrap(),
                    ca_key_name: CString::new(DEFAULT_CA_KEY_FILE).unwrap(),
                }
            }
        }

        fn with_locked_file(
            lock_file: &str,
            f: impl FnOnce() -> std::io::Result<()>,
        ) -> std::io::Result<()> {
            let fd = rustix::fs::open(
                lock_file,
                OFlags::CREATE | OFlags::TRUNC | OFlags::RDWR,
                Mode::from_raw_mode(0o666),
            )?;
            rustix::fs::flock(&fd, rustix::fs::FlockOperation::LockExclusive)?;
            let result = f();
            rustix::fs::flock(&fd, rustix::fs::FlockOperation::Unlock)?;
            result
        }

        pub fn setup_default_certs() -> std::io::Result<CertAndKeyFileNames> {
            // To avoid race conditions in unit tests, create a lock file and exclusively
            // lock it while generating certs. This is to avoid a race where the cert
            // is created by one test while the key is from another.
            with_locked_file(DEFAULT_LOCK_FILE, || {
                setup_certs(
                    DEFAULT_SERVER_CERT_FILE,
                    DEFAULT_SERVER_KEY_FILE,
                    DEFAULT_CA_CERT_FILE,
                    DEFAULT_CA_KEY_FILE,
                    DEFAULT_CLIENT_CERT_FILE,
                    DEFAULT_CLIENT_KEY_FILE,
                    Uuid::new_v4(),
                )
            })?;

            Ok(Default::default())
        }

        pub fn create_certs(
            test_name: &str,
            server_name: &CStr,
            client_name: &CStr,
        ) -> std::io::Result<CertAndKeyFileNames> {
            let server_cert_name: String =
                DEFAULT_CERT_LOCATION.to_owned() + test_name + "_server.crt";
            let server_key_name: String =
                DEFAULT_CERT_LOCATION.to_owned() + test_name + "_server.key";
            let client_cert_name: String =
                DEFAULT_CERT_LOCATION.to_owned() + test_name + "_client.crt";
            let client_key_name: String =
                DEFAULT_CERT_LOCATION.to_owned() + test_name + "_client.key";
            let ca_cert_name: String = DEFAULT_CERT_LOCATION.to_owned() + test_name + "_ca.crt";
            let ca_key_name: String = DEFAULT_CERT_LOCATION.to_owned() + test_name + "_ca.key";
            setup_certs_helper(
                Path::new(&server_cert_name),
                Path::new(&server_key_name),
                Path::new(&client_cert_name),
                Path::new(&client_key_name),
                Path::new(&ca_cert_name),
                Path::new(&ca_key_name),
                server_name,
                client_name,
            )?;

            let cert_and_key_file_names = CertAndKeyFileNames {
                server_cert_name: CString::new(server_cert_name)
                    .expect("error building server cert file name"),
                server_key_name: CString::new(server_key_name)
                    .expect("error building server key file name"),
                client_cert_name: CString::new(client_cert_name)
                    .expect("error building client cert file name"),
                client_key_name: CString::new(client_key_name)
                    .expect("error building client key file name"),
                ca_cert_name: CString::new(ca_cert_name).expect("error building ca cert file name"),
                ca_key_name: CString::new(ca_key_name).expect("error building ca key file name"),
            };
            Ok(cert_and_key_file_names)
        }

        pub fn default_client_name_suffix() -> CString {
            CString::new(DEFAULT_CLIENT_NAME_SUFFIX)
                .unwrap_or_else(|error| panic!("invalid default suffix for names: {error}"))
        }

        pub fn default_server_name() -> CString {
            CString::new(DEFAULT_SERVER_NAME)
                .unwrap_or_else(|error| panic!("invalid default server name: {error}"))
        }

        pub fn default_admin_name() -> CString {
            CString::new(DEFAULT_ADMIN_NAME)
                .unwrap_or_else(|error| panic!("invalid default admin name: {error}"))
        }

        pub fn build_client_name(tenant_id: Uuid) -> CString {
            let tenant_id = Hyphenated::from_uuid(tenant_id).to_string();
            let client_name = tenant_id
                + "."
                + &default_client_name_suffix()
                    .into_string()
                    .expect("error building default client name suffix");
            CString::new(client_name)
                .unwrap_or_else(|error| panic!("invalid default client name: {error}"))
        }

        fn delete_file(file_name: CString) {
            let cert_name_path = Path::new(
                file_name
                    .to_str()
                    .unwrap_or_else(|error| panic!("invalid file name: {error}")),
            );
            if cert_name_path.exists() {
                fs::remove_file(cert_name_path).unwrap_or_else(|error| {
                    panic!(
                        "Deleting file {} failed; {}",
                        file_name.to_str().unwrap(),
                        error
                    )
                });
            }
        }

        pub fn delete_cert_and_key_files(cert_name: CString, key_name: CString) {
            delete_file(cert_name);
            delete_file(key_name);
        }

        // Creates self signed cert and key files, and returns their fully qualified
        // names to the caller. The caller should delete the cert and key files after
        // using them by invoking the function delete_cert_and_key_files.
        pub fn create_self_signed_cert(
            file_name: String,
            san: Vec<String>,
        ) -> std::io::Result<(CString, CString)> {
            let cert_file_name: String = DEFAULT_CERT_LOCATION.to_owned() + &file_name + ".crt";
            let key_file_name: String = DEFAULT_CERT_LOCATION.to_owned() + &file_name + ".key";

            setup_self_signed_certs(&cert_file_name, &key_file_name, san)?;
            Ok((
                CString::new(cert_file_name).unwrap(),
                CString::new(key_file_name).unwrap(),
            ))
        }

        fn new_ca() -> std::io::Result<(Certificate, KeyPair)> {
            let mut params = CertificateParams::default();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params
                .distinguished_name
                .push(DnType::OrganizationName, "Test CA");
            params.key_usages.push(KeyUsagePurpose::DigitalSignature);
            params.key_usages.push(KeyUsagePurpose::KeyCertSign);
            params.key_usages.push(KeyUsagePurpose::CrlSign);
            let key_pair = KeyPair::generate().expect("CA key generation failed");
            Ok((
                params
                    .self_signed(&key_pair)
                    .expect("Signing CA certificate failed"),
                key_pair,
            ))
        }

        fn new_self_signed_certificate(
            san: Vec<String>,
        ) -> std::io::Result<(Certificate, KeyPair)> {
            let key_pair =
                KeyPair::generate().expect("self signed certificate key generation failed");
            let cert = CertificateParams::new(san)
                .expect("Creating self signed certificate parameters failed")
                .self_signed(&key_pair)
                .expect("Self Signing certificate failed");
            Ok((cert, key_pair))
        }

        fn create_file_and_write_contents(
            file: &std::path::Path,
            contents: &str,
        ) -> std::io::Result<()> {
            File::create(file)
                .unwrap_or_else(|_| panic!("Creating {} failed", file.to_str().unwrap()))
                .write_all(contents.as_bytes())
                .unwrap_or_else(|_| {
                    panic!("Writing contents into {} failed", file.to_str().unwrap())
                });
            Ok(())
        }

        #[allow(clippy::too_many_arguments)]
        fn setup_certs_helper(
            server_cert_file: &Path,
            server_key_file: &Path,
            client_cert_file: &Path,
            client_key_file: &Path,
            ca_cert_file: &Path,
            ca_key_file: &Path,
            server_name: &CStr,
            client_name: &CStr,
        ) -> std::io::Result<()> {
            let (ca_cert, ca_key) = new_ca()?;
            println!("[Test] Generating self signed CA certificate {ca_cert_file:#?}");
            create_file_and_write_contents(ca_cert_file, &ca_cert.pem())?;
            create_file_and_write_contents(ca_key_file, &ca_key.serialize_pem())?;

            println!(
                "[Test] Generating self signed server certificate {server_cert_file:#?} and key {server_key_file:#?}"
            );
            // create server key and cert
            let server_key = KeyPair::generate().expect("Server key generation failed");
            let mut server_san: Vec<String> = Vec::new();
            if !server_name.is_empty() {
                server_san.push(
                    server_name
                        .to_str()
                        .expect("error building server name")
                        .to_string(),
                );
            }
            let issuer = Issuer::from_ca_cert_pem(&ca_cert.pem(), ca_key).unwrap();
            let server_cert = CertificateParams::new(server_san)
                .expect("Creating server certificate parameters failed")
                .signed_by(&server_key, &issuer)
                .expect("Signing server certificate failed");

            create_file_and_write_contents(server_cert_file, &server_cert.pem())?;
            create_file_and_write_contents(server_key_file, &server_key.serialize_pem())?;

            println!(
                "[Test] Generating self signed client certificate {client_cert_file:#?} and key {client_key_file:#?}"
            );
            // create client key and cert
            let client_key = KeyPair::generate().expect("Client key generation failed");
            let mut client_san: Vec<String> = Vec::new();
            if !client_name.is_empty() {
                client_san.push(
                    client_name
                        .to_str()
                        .expect("error building client name")
                        .to_string(),
                );
            }
            let client_cert = CertificateParams::new(client_san)
                .expect("Creating client certificate parameters failed")
                .signed_by(&client_key, &issuer)
                .expect("Signing client certificate failed");

            create_file_and_write_contents(client_cert_file, &client_cert.pem())?;
            create_file_and_write_contents(client_key_file, &client_key.serialize_pem())?;
            Ok(())
        }

        #[allow(clippy::too_many_arguments)]
        pub fn setup_certs(
            server_cert_name: &str,
            server_key_name: &str,
            ca_cert_name: &str,
            ca_key_name: &str,
            client_cert_name: &str,
            client_key_name: &str,
            tenant_id: Uuid,
        ) -> std::io::Result<()> {
            let server_cert_file = std::path::Path::new(server_cert_name);
            let server_key_file = std::path::Path::new(server_key_name);
            let ca_cert_file = std::path::Path::new(ca_cert_name);
            let ca_key_file = std::path::Path::new(ca_key_name);
            let client_cert_file = std::path::Path::new(client_cert_name);
            let client_key_file = std::path::Path::new(client_key_name);
            if !ca_cert_file.exists()
                || !ca_key_file.exists()
                || !server_cert_file.exists()
                || !server_key_file.exists()
                || !client_cert_file.exists()
                || !client_key_file.exists()
            {
                setup_certs_helper(
                    server_cert_file,
                    server_key_file,
                    client_cert_file,
                    client_key_file,
                    ca_cert_file,
                    ca_key_file,
                    &default_server_name(),
                    &build_client_name(tenant_id),
                )?;
            }
            Ok(())
        }

        fn setup_self_signed_certs(
            cert_name: &str,
            key_name: &str,
            san: Vec<String>,
        ) -> std::io::Result<()> {
            let cert_file = std::path::Path::new(cert_name);
            let key_file = std::path::Path::new(key_name);
            println!("[Test] Generating self signed certificate {cert_name} and key {key_name}");
            let (cert, key) = new_self_signed_certificate(san)?;
            create_file_and_write_contents(cert_file, &cert.pem())?;
            create_file_and_write_contents(key_file, &key.serialize_pem())?;
            Ok(())
        }
    }
}
