# Kimojio Stream Adaptor

This crate provides stream adaptors that bridge Kimojio's async stream traits (`AsyncStreamRead`, `AsyncStreamWrite`) with the `futures_util` async I/O traits (`futures_util::io::AsyncRead`, `futures_util::io::AsyncWrite`). This is analogous to the [tokio-uring stream_adapt.rs](https://github.com/youyuanwu/tokio-misc/blob/main/tokio-uring-util/src/stream_adapt.rs) implementation.

## Usage

```rust
use kimojio::OwnedFdStream;
use kimojio_util::AsyncStream;
use futures_util::io::{AsyncReadExt, AsyncWriteExt};

async fn example(stream: OwnedFdStream) {
    let mut async_stream = AsyncStream::new(stream);

    // Now compatible with futures_util traits!
    async_stream.write_all(b"Hello").await.unwrap();
    async_stream.flush().await.unwrap();

    let mut buf = [0u8; 1024];
    let n = async_stream.read(&mut buf).await.unwrap();
}
```

## Types

### `SyncStream<S>`

A synchronous wrapper that buffers data and provides `std::io::Read`/`Write` traits. Returns `WouldBlock` when buffers need async operations.

**Key Methods:**
- `new(stream: S)` - Create with default buffer capacity (8KB)
- `with_capacity(capacity, stream)` - Create with specified capacity
- `fill_read_buf()` - Async: fill read buffer from inner stream
- `flush_write_buf()` - Async: flush write buffer to inner stream
- `shutdown()` - Async: flush and shutdown the stream

### `AsyncStream<S>`

A futures-compatible wrapper implementing `futures_util::io::AsyncRead`/`AsyncWrite`.

**Key Methods:**
- `new(stream: S)` - Create with default buffer capacity
- `with_capacity(capacity, stream)` - Create with specified capacity
- `get_ref()` / `get_mut()` - Access inner stream

---

## Design Notes

## Problem Statement

Kimojio has its own async stream traits:
- `AsyncStreamRead` - with `try_read()` and `read()` methods
- `AsyncStreamWrite` - with `write()`, `writev()`, `shutdown()`, and `close()` methods

These traits are not compatible with the standard `futures_util::io` traits (`AsyncRead`, `AsyncWrite`) that many ecosystem crates expect. An adaptor layer is needed to bridge these two worlds.

## Reference Implementation Analysis

The tokio-uring `stream_adapt.rs` provides two main types:

1. **`SyncStream<S>`** - Wraps an async stream with internal buffers to provide sync `std::io::Read`/`Write` traits
   - Returns `WouldBlock` when buffers need async operations
   - Provides `fill_read_buf()` and `flush_write_buf()` async methods for buffer management

2. **`AsyncStream<S>`** - Wraps `SyncStream` to implement `futures_util::io::AsyncRead`/`AsyncWrite`
   - Uses pinned boxed futures to track ongoing read/write/shutdown operations
   - Converts `WouldBlock` errors into pending state with async buffer operations

## Implementation Plan

### 1. Define the Module Structure

Create `kimojio-util/src/stream_adapt.rs` with the following structure:

```
kimojio-util/src/
├── lib.rs           # Export stream_adapt module
└── stream_adapt.rs  # Stream adaptor implementation
```

### 2. Implement `SyncStream<S>`

A synchronous wrapper that buffers data and provides `std::io::Read`/`Write` traits.

**Fields:**
```rust
pub struct SyncStream<S> {
    inner: S,
    read_buffer: VecDeque<u8>,
    write_buffer: Vec<u8>,
    eof: bool,
}
```

**Key Methods:**
- `new(stream: S) -> Self` - Create with default buffer capacity
- `with_capacity(capacity: usize, stream: S) -> Self` - Create with specified capacity
- `fill_read_buf(&mut self) -> impl Future<Output = io::Result<usize>>` - Fill read buffer from inner stream
- `flush_write_buf(&mut self) -> impl Future<Output = io::Result<usize>>` - Flush write buffer to inner stream
- `shutdown(&mut self) -> impl Future<Output = io::Result<()>>` - Flush and shutdown

**Trait Implementations:**
- `std::io::Read` - Returns `WouldBlock` when read buffer is empty
- `std::io::Write` - Returns `WouldBlock` when write buffer needs flushing

**Key Differences from tokio-uring:**

| tokio-uring | Kimojio |
|-------------|---------|
| `inner.read(buf).await` returns `(Result, buf)` | `inner.try_read(&mut buf, deadline).await` returns `Result<usize>` |
| `inner.write_all(data).await` returns `(Result, buf)` | `inner.write(&buf, deadline).await` returns `Result<()>` |
| `inner.shutdown().await` | `inner.shutdown().await` (similar) |

### 3. Implement `AsyncStream<S>`

A futures-compatible wrapper that implements `futures_util::io::AsyncRead`/`AsyncWrite`.

**Fields:**
```rust
pin_project! {
    pub struct AsyncStream<S> {
        #[pin]
        inner: SyncStream<S>,
        read_future: Option<PinBoxFuture<io::Result<usize>>>,
        write_future: Option<PinBoxFuture<io::Result<usize>>>,
        shutdown_future: Option<PinBoxFuture<io::Result<()>>>,
    }
}
```

**Trait Implementations:**
- `futures_util::io::AsyncRead` via `poll_read()`
- `futures_util::io::AsyncWrite` via `poll_write()`, `poll_flush()`, `poll_close()`

**Poll Logic:**
1. Check if there's an ongoing future, poll it
2. If pending, store future and return `Poll::Pending`
3. If complete, try sync operation
4. If sync returns `WouldBlock`, spawn async buffer operation and return `Poll::Pending`
5. Otherwise return the result

### 4. Dependencies

Add to `kimojio-util/Cargo.toml`:
```toml
[dependencies]
kimojio = { path = "../kimojio" }
futures-util = { version = "0.3", features = ["io"] }
pin-project-lite = "0.2"
```

### 5. API Design Considerations

**Generic Bounds:**
```rust
impl<S: AsyncStreamRead + AsyncStreamWrite> SyncStream<S> { ... }
impl<S: AsyncStreamRead + AsyncStreamWrite + 'static> AsyncRead for AsyncStream<S> { ... }
impl<S: AsyncStreamRead + AsyncStreamWrite + 'static> AsyncWrite for AsyncStream<S> { ... }
```

**Deadline Handling:**
- Kimojio streams support optional deadlines
- For the adaptor, we can:
  - Option A: Use `None` (no deadline) for simplicity
  - Option B: Add a configurable deadline field to `SyncStream`
  - Option C: Provide methods to set per-operation deadlines

**Recommended:** Start with Option A (no deadline), add configuration later if needed.

### 6. Implementation Status

- [x] **Task 1:** Create `stream_adapt.rs` with `SyncStream` struct and constructors
- [x] **Task 2:** Implement `fill_read_buf()` using `AsyncStreamRead::try_read()`
- [x] **Task 3:** Implement `flush_write_buf()` using `AsyncStreamWrite::write()`
- [x] **Task 4:** Implement `shutdown()` using `AsyncStreamWrite::shutdown()`
- [x] **Task 5:** Implement `std::io::Read` for `SyncStream`
- [x] **Task 6:** Implement `std::io::Write` for `SyncStream`
- [x] **Task 7:** Create `AsyncStream` struct with pinned futures
- [x] **Task 8:** Implement `futures_util::io::AsyncRead` for `AsyncStream`
- [x] **Task 9:** Implement `futures_util::io::AsyncWrite` for `AsyncStream`
- [x] **Task 10:** Add unit tests using `BufferPipe`
- [ ] **Task 11:** Add integration tests with TCP streams
- [x] **Task 12:** Update `lib.rs` to export the new types

### 7. Current Implementation

The implementation is in `src/stream_adapt.rs` with the following structure:

```
kimojio-util/src/
├── lib.rs           # Exports SyncStream and AsyncStream
└── stream_adapt.rs  # Stream adaptor implementation
```

**Dependencies** (in `Cargo.toml`):
```toml
[dependencies]
kimojio = { path = "../kimojio" }
futures-util = { version = "0.3", features = ["io"] }
pin-project-lite = "0.2"
```

**Unit Tests (7 passing):**
- `test_sync_stream_basic_read_write` - Basic sync buffer operations
- `test_sync_stream_would_block_on_empty_read` - WouldBlock behavior
- `test_sync_stream_eof` - EOF handling
- `test_async_stream_read_write` - Async read/write
- `test_async_stream_partial_read` - Partial reads
- `test_async_stream_large_data` - Large data with concurrent read/write
- `test_async_stream_close` - Stream close behavior

### 8. Example Usage

```rust
use kimojio::{OwnedFdStream, operations};
use kimojio_util::AsyncStream;
use futures_util::io::{AsyncReadExt, AsyncWriteExt};

async fn example() {
    let socket = operations::tcp_connect("127.0.0.1:8080").await.unwrap();
    let stream = OwnedFdStream::new(socket);
    let mut async_stream = AsyncStream::new(stream);

    // Now compatible with futures_util traits!
    async_stream.write_all(b"Hello").await.unwrap();
    async_stream.flush().await.unwrap();

    let mut buf = [0u8; 1024];
    let n = async_stream.read(&mut buf).await.unwrap();
}
```

### 9. Safety Considerations

The reference implementation uses `unsafe` to extend lifetimes for pinned boxed futures:
```rust
let inner: &'static mut SyncStream<S> = 
    unsafe { &mut *(this.inner.get_unchecked_mut() as *mut _) };
```

This is necessary because:
1. The boxed future needs to capture a reference to `SyncStream`
2. The future lives in the struct alongside `SyncStream`
3. We control the lifetime and ensure the stream isn't moved

**Mitigation:** Document the safety invariants clearly and consider alternative designs if possible.

### 10. Testing Strategy

1. **Unit tests:** Test `SyncStream` buffer management with `BufferPipe`
2. **Integration tests:** Test `AsyncStream` with actual TCP sockets
3. **Edge cases:**
   - EOF handling
   - Partial reads/writes
   - WouldBlock scenarios
   - Shutdown during pending operations
   - Large data transfers spanning multiple buffers

### 11. Tokio Compatibility

**Important:** `futures_util::io` traits and `tokio::io` traits are **separate and incompatible**:

| Crate | Traits |
|-------|--------|
| `futures-util` / `futures-io` | `futures_util::io::AsyncRead`, `AsyncWrite` |
| `tokio` | `tokio::io::AsyncRead`, `AsyncWrite` |

Implementing `futures_util` traits does **not** automatically provide tokio compatibility.

#### Option A: Use `tokio-util::compat` (Recommended for Users)

Users can wrap the adapted stream with tokio's compatibility layer:

```rust
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

let futures_stream = AsyncStream::new(inner);
let tokio_stream = futures_stream.compat(); // Now implements tokio::io::AsyncRead/Write
```

#### Option B: Native Tokio Support (Future Enhancement)

Add tokio trait implementations behind a feature flag:

```toml
[features]
default = []
tokio-compat = ["tokio"]

[dependencies]
tokio = { version = "1", features = ["io-util"], optional = true }
```

```rust
#[cfg(feature = "tokio-compat")]
impl<S: AsyncStreamRead + AsyncStreamWrite + 'static> tokio::io::AsyncRead for AsyncStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Note: tokio uses ReadBuf instead of &mut [u8]
        // Implementation differs from futures_util version
    }
}

#[cfg(feature = "tokio-compat")]
impl<S: AsyncStreamRead + AsyncStreamWrite + 'static> tokio::io::AsyncWrite for AsyncStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Similar to futures_util version
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> { ... }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> { ... }
}
```

**Key API Differences:**
- `tokio::io::AsyncRead::poll_read` uses `ReadBuf<'_>` instead of returning `usize`
- `tokio::io::AsyncWrite` uses `poll_shutdown` instead of `poll_close`

#### Recommendation

Start with `futures_util` traits only. Users needing tokio compatibility can use `tokio-util::compat`. Add native tokio support later if there's demand.

### 12. Remaining Work & Future Enhancements

- [ ] Add integration tests with TCP streams (Task 11)
- [ ] Native `tokio::io` trait support (optional `tokio-compat` feature)
- [ ] Configurable deadline support
- [ ] Metrics/tracing integration
- [ ] Split stream support (`AsyncStream::split()`)
