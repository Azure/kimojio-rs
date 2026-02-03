# kimojio-util

Utility types for working with [Kimojio](../kimojio) async streams.

> **Note:** This crate is a proof of concept and is not optimized for performance. It is intended to demonstrate how Kimojio streams can integrate with the broader async ecosystem.

## Overview

This crate provides adapters that bridge Kimojio's async stream traits (`AsyncStreamRead`, `AsyncStreamWrite`) with standard async I/O ecosystems like `futures_util` and `tokio`.

## Features

- **`SyncStream<S>`** - Synchronous wrapper with internal read/write buffers. Implements `std::io::Read` and `std::io::Write`, returning `WouldBlock` when async operations are needed.

- **`AsyncStream<S>`** - Async wrapper implementing `futures_util::io::AsyncRead` and `AsyncWrite`. Can be used with `tokio-util::compat` for tokio compatibility.

## Usage

```rust
use kimojio::OwnedFdStream;
use kimojio_util::AsyncStream;
use futures_util::io::{AsyncReadExt, AsyncWriteExt};

async fn example(stream: OwnedFdStream) {
    let mut async_stream = AsyncStream::new(stream);
    
    // Use futures_util traits
    async_stream.write_all(b"Hello").await.unwrap();
    async_stream.flush().await.unwrap();

    let mut buf = [0u8; 1024];
    let n = async_stream.read(&mut buf).await.unwrap();
}
```

### Tokio Compatibility

Use `tokio-util::compat` to get `tokio::io` traits:

```rust
use tokio_util::compat::FuturesAsyncReadCompatExt;
use hyper_util::rt::TokioIo;

let async_stream = AsyncStream::new(stream);
let compat_stream = async_stream.compat();  // Now implements tokio::io traits
let io = TokioIo::new(compat_stream);       // Ready for hyper
```

## Design

See [DESIGN.md](DESIGN.md) for implementation details and design notes.
