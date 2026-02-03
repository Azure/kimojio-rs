// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for stream adaptors with tokio compatibility.

use futures_util::io::{AsyncReadExt as FutAsyncReadExt, AsyncWriteExt as FutAsyncWriteExt};
use kimojio::OwnedFdStream;
use kimojio::operations::{self, spawn_task};
use kimojio::socket_helpers::{create_client_socket, create_server_socket, update_accept_socket};
use kimojio_util::AsyncStream;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt as TokioAsyncReadExt, AsyncWriteExt as TokioAsyncWriteExt};
use tokio_util::compat::FuturesAsyncReadCompatExt;

/// Helper to get the port from a bound server socket.
fn get_server_port(server_socket: &kimojio::OwnedFd) -> u16 {
    let addr: SocketAddr = rustix::net::getsockname(server_socket)
        .unwrap()
        .try_into()
        .unwrap();
    addr.port()
}

/// Test TCP echo using AsyncStream with tokio-util compat layer.
///
/// This test demonstrates:
/// 1. Creating a TCP server and client using kimojio
/// 2. Wrapping the streams with AsyncStream (futures_util traits)
/// 3. Using tokio-util::compat to get tokio::io traits
/// 4. Using tokio::io::AsyncReadExt/AsyncWriteExt for I/O
#[kimojio::test]
async fn test_tcp_echo_with_tokio_compat() {
    // Start server on port 0 to get a random available port
    let server_socket = create_server_socket(0).await.unwrap();
    let port = get_server_port(&server_socket);

    // Spawn echo server
    let server_task = spawn_task(async move {
        let client_fd = operations::accept(&server_socket).await.unwrap();
        update_accept_socket(&client_fd).unwrap();

        let stream = OwnedFdStream::new(client_fd);
        let async_stream = AsyncStream::new(stream);

        // Use tokio compat layer
        let mut compat_stream = async_stream.compat();

        // Echo loop - read and write back using tokio traits
        let mut buf = [0u8; 1024];
        loop {
            let n = TokioAsyncReadExt::read(&mut compat_stream, &mut buf)
                .await
                .unwrap();
            if n == 0 {
                break;
            }
            TokioAsyncWriteExt::write_all(&mut compat_stream, &buf[..n])
                .await
                .unwrap();
            TokioAsyncWriteExt::flush(&mut compat_stream).await.unwrap();
        }
    });

    // Client side
    let addr: SocketAddr = format!("[::1]:{}", port).parse().unwrap();
    let client_socket = create_client_socket(&addr).await.unwrap();
    let stream = OwnedFdStream::new(client_socket);
    let async_stream = AsyncStream::new(stream);

    // Use tokio compat layer
    let mut compat_stream = async_stream.compat();

    // Send test data using tokio traits
    let test_data = b"Hello from tokio compat!";
    TokioAsyncWriteExt::write_all(&mut compat_stream, test_data)
        .await
        .unwrap();
    TokioAsyncWriteExt::flush(&mut compat_stream).await.unwrap();

    // Read echoed data using tokio traits
    let mut read_buf = vec![0u8; test_data.len()];
    TokioAsyncReadExt::read_exact(&mut compat_stream, &mut read_buf)
        .await
        .unwrap();

    assert_eq!(&read_buf, test_data);

    // Close connection
    TokioAsyncWriteExt::shutdown(&mut compat_stream)
        .await
        .unwrap();

    server_task.await.unwrap();
}

/// Test TCP echo using AsyncStream with futures_util traits directly.
#[kimojio::test]
async fn test_tcp_echo_with_futures_util() {
    // Start server on port 0 to get a random available port
    let server_socket = create_server_socket(0).await.unwrap();
    let port = get_server_port(&server_socket);

    // Spawn echo server
    let server_task = spawn_task(async move {
        let client_fd = operations::accept(&server_socket).await.unwrap();
        update_accept_socket(&client_fd).unwrap();

        let stream = OwnedFdStream::new(client_fd);
        let mut async_stream = AsyncStream::new(stream);

        // Echo loop using futures_util traits
        let mut buf = [0u8; 1024];
        loop {
            let n = FutAsyncReadExt::read(&mut async_stream, &mut buf)
                .await
                .unwrap();
            if n == 0 {
                break;
            }
            FutAsyncWriteExt::write_all(&mut async_stream, &buf[..n])
                .await
                .unwrap();
            FutAsyncWriteExt::flush(&mut async_stream).await.unwrap();
        }
    });

    // Client side
    let addr: SocketAddr = format!("[::1]:{}", port).parse().unwrap();
    let client_socket = create_client_socket(&addr).await.unwrap();
    let stream = OwnedFdStream::new(client_socket);
    let mut async_stream = AsyncStream::new(stream);

    // Send test data
    let test_data = b"Hello from futures_util!";
    FutAsyncWriteExt::write_all(&mut async_stream, test_data)
        .await
        .unwrap();
    FutAsyncWriteExt::flush(&mut async_stream).await.unwrap();

    // Read echoed data
    let mut read_buf = vec![0u8; test_data.len()];
    FutAsyncReadExt::read_exact(&mut async_stream, &mut read_buf)
        .await
        .unwrap();

    assert_eq!(&read_buf, test_data);

    // Close connection
    FutAsyncWriteExt::close(&mut async_stream).await.unwrap();

    server_task.await.unwrap();
}

/// Test multiple round trips with tokio compat layer.
#[kimojio::test]
async fn test_tcp_echo_multiple_messages_tokio_compat() {
    // Start server on port 0 to get a random available port
    let server_socket = create_server_socket(0).await.unwrap();
    let port = get_server_port(&server_socket);

    // Spawn echo server
    let server_task = spawn_task(async move {
        let client_fd = operations::accept(&server_socket).await.unwrap();
        update_accept_socket(&client_fd).unwrap();

        let stream = OwnedFdStream::new(client_fd);
        let async_stream = AsyncStream::new(stream);
        let mut compat_stream = async_stream.compat();

        let mut buf = [0u8; 1024];
        loop {
            let n = TokioAsyncReadExt::read(&mut compat_stream, &mut buf)
                .await
                .unwrap();
            if n == 0 {
                break;
            }
            TokioAsyncWriteExt::write_all(&mut compat_stream, &buf[..n])
                .await
                .unwrap();
            TokioAsyncWriteExt::flush(&mut compat_stream).await.unwrap();
        }
    });

    // Client side
    let addr: SocketAddr = format!("[::1]:{}", port).parse().unwrap();
    let client_socket = create_client_socket(&addr).await.unwrap();
    let stream = OwnedFdStream::new(client_socket);
    let async_stream = AsyncStream::new(stream);
    let mut compat_stream = async_stream.compat();

    // Multiple round trips
    for i in 0..10 {
        let test_data = format!("Message {}", i);
        TokioAsyncWriteExt::write_all(&mut compat_stream, test_data.as_bytes())
            .await
            .unwrap();
        TokioAsyncWriteExt::flush(&mut compat_stream).await.unwrap();

        let mut read_buf = vec![0u8; test_data.len()];
        TokioAsyncReadExt::read_exact(&mut compat_stream, &mut read_buf)
            .await
            .unwrap();

        assert_eq!(String::from_utf8(read_buf).unwrap(), test_data);
    }

    TokioAsyncWriteExt::shutdown(&mut compat_stream)
        .await
        .unwrap();

    server_task.await.unwrap();
}
