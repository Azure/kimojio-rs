// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for hyper HTTP/1.1 with kimojio stream adaptors.

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1 as server_http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use kimojio::OwnedFdStream;
use kimojio::operations::{self, spawn_task};
use kimojio::socket_helpers::{create_client_socket, create_server_socket, update_accept_socket};
use kimojio_util::AsyncStream;
use std::net::SocketAddr;
use tokio_util::compat::FuturesAsyncReadCompatExt;

/// Helper to get the port from a bound server socket.
fn get_server_port(server_socket: &kimojio::OwnedFd) -> u16 {
    let addr: SocketAddr = rustix::net::getsockname(server_socket)
        .unwrap()
        .try_into()
        .unwrap();
    addr.port()
}

/// Simple HTTP handler that echoes back the request body or returns "Hello, World!".
async fn echo_handler(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, hyper::Error> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => Ok(Response::new(Full::new(Bytes::from("Hello, World!")))),
        (&Method::GET, "/health") => Ok(Response::new(Full::new(Bytes::from("OK")))),
        (&Method::POST, "/echo") => {
            // Collect the body and echo it back
            let body_bytes = req.collect().await?.to_bytes();
            Ok(Response::new(Full::new(body_bytes)))
        }
        _ => {
            let mut response = Response::new(Full::new(Bytes::from("Not Found")));
            *response.status_mut() = StatusCode::NOT_FOUND;
            Ok(response)
        }
    }
}

/// Test HTTP/1.1 server using hyper with kimojio stream adaptor.
///
/// This test demonstrates:
/// 1. Creating a TCP server using kimojio
/// 2. Wrapping the stream with AsyncStream + tokio compat layer
/// 3. Using hyper's HTTP/1.1 server with the adapted stream
#[kimojio::test]
async fn test_hyper_http1_server_get() {
    // Start server on a random port
    let server_socket = create_server_socket(0).await.unwrap();
    let port = get_server_port(&server_socket);

    // Spawn HTTP server
    let server_task = spawn_task(async move {
        let client_fd = operations::accept(&server_socket).await.unwrap();
        update_accept_socket(&client_fd).unwrap();

        let stream = OwnedFdStream::new(client_fd);
        let async_stream = AsyncStream::new(stream);
        let compat_stream = async_stream.compat();

        // Wrap in TokioIo for hyper compatibility
        let io = TokioIo::new(compat_stream);

        // Serve HTTP/1.1 connection
        let result = server_http1::Builder::new()
            .serve_connection(io, service_fn(echo_handler))
            .await;

        if let Err(e) = result {
            // Connection closed by client is expected
            if !e.is_incomplete_message() {
                panic!("Server error: {:?}", e);
            }
        }
    });

    // Client side - use a simple TCP connection with hyper client
    let addr: SocketAddr = format!("[::1]:{}", port).parse().unwrap();
    let client_socket = create_client_socket(&addr).await.unwrap();
    let stream = OwnedFdStream::new(client_socket);
    let async_stream = AsyncStream::new(stream);
    let compat_stream = async_stream.compat();
    let io = TokioIo::new(compat_stream);

    // Perform HTTP handshake
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();

    // Spawn connection handler
    let conn_task = spawn_task(async move {
        if let Err(e) = conn.await
            && !e.is_incomplete_message()
        {
            panic!("Connection error: {:?}", e);
        }
    });

    // Send GET request
    let req = Request::builder()
        .method(Method::GET)
        .uri("/")
        .header("Host", format!("[::1]:{}", port))
        .body(Empty::<Bytes>::new())
        .unwrap();

    let response = sender.send_request(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"Hello, World!");

    // Close connection
    drop(sender);
    conn_task.await.unwrap();
    server_task.await.unwrap();
}

/// Test HTTP/1.1 POST with body echo.
#[kimojio::test]
async fn test_hyper_http1_server_post_echo() {
    // Start server on a random port
    let server_socket = create_server_socket(0).await.unwrap();
    let port = get_server_port(&server_socket);

    // Spawn HTTP server
    let server_task = spawn_task(async move {
        let client_fd = operations::accept(&server_socket).await.unwrap();
        update_accept_socket(&client_fd).unwrap();

        let stream = OwnedFdStream::new(client_fd);
        let async_stream = AsyncStream::new(stream);
        let compat_stream = async_stream.compat();
        let io = TokioIo::new(compat_stream);

        let result = server_http1::Builder::new()
            .serve_connection(io, service_fn(echo_handler))
            .await;

        if let Err(e) = result
            && !e.is_incomplete_message()
        {
            panic!("Server error: {:?}", e);
        }
    });

    // Client side
    let addr: SocketAddr = format!("[::1]:{}", port).parse().unwrap();
    let client_socket = create_client_socket(&addr).await.unwrap();
    let stream = OwnedFdStream::new(client_socket);
    let async_stream = AsyncStream::new(stream);
    let compat_stream = async_stream.compat();
    let io = TokioIo::new(compat_stream);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();

    let conn_task = spawn_task(async move {
        if let Err(e) = conn.await
            && !e.is_incomplete_message()
        {
            panic!("Connection error: {:?}", e);
        }
    });

    // Send POST request with body
    let body_content = b"This is the request body to echo!";
    let req = Request::builder()
        .method(Method::POST)
        .uri("/echo")
        .header("Host", format!("[::1]:{}", port))
        .header("Content-Length", body_content.len())
        .body(Full::new(Bytes::from(&body_content[..])))
        .unwrap();

    let response = sender.send_request(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], body_content);

    drop(sender);
    conn_task.await.unwrap();
    server_task.await.unwrap();
}

/// Test multiple HTTP/1.1 requests on the same connection (keep-alive).
#[kimojio::test]
async fn test_hyper_http1_keep_alive() {
    // Start server on a random port
    let server_socket = create_server_socket(0).await.unwrap();
    let port = get_server_port(&server_socket);

    // Spawn HTTP server
    let server_task = spawn_task(async move {
        let client_fd = operations::accept(&server_socket).await.unwrap();
        update_accept_socket(&client_fd).unwrap();

        let stream = OwnedFdStream::new(client_fd);
        let async_stream = AsyncStream::new(stream);
        let compat_stream = async_stream.compat();
        let io = TokioIo::new(compat_stream);

        // Enable keep-alive
        let result = server_http1::Builder::new()
            .keep_alive(true)
            .serve_connection(io, service_fn(echo_handler))
            .await;

        if let Err(e) = result
            && !e.is_incomplete_message()
        {
            panic!("Server error: {:?}", e);
        }
    });

    // Client side
    let addr: SocketAddr = format!("[::1]:{}", port).parse().unwrap();
    let client_socket = create_client_socket(&addr).await.unwrap();
    let stream = OwnedFdStream::new(client_socket);
    let async_stream = AsyncStream::new(stream);
    let compat_stream = async_stream.compat();
    let io = TokioIo::new(compat_stream);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();

    let conn_task = spawn_task(async move {
        if let Err(e) = conn.await
            && !e.is_incomplete_message()
        {
            panic!("Connection error: {:?}", e);
        }
    });

    // Send multiple requests on the same connection
    for i in 0..5 {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/health")
            .header("Host", format!("[::1]:{}", port))
            .body(Empty::<Bytes>::new())
            .unwrap();

        let response = sender.send_request(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "Request {} failed", i);

        let body = response.collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"OK", "Request {} body mismatch", i);
    }

    drop(sender);
    conn_task.await.unwrap();
    server_task.await.unwrap();
}

/// Test HTTP/1.1 with large body.
#[kimojio::test]
async fn test_hyper_http1_large_body() {
    // Start server on a random port
    let server_socket = create_server_socket(0).await.unwrap();
    let port = get_server_port(&server_socket);

    // Spawn HTTP server
    let server_task = spawn_task(async move {
        let client_fd = operations::accept(&server_socket).await.unwrap();
        update_accept_socket(&client_fd).unwrap();

        let stream = OwnedFdStream::new(client_fd);
        let async_stream = AsyncStream::new(stream);
        let compat_stream = async_stream.compat();
        let io = TokioIo::new(compat_stream);

        let result = server_http1::Builder::new()
            .serve_connection(io, service_fn(echo_handler))
            .await;

        if let Err(e) = result
            && !e.is_incomplete_message()
        {
            panic!("Server error: {:?}", e);
        }
    });

    // Client side
    let addr: SocketAddr = format!("[::1]:{}", port).parse().unwrap();
    let client_socket = create_client_socket(&addr).await.unwrap();
    let stream = OwnedFdStream::new(client_socket);
    let async_stream = AsyncStream::new(stream);
    let compat_stream = async_stream.compat();
    let io = TokioIo::new(compat_stream);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();

    let conn_task = spawn_task(async move {
        if let Err(e) = conn.await
            && !e.is_incomplete_message()
        {
            panic!("Connection error: {:?}", e);
        }
    });

    // Create a large body (100KB)
    let large_body: Vec<u8> = (0..255u8).cycle().take(100 * 1024).collect();
    let expected_body = large_body.clone();

    let req = Request::builder()
        .method(Method::POST)
        .uri("/echo")
        .header("Host", format!("[::1]:{}", port))
        .header("Content-Length", large_body.len())
        .body(Full::new(Bytes::from(large_body)))
        .unwrap();

    let response = sender.send_request(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), expected_body.len());
    assert_eq!(&body[..], &expected_body[..]);

    drop(sender);
    conn_task.await.unwrap();
    server_task.await.unwrap();
}
