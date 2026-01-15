# copyfile - A dd-like File Copy Utility

This example demonstrates kimojio's async I/O capabilities by implementing a file copy utility
similar to `dd`. It showcases:

- **Polled I/O**: Using `pread_polled` and `pwrite_polled` for low-latency I/O operations
- **Direct I/O**: Bypassing the page cache for predictable performance (when using polled mode)
- **FuturesUnordered**: Managing multiple in-flight I/O operations dynamically
- **io_scope**: Ensuring proper cleanup and cancellation on errors

## Usage

```bash
# Basic copy
copyfile -i input.bin -o output.bin

# Copy with custom block size (default: 4096)
copyfile -i input.bin -o output.bin --block-size 65536

# Copy only N blocks
copyfile -i input.bin -o output.bin --count 100

# Use multiple in-flight I/O operations (default: 4)
copyfile -i input.bin -o output.bin --in-flight 8

# Use polled I/O (implies O_DIRECT)
copyfile -i input.bin -o output.bin --polled
```

## How It Works

The utility reads from the source file and writes to the destination file using kimojio's
`pread_polled` and `pwrite_polled` operations. Multiple I/O operations can be in flight
simultaneously to maximize throughput.

When `--polled` is specified:
- I/O operations use polling mode for lower latency
- Files are opened with `O_DIRECT` to bypass the page cache
- Block size must be aligned to 512 bytes (the utility enforces this)

The `FuturesUnordered` collection is used to track in-flight operations. As each operation
completes, a new one is issued (if there's more data to copy), maintaining the target
number of in-flight operations until all data is copied.

The `io_scope` wrapper allows us to avoid blocking the event loop in a situation where
I/O operation futures will be dropped.
