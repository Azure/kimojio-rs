// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A simple TLS echo server example demonstrating kimojio's TlsContext API.
//!
//! This server listens on port 8443 and echoes back any data it receives from clients
//! over a TLS 1.3 connection. It demonstrates proper usage of kimojio's async I/O
//! and TLS capabilities.

use anyhow::{Context, Result};
use clap::Parser;
use kimojio::{
    AsyncStreamRead, AsyncStreamWrite,
    operations::{accept, spawn_task},
    socket_helpers::{create_server_socket, update_accept_socket},
    tlscontext::TlsContext,
};
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod, SslVerifyMode, SslVersion};
use std::path::PathBuf;
use std::rc::Rc;

/// A TLS echo server demonstrating kimojio's async I/O with TLS 1.3
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the server certificate file (PEM format)
    #[arg(short, long, value_name = "FILE")]
    cert: PathBuf,

    /// Path to the server private key file (PEM format)
    #[arg(short, long, value_name = "FILE")]
    key: PathBuf,

    /// Path to the CA certificate file for client verification (PEM format)
    #[arg(long, value_name = "FILE")]
    ca_cert: PathBuf,

    /// Port number to listen on
    #[arg(short, long, default_value_t = 8443)]
    port: u16,
}

fn create_tls_context(cert_file: &str, key_file: &str, ca_cert_file: &str) -> Result<TlsContext> {
    // Create an SSL acceptor with TLS 1.3
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())
        .context("Failed to create SSL acceptor")?;

    // Load server certificate
    acceptor
        .set_certificate_file(cert_file, SslFiletype::PEM)
        .with_context(|| format!("Failed to load certificate file '{}'", cert_file))?;

    // Load server private key
    acceptor
        .set_private_key_file(key_file, SslFiletype::PEM)
        .with_context(|| format!("Failed to load private key file '{}'", key_file))?;

    // Load CA certificate for client verification
    acceptor
        .set_ca_file(ca_cert_file)
        .with_context(|| format!("Failed to load CA certificate file '{}'", ca_cert_file))?;

    // Enable client certificate verification
    acceptor.set_verify_callback(
        SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT,
        |ok, _ctx| ok,
    );

    // Restrict to TLS 1.3 minimum
    acceptor
        .set_min_proto_version(Some(SslVersion::TLS1_3))
        .context("Failed to set minimum TLS version")?;

    // Verify that the private key matches the certificate
    acceptor
        .check_private_key()
        .context("Private key does not match certificate")?;

    let ctx = acceptor.build().into_context();
    Ok(TlsContext::from_openssl(ctx))
}

async fn handle_client(
    client_socket: rustix::fd::OwnedFd,
    tls_context: Rc<TlsContext>,
    client_id: usize,
) {
    println!("[Client {}] New connection established", client_id);

    // Perform TLS handshake
    let mut tls_stream = match tls_context.server(16384, client_socket, None).await {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("[Client {}] TLS handshake failed: {:?}", client_id, e);
            return;
        }
    };

    println!("[Client {}] TLS handshake completed", client_id);

    // Echo loop: read data and write it back
    let mut buffer = [0u8; 8192];
    loop {
        match tls_stream.try_read(&mut buffer, None).await {
            Ok(0) => {
                // EOF - client closed the connection
                println!("[Client {}] Connection closed by client", client_id);
                break;
            }
            Ok(amount) => {
                println!("[Client {}] Received {} bytes", client_id, amount);

                // Echo the data back
                if let Err(e) = tls_stream.write(&buffer[..amount], None).await {
                    eprintln!("[Client {}] Failed to write data: {:?}", client_id, e);
                    break;
                }
            }
            Err(e) => {
                eprintln!("[Client {}] Failed to read data: {:?}", client_id, e);
                break;
            }
        }
    }

    // Clean shutdown
    if let Err(e) = tls_stream.shutdown().await {
        eprintln!("[Client {}] Failed to shutdown write: {:?}", client_id, e);
    }
    if let Err(e) = tls_stream.close().await {
        eprintln!("[Client {}] Failed to close write: {:?}", client_id, e);
    }

    println!("[Client {}] Handler completed", client_id);
}

async fn run_server(cert_file: String, key_file: String, ca_cert_file: String, port: u16) {
    // Create TLS context
    let tls_context = Rc::new(
        create_tls_context(&cert_file, &key_file, &ca_cert_file)
            .expect("Failed to create TLS context"),
    );

    // Create listening socket
    let server_socket = create_server_socket(port)
        .await
        .unwrap_or_else(|e| panic!("Failed to create server socket on port {}: {:?}", port, e));

    println!("TLS Echo Server listening on 0.0.0.0:{}", port);
    println!("Certificate: {}", cert_file);
    println!("Private key: {}", key_file);
    println!("CA certificate: {}", ca_cert_file);
    println!("Waiting for connections...\n");

    let mut client_id = 0;

    // Accept loop
    loop {
        match accept(&server_socket).await {
            Ok(client_socket) => {
                client_id += 1;
                if let Err(e) = update_accept_socket(&client_socket) {
                    eprintln!("Failed to update accept socket options: {:?}", e);
                    continue;
                }

                // Spawn a task to handle this client
                let tls_context_clone = tls_context.clone();
                spawn_task(handle_client(client_socket, tls_context_clone, client_id));
            }
            Err(e) => {
                eprintln!("Failed to accept connection: {:?}", e);
                // Continue accepting other connections
            }
        }
    }
}

fn main() {
    let args = Args::parse();

    // Verify files exist
    for (name, path) in [
        ("Certificate", &args.cert),
        ("Private key", &args.key),
        ("CA certificate", &args.ca_cert),
    ] {
        if !path.exists() {
            eprintln!("Error: {} file not found: {}", name, path.display());
            std::process::exit(1);
        }
    }

    let cert_file = args.cert.to_string_lossy().to_string();
    let key_file = args.key.to_string_lossy().to_string();
    let ca_cert_file = args.ca_cert.to_string_lossy().to_string();

    // Run the server using kimojio's test runner (which sets up the io_uring runtime)
    kimojio::run(0, run_server(cert_file, key_file, ca_cert_file, args.port));
}
