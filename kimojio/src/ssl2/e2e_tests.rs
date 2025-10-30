use std::{os::fd::OwnedFd, time::Instant};

use rustix_uring::Errno;

use crate::{
    AsyncStreamRead, AsyncStreamWrite,
    operations::spawn_task,
    pipe::bipipe,
    ssl2::tests::{create_openssl_acceptor, create_openssl_connector},
    tlscontext::{TlsContext, test::test_utils::CertAndKeyFileNames},
};

async fn c_server2(
    cert_and_key_file_names: &CertAndKeyFileNames,
    server_fd: OwnedFd,
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    let bufsize = 16384;
    let acceptor = create_openssl_acceptor(cert_and_key_file_names);
    let ctx = acceptor.into_context();
    let tls_context = TlsContext::from_openssl(ctx);
    let mut stream = tls_context.server(bufsize, server_fd, deadline).await?;

    let mut message = [0; 5];
    stream.read(&mut message, deadline).await.unwrap();
    assert_eq!(message, "hello".as_bytes());
    stream.write("goodbye".as_bytes(), None).await.unwrap();

    stream.shutdown().await.unwrap();

    Ok(())
}
pub(crate) async fn c_client2(
    cert_and_key_file_names: &CertAndKeyFileNames,
    client_fd: OwnedFd,
    deadline: Option<Instant>,
) -> Result<(), Errno> {
    let bufsize = 16384;
    let connector = create_openssl_connector(cert_and_key_file_names);

    // TODO: need to set servername sni verification etc. openssl crate has a builder to do this.
    // Most likely we do not do this in the current implementation.
    let tls_context = TlsContext::from_openssl(connector.into_context());
    let mut stream = tls_context.client(bufsize, client_fd, deadline).await?;

    stream.write("hello".as_bytes(), None).await.unwrap();
    let mut response = [0; 7];
    stream.read(&mut response, deadline).await?;
    assert_eq!(response, "goodbye".as_bytes());

    stream.shutdown().await.unwrap();

    Ok(())
}

async fn c_stream() {
    let cert_and_key_file_names =
        crate::tlscontext::test::test_utils::setup_default_certs().unwrap();
    let (client_fd, server_fd) = bipipe();
    let cert_and_key_file_names_clone = cert_and_key_file_names.clone();
    spawn_task(async move {
        c_server2(&cert_and_key_file_names_clone, server_fd, None)
            .await
            .unwrap();
    });

    c_client2(&cert_and_key_file_names, client_fd, None)
        .await
        .unwrap();
}

/// This tests constructing tls context from OpenSSL crate SslContext.
/// And use it to create a client and server stream.
#[test]
fn c_stream_test() {
    crate::run_test("c_stream", c_stream())
}
