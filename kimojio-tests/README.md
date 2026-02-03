# kimojio-tests

Integration tests for [kimojio-util](../kimojio-util) stream adapters.

## Overview

This crate contains integration tests that verify the stream adapters work correctly with real-world async I/O scenarios, including TCP sockets and HTTP protocols.

## Tests

### Stream Echo Tests (`stream_echo.rs`)

Basic TCP echo tests using the stream adapters:
- `test_tcp_echo_with_tokio_compat` - Echo using `tokio::io` traits via compat layer
- `test_tcp_echo_with_futures_util` - Echo using `futures_util::io` traits directly
- `test_tcp_echo_multiple_messages_tokio_compat` - Multiple round-trips on one connection

### Hyper HTTP/1.1 Tests (`hyper_http1.rs`)

HTTP/1.1 server/client tests using hyper with kimojio streams:
- `test_hyper_http1_server_get` - Basic GET request
- `test_hyper_http1_server_post_echo` - POST with body echo
- `test_hyper_http1_keep_alive` - HTTP keep-alive with multiple requests
- `test_hyper_http1_large_body` - Large (100KB) request body

## Running Tests

```bash
cargo test --package kimojio-tests
```
