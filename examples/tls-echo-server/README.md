# TLS Echo Server Example

A simple TLS echo server demonstrating the use of kimojio's async I/O capabilities with TLS 1.3. This server accepts client connections, performs mutual TLS authentication, and echoes back any data it receives.

## Features

- **TLS 1.3**: Uses modern TLS 1.3 for secure communication
- **Mutual Authentication**: Requires both server and client certificates
- **Async I/O**: Built on kimojio's efficient io_uring-based async runtime
- **Multiple Clients**: Handles multiple concurrent client connections

## Building

Build the example from the workspace root:

```bash
cd /workspace/kimojio-rs
cargo build --release --package tls-echo-server
```

The binary will be located at:
```
target/x86_64-unknown-linux-gnu/release/tls-echo-server
```

Or you can run it directly with:
```bash
cargo run --release --package tls-echo-server -- --help
```

## Creating Self-Signed Certificates

For testing purposes, you can create self-signed certificates. The following script creates:
- A Certificate Authority (CA)
- A server certificate signed by the CA
- A client certificate signed by the CA

```bash
#!/bin/bash

# Create a directory for certificates
mkdir -p certs
cd certs

# 1. Generate CA private key and certificate
openssl req -x509 -newkey rsa:4096 -sha256 -days 365 \
    -nodes -keyout ca.key -out ca.crt \
    -subj "/CN=Test CA/O=Kimojio Tests/C=US" \
    -addext "basicConstraints=critical,CA:TRUE"

# 2. Generate server private key
openssl genrsa -out server.key 4096

# 3. Create server certificate signing request (CSR)
openssl req -new -key server.key -out server.csr \
    -subj "/CN=server.unit.tests/O=Kimojio Tests/C=US"

# 4. Sign server certificate with CA
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key \
    -CAcreateserial -out server.crt -days 365 -sha256 \
    -extfile <(printf "subjectAltName=DNS:server.unit.tests,DNS:localhost\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth")

# 5. Generate client private key
openssl genrsa -out client.key 4096

# 6. Create client certificate signing request (CSR)
openssl req -new -key client.key -out client.csr \
    -subj "/CN=client.unit.tests/O=Kimojio Tests/C=US"

# 7. Sign client certificate with CA
openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key \
    -CAcreateserial -out client.crt -days 365 -sha256 \
    -extfile <(printf "subjectAltName=DNS:client.unit.tests\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=clientAuth")

# Clean up CSR files
rm server.csr client.csr

echo "Certificates created successfully!"
echo "  CA certificate:     ca.crt"
echo "  CA key:             ca.key"
echo "  Server certificate: server.crt"
echo "  Server key:         server.key"
echo "  Client certificate: client.crt"
echo "  Client key:         client.key"

cd ..
```

Save this script as `create-certs.sh`, make it executable with `chmod +x create-certs.sh`, and run it:

```bash
./create-certs.sh
```

This will create a `certs/` directory with all necessary certificate and key files.

## Running the Server

Run the TLS echo server with the generated certificates:

```bash
cargo run --release --package tls-echo-server -- \
    --cert certs/server.crt \
    --key certs/server.key \
    --ca-cert certs/ca.crt \
    --port 8443
```

Or using the binary directly:

```bash
./target/x86_64-unknown-linux-gnu/release/tls-echo-server \
    --cert certs/server.crt \
    --key certs/server.key \
    --ca-cert certs/ca.crt \
    --port 8443
```

Options:
- `--cert, -c <FILE>` - Server certificate file (PEM format) **[required]**
- `--key, -k <FILE>` - Server private key file (PEM format) **[required]**
- `--ca-cert <FILE>` - CA certificate for verifying client certificates **[required]**
- `--port, -p <PORT>` - Port to listen on (default: 8443)
- `--help, -h` - Display help information
- `--version, -V` - Display version information

### Example with default port

```bash
cargo run --release --package tls-echo-server -- \
    -c certs/server.crt \
    -k certs/server.key \
    --ca-cert certs/ca.crt
```

The server will start and display:
```
TLS Echo Server listening on 0.0.0.0:8443
Certificate: certs/server.crt
Private key: certs/server.key
CA certificate: certs/ca.crt
Waiting for connections...
```

## Testing the Server

### Using OpenSSL s_client

Test the server using OpenSSL's command-line client:

```bash
openssl s_client \
    -connect localhost:8443 \
    -cert certs/client.crt \
    -key certs/client.key \
    -CAfile certs/ca.crt \
    -tls1_3
```

Once connected, you can type messages and see them echoed back:

```
Hello, TLS Echo Server!
Hello, TLS Echo Server!
This is a test message.
This is a test message.
```

Press `Ctrl+D` to close the connection.

### Using curl (for HTTP-based testing)

If you want to test with curl (which sends HTTP-formatted data):

```bash
curl --cert certs/client.crt \
     --key certs/client.key \
     --cacert certs/ca.crt \
     --tls13 \
     --insecure \
     -v https://localhost:8443
```

Note: The server echoes raw bytes, so HTTP clients may not parse the response correctly, but you can see the data being sent and received in the verbose output.

### Using ncat (nmap's netcat)

If you have ncat installed:

```bash
ncat --ssl \
     --ssl-cert certs/client.crt \
     --ssl-key certs/client.key \
     --ssl-trustfile certs/ca.crt \
     localhost 8443
```

### Testing with a Simple Script

Create a test script using OpenSSL's s_client in batch mode:

```bash
#!/bin/bash
# test-echo.sh

echo "Testing TLS Echo Server..."
echo "Sending test message..."

echo "Hello from TLS client!" | openssl s_client \
    -connect localhost:8443 \
    -cert certs/client.crt \
    -key certs/client.key \
    -CAfile certs/ca.crt \
    -tls1_3 \
    -quiet 2>/dev/null

echo ""
echo "Test complete!"
```

Run it:
```bash
chmod +x test-echo.sh
./test-echo.sh
```

## Server Output

When a client connects, you'll see output like:

```
[Client 1] New connection established
[Client 1] TLS handshake completed
[Client 1] Received 27 bytes
[Client 1] Received 28 bytes
[Client 1] Connection closed by client
[Client 1] Handler completed
```

## Implementation Details

The server demonstrates several key features of kimojio:

1. **Async Socket Handling**: Uses `create_server_socket()` and `accept()` for non-blocking server operations
2. **TLS Context**: Creates a `TlsContext` from OpenSSL's `SslAcceptor` with TLS 1.3 and mutual authentication
3. **Stream Splitting**: Uses `split()` to separate read and write operations for cleaner code
4. **Task Spawning**: Spawns independent tasks for each client connection using `spawn_task()`
5. **Graceful Shutdown**: Properly shuts down TLS connections with `shutdown()` and `close()`

## Error Handling

The server includes comprehensive error handling:
- Certificate validation errors
- TLS handshake failures
- Connection errors
- Read/write errors

All errors are logged with the client ID for easy debugging.

## Troubleshooting

### Certificate Verification Failed

If you see TLS handshake errors, ensure:
- The server certificate is signed by the CA
- The client certificate is signed by the same CA
- The certificate files are in PEM format
- The certificates haven't expired

### Port Already in Use

If port 8443 is already in use, specify a different port:
```bash
cargo run --release --package tls-echo-server -- \
    --cert certs/server.crt \
    --key certs/server.key \
    --ca-cert certs/ca.crt \
    --port 9443
```

### Permission Denied

On Linux, ports below 1024 require root privileges. Use a port >= 1024 or run with sudo:
```bash
sudo ./target/x86_64-unknown-linux-gnu/release/tls-echo-server \
    --cert certs/server.crt \
    --key certs/server.key \
    --ca-cert certs/ca.crt \
    --port 443
```

## License

This example is part of the kimojio-rs project and is licensed under the MIT License.
