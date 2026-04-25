// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A plain TCP echo server demonstrating kimojio's async networking without TLS.
//!
//! This server listens on a configurable port and echoes back any data it receives
//! from each client. It demonstrates:
//! - Creating a TCP server socket with `create_server_socket`
//! - Accepting connections with `accept`
//! - Wrapping a raw fd in `OwnedFdStream` for buffered async I/O
//! - Spawning per-client handler tasks with `spawn_task`
//! - Graceful connection shutdown

use anyhow::Result;
use clap::Parser;
use kimojio::{
    AsyncStreamRead, AsyncStreamWrite, OwnedFdStream,
    operations::{accept, spawn_task},
    socket_helpers::{create_server_socket, update_accept_socket},
};

/// A plain TCP echo server demonstrating kimojio's async networking
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Port number to listen on
    #[arg(short, long, default_value_t = 7878)]
    port: u16,
}

async fn handle_client(fd: rustix::fd::OwnedFd, client_id: usize) {
    println!("[Client {}] Connected", client_id);

    let mut stream = OwnedFdStream::new(fd);
    let mut buf = vec![0u8; 8192];

    loop {
        match stream.try_read(&mut buf, None).await {
            Ok(0) => {
                println!("[Client {}] Disconnected", client_id);
                break;
            }
            Ok(n) => {
                println!("[Client {}] Echoing {} bytes", client_id, n);
                if let Err(e) = stream.write(&buf[..n], None).await {
                    eprintln!("[Client {}] Write error: {:?}", client_id, e);
                    break;
                }
            }
            Err(e) => {
                eprintln!("[Client {}] Read error: {:?}", client_id, e);
                break;
            }
        }
    }

    if let Err(e) = stream.shutdown().await {
        eprintln!("[Client {}] Shutdown error: {:?}", client_id, e);
    }
    if let Err(e) = stream.close().await {
        eprintln!("[Client {}] Close error: {:?}", client_id, e);
    }

    println!("[Client {}] Handler done", client_id);
}

async fn run_server(port: u16) {
    let server_fd = create_server_socket(port)
        .await
        .unwrap_or_else(|e| panic!("Failed to create server socket on port {port}: {e:?}"));

    println!("TCP Echo Server listening on 0.0.0.0:{port}");
    println!("Connect with: nc localhost {port}");
    println!("Waiting for connections...\n");

    let mut client_id = 0usize;
    loop {
        match accept(&server_fd).await {
            Ok(client_fd) => {
                client_id += 1;
                if let Err(e) = update_accept_socket(&client_fd) {
                    eprintln!("Failed to configure accepted socket: {e:?}");
                    continue;
                }
                spawn_task(handle_client(client_fd, client_id));
            }
            Err(e) => {
                eprintln!("Accept error: {e:?}");
            }
        }
    }
}

/// Sets up the kimojio runtime and starts the TCP echo server.
#[kimojio::main]
async fn main() -> Result<(), kimojio::Errno> {
    let args = Args::parse();
    run_server(args.port).await;
    Ok(())
}
