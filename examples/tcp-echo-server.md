# TCP Echo Server Example

A minimal TCP echo server demonstrating kimojio's async networking without any TLS dependency. It accepts client connections and echoes back every byte received, showing the core async I/O primitives in action.

## Features

- **No TLS required**: Works out of the box with no certificate setup
- **Async I/O**: Uses `OwnedFdStream` for buffered async reads and writes
- **Multiple clients**: Each connection is handled in its own spawned task
- **Graceful shutdown**: Properly flushes and closes each connection

## Building

```bash
cargo build --release -p examples --bin tcp-echo-server
```

Or run directly:
```bash
cargo run --release -p examples --bin tcp-echo-server -- --help
```

> **Note:** When cross-compiling to Linux (e.g., from Windows) without the cross-compile toolchain, you can still check the code compiles cleanly with:
> ```bash
> cargo clippy -p examples --bin tcp-echo-server --target x86_64-unknown-linux-gnu --no-default-features --features io_uring
> ```

## Running

```bash
cargo run --release -p examples --bin tcp-echo-server
```

The server starts on port `7878` by default:
```
TCP Echo Server listening on 0.0.0.0:7878
Connect with: nc localhost 7878
Waiting for connections...
```

Use `--port` to choose a different port:
```bash
cargo run --release -p examples --bin tcp-echo-server -- --port 9000
```

## Testing

Use `nc` (netcat) to connect and send data:
```bash
nc localhost 7878
```

Type anything and press Enter — the server echoes it back. Press `Ctrl+D` to close the connection.

For a quick one-liner:
```bash
echo "Hello, kimojio!" | nc localhost 7878
```

Expected server output:
```
[Client 1] Connected
[Client 1] Echoing 17 bytes
[Client 1] Disconnected
[Client 1] Handler done
```

## Implementation Details

1. **`create_server_socket(port)`** — creates an IPv6 dual-stack TCP socket, sets `SO_REUSEADDR` and `TCP_NODELAY`, binds and listens.
2. **`accept(&server_fd)`** — waits asynchronously for the next incoming connection.
3. **`update_accept_socket`** — applies `TCP_NODELAY` and keep-alive options to the accepted fd.
4. **`spawn_task`** — runs each client handler as an independent task on the single-threaded runtime.
5. **`OwnedFdStream`** — wraps the raw `OwnedFd` in a buffered stream that implements `AsyncStreamRead` and `AsyncStreamWrite`.

## License

This example is part of the kimojio-rs project and is licensed under the MIT License.
