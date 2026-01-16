// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A dd-like file copy utility demonstrating kimojio's async I/O capabilities.
//!
//! This example shows how to:
//! - Use `pread_polled` and `pwrite_polled` for efficient file I/O
//! - Manage multiple in-flight I/O operations with `FuturesUnordered`
//! - Use `io_scope` for proper cleanup on errors
//! - Use O_DIRECT mode with polled I/O for low-latency operations
//!
//! The utility copies data from a source file to a destination file, allowing
//! configuration of block size, number of blocks, in-flight I/O count, and
//! whether to use polled I/O mode.

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use kimojio::operations::{self, io_scope, io_scope_drain_futures, OFlags};
use rustix::fs::Mode;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;

/// Polled I/O mode selection.
/// Polled I/O avoids interrupt overhead but requires O_DIRECT (aligned buffers).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum PolledMode {
    /// No polled I/O - use standard interrupt-driven completion
    #[default]
    None,
    /// Use polled I/O for reads only
    Read,
    /// Use polled I/O for writes only
    Write,
    /// Use polled I/O for both reads and writes
    Both,
}

impl PolledMode {
    /// Returns true if reads should use polled I/O
    pub fn poll_reads(self) -> bool {
        matches!(self, PolledMode::Read | PolledMode::Both)
    }

    /// Returns true if writes should use polled I/O
    pub fn poll_writes(self) -> bool {
        matches!(self, PolledMode::Write | PolledMode::Both)
    }

    /// Returns true if any polled I/O is enabled (requires O_DIRECT)
    pub fn any_polled(self) -> bool {
        self != PolledMode::None
    }
}

/// Type alias for a boxed future that returns an IoBuffer or an error.
/// This simplifies the type annotations for FuturesUnordered and improves readability.
/// Each future performs a read-then-write operation for one block.
type CopyBlockResult =
    Pin<Box<dyn std::future::Future<Output = Result<CopyResult, kimojio::Errno>>>>;

/// Result of copying a single block.
/// Contains the buffer (for reuse) and information about what was copied.
struct CopyResult {
    /// The buffer that was used, returned for reuse
    buffer: IoBuffer,
    /// Number of bytes that were copied (0 means EOF was reached)
    bytes_copied: usize,
}

/// A dd-like file copy utility demonstrating kimojio's async I/O
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the source (input) file
    #[arg(short = 'i', long, value_name = "FILE")]
    input: PathBuf,

    /// Path to the destination (output) file
    #[arg(short = 'o', long, value_name = "FILE")]
    output: PathBuf,

    /// Block size in bytes for each I/O operation.
    /// When using --polled, this must be a multiple of 512 for O_DIRECT alignment.
    #[arg(short = 'b', long, default_value_t = 4096)]
    block_size: usize,

    /// Number of blocks to copy. If not specified, copies until EOF.
    #[arg(short = 'c', long)]
    count: Option<u64>,

    /// Maximum number of I/O operations in flight at once.
    /// Higher values can improve throughput but use more memory.
    #[arg(short = 'f', long, default_value_t = 4)]
    in_flight: usize,

    /// Use polled I/O mode for lower latency.
    /// Options: none, read, write, both.
    /// Polled I/O implies O_DIRECT mode, which requires block-aligned I/O.
    #[arg(short = 'p', long, value_enum, default_value_t = PolledMode::None)]
    polled: PolledMode,

    /// Print io_uring statistics after the copy completes.
    /// Shows metrics like max in-flight I/O, task polls, etc.
    #[arg(short = 's', long)]
    stats: bool,
}

/// Represents a single I/O buffer that will be used for a read-then-write cycle.
/// We track the block index to know where to write the data after reading.
/// Buffers are always aligned to 512 bytes to support O_DIRECT mode.
struct IoBuffer {
    /// The actual buffer holding the data.
    /// Always allocated with 512-byte alignment to support O_DIRECT.
    data: Box<[u8]>,
    /// The block index this buffer is processing (used to calculate file offset)
    block_index: u64,
}

/// Allocates aligned memory for I/O operations.
///
/// We always allocate aligned memory (512-byte alignment) so that
/// buffers work with O_DIRECT mode. This simplifies the code by
/// avoiding conditional allocation paths.
///
/// O_DIRECT requires that:
/// 1. The buffer address is aligned (typically to 512 bytes or the filesystem block size)
/// 2. The I/O size is a multiple of the alignment
/// 3. The file offset is a multiple of the alignment
fn allocate_aligned_buffer(size: usize, alignment: usize) -> Box<[u8]> {
    // Use the Layout API to allocate memory with specific alignment
    let layout = std::alloc::Layout::from_size_align(size, alignment)
        .expect("Invalid layout for aligned buffer");

    // SAFETY: We're allocating memory with a valid layout and will properly
    // initialize it before use. The memory is managed by Box.
    unsafe {
        let ptr = std::alloc::alloc_zeroed(layout);
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        // Create a slice from the raw pointer and wrap it in a Box
        Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, size))
    }
}

/// Creates an IoBuffer with aligned memory.
/// Always uses 512-byte alignment to support O_DIRECT mode.
fn create_buffer(block_size: usize) -> IoBuffer {
    // 512-byte alignment is the minimum for O_DIRECT on most filesystems.
    // We always use aligned buffers to simplify the code and ensure
    // compatibility with O_DIRECT regardless of the polled mode setting.
    const DIRECT_IO_ALIGNMENT: usize = 512;
    IoBuffer {
        data: allocate_aligned_buffer(block_size, DIRECT_IO_ALIGNMENT),
        block_index: 0,
    }
}

/// The main copy operation that uses io_scope to ensure proper cleanup.
///
/// This function demonstrates several important patterns:
///
/// 1. **io_scope**: Wraps the entire I/O operation to ensure that if any error
///    occurs, all pending I/O is cancelled and completed before returning.
///    This prevents performance issues by ensuring that canceled I/O do not
///    block the event loop when the scope is exited, which happens by default
///    when a kimojio I/O futures is dropped.
///
/// 2. **FuturesUnordered**: Unlike FuturesOrdered, this collection allows
///    futures to complete in any order. This is more efficient for I/O because
///    we don't need results in order - we just need to process them as they
///    complete. This maximizes I/O throughput by never blocking on a slow
///    operation when faster ones are ready.
///
/// 3. **Dynamic queue management**: We maintain a target number of in-flight
///    operations. As each operation completes, we immediately issue a new one
///    (if there's more work to do) to keep the I/O pipeline full.
///
/// 4. **pread_polled/pwrite_polled**: These functions allow us to optionally
///    use polling mode for I/O completion. Polling avoids the overhead of
///    interrupts and context switches, which can reduce latency for fast
///    storage devices like NVMe SSDs.
///
/// 5. **Read-then-write futures**: Each future in FuturesUnordered performs
///    both the read and write for a single block. This means neither reads
///    nor writes block the main loop - all I/O happens concurrently.
async fn copy_file(
    src_fd: Rc<operations::OwnedFd>,
    dst_fd: Rc<operations::OwnedFd>,
    block_size: usize,
    max_blocks: Option<u64>,
    in_flight: usize,
    polled: PolledMode,
) -> Result<u64> {
    // io_scope allows to ensure that in a situation where we will drop in
    // flight futures, we can cancel them without blocking the event loop.
    io_scope(async move || {
        // Track the total bytes copied for reporting
        let mut total_bytes_copied: u64 = 0;

        // Track which block we'll issue next
        let mut next_block: u64 = 0;

        // Flag to indicate we've reached EOF or copied all requested blocks
        let mut copy_complete = false;

        // Pool of available buffers for I/O operations.
        // We pre-allocate these to avoid allocation during the copy loop.
        // Using a pool lets us reuse buffers efficiently.
        // Buffers are always aligned to support O_DIRECT mode.
        let mut buffer_pool: Vec<IoBuffer> = Vec::with_capacity(in_flight);
        for _ in 0..in_flight {
            buffer_pool.push(create_buffer(block_size));
        }

        // FuturesUnordered is a collection of futures that can complete in any order.
        // Unlike join_all (which waits for all) or FuturesOrdered (which preserves order),
        // FuturesUnordered lets us process completions as they happen.
        //
        // Each future in this collection performs a complete read-then-write cycle
        // for one block. This design has several advantages:
        // - Neither reads nor writes block the main loop
        // - The state machine remains simple (no need to track read vs write state)
        // - Maximum parallelism for both read and write operations
        //
        // The workflow is:
        // 1. Take a buffer from the pool and assign it a block index
        // 2. Create a future that reads the block, then writes it
        // 3. Add the future to FuturesUnordered
        // 4. When a future completes, recycle its buffer for the next block
        let mut in_flight_ops: FuturesUnordered<CopyBlockResult> = FuturesUnordered::new();

        // Helper to check if we should issue more block copies
        let should_copy_more =
            |next_block: u64, copy_complete: bool, max_blocks: Option<u64>| -> bool {
                if copy_complete {
                    return false;
                }
                match max_blocks {
                    Some(max) => next_block < max,
                    None => true, // No limit, keep copying until EOF
                }
            };

        // Main copy loop: we issue read+write operations and process completions.
        // The strategy is:
        // 1. Issue as many copy operations as we have buffers for
        // 2. Wait for any operation to complete
        // 3. If it succeeded, recycle the buffer and issue the next operation
        // 4. If it hit EOF, mark copy as complete and wait for remaining ops
        // 5. If it failed, cancel all operations and return the error
        loop {
            // First, try to start new copy operations while we have buffers available
            // and there's more data to copy.
            while !buffer_pool.is_empty() && should_copy_more(next_block, copy_complete, max_blocks)
            {
                // Get a buffer from the pool and assign it the next block index
                let mut buffer = buffer_pool.pop().unwrap();
                buffer.block_index = next_block;
                let block_index = next_block;
                next_block += 1;

                // Clone the file descriptors for the async block
                let src_fd_clone = src_fd.clone();
                let dst_fd_clone = dst_fd.clone();

                // Extract polled flags for use in the async block
                let poll_reads = polled.poll_reads();
                let poll_writes = polled.poll_writes();

                // Create a future that performs both read and write for this block.
                // This approach means:
                // - The main loop doesn't block on either reads or writes
                // - Each block is handled independently
                // - We get maximum parallelism from FuturesUnordered
                let copy_future = Box::pin(async move {
                    // Calculate the file offset for this block
                    let offset = block_index * block_size as u64;

                    // Step 1: Read the block from the source file
                    // pread_polled reads at a specific offset without changing file position
                    let bytes_read = operations::pread_polled(
                        src_fd_clone.as_ref(),
                        &mut buffer.data,
                        offset,
                        poll_reads,
                    )
                    .await?;

                    // Check for EOF - if we read 0 bytes, there's nothing to write
                    if bytes_read == 0 {
                        return Ok(CopyResult {
                            buffer,
                            bytes_copied: 0,
                        });
                    }

                    // Step 2: Write the block to the destination file
                    // We only write the bytes that were actually read (important for last block)
                    operations::pwrite_polled(
                        dst_fd_clone.as_ref(),
                        &buffer.data[..bytes_read],
                        offset,
                        poll_writes,
                    )
                    .await?;

                    // Return the result with the buffer for reuse
                    Ok(CopyResult {
                        buffer,
                        bytes_copied: bytes_read,
                    })
                });

                in_flight_ops.push(copy_future);
            }

            // Wait for the next operation to complete.
            // FuturesUnordered returns results as they complete, not in submission order.
            // This is more efficient because we never wait for a slow operation when
            // faster ones are ready.
            match in_flight_ops.next().await {
                Some(Ok(result)) => {
                    if result.bytes_copied == 0 {
                        // This block hit EOF. Mark copy as complete so we don't
                        // issue more operations, but continue processing any
                        // already in-flight operations.
                        copy_complete = true;
                    } else {
                        // Block copied successfully. Add the bytes to our total.
                        total_bytes_copied += result.bytes_copied as u64;
                    }
                    // Return the buffer to the pool for reuse
                    buffer_pool.push(result.buffer);
                }
                Some(Err(e)) => {
                    // An operation failed. We need to cancel all remaining operations
                    // and return the error. io_scope_drain_futures will:
                    // 1. Cancel all pending I/O in the current io_scope
                    // 2. Wait for all futures to complete (even if cancelled)
                    // 3. Return the first error
                    io_scope_drain_futures(
                        Err(e),
                        in_flight_ops,
                        |(), _result| (), // Ignore successful completions
                    )
                    .await?;
                    unreachable!("io_scope_drain_futures returned Ok after Err");
                }
                None => {
                    // All done
                    break;
                }
            }
        }

        Ok(total_bytes_copied)
    })
    .await
}

/// Opens a file for reading with optional O_DIRECT flag.
///
/// O_DIRECT bypasses the kernel's page cache, which is useful when:
/// - You're doing your own caching (we're not, but polled I/O benefits from it)
/// - You want predictable I/O latency (no cache eviction surprises)
/// - You're copying large files and don't want to pollute the cache
///
/// The downside is that O_DIRECT requires aligned I/O operations.
async fn open_source_file(path: &str, direct: bool) -> Result<operations::OwnedFd> {
    let c_path = std::ffi::CString::new(path).context("Invalid path")?;
    let mut flags = OFlags::RDONLY;
    if direct {
        // O_DIRECT tells the kernel to bypass the page cache.
        // This requires that I/O is properly aligned (buffer, size, and offset).
        flags |= OFlags::DIRECT;
    }
    operations::open(&c_path, flags, Mode::empty())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to open source file '{}': {}", path, e))
}

/// Opens or creates a file for writing with optional O_DIRECT flag.
async fn open_dest_file(path: &str, direct: bool) -> Result<operations::OwnedFd> {
    let c_path = std::ffi::CString::new(path).context("Invalid path")?;
    // CREATE: Create the file if it doesn't exist
    // TRUNC: Truncate existing file to zero length
    // WRONLY: Open for writing only
    let mut flags = OFlags::CREATE | OFlags::TRUNC | OFlags::WRONLY;
    if direct {
        flags |= OFlags::DIRECT;
    }
    // Mode 0o644 = rw-r--r-- (owner read/write, group/others read-only)
    operations::open(&c_path, flags, Mode::from_raw_mode(0o644))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to open destination file '{}': {}", path, e))
}

/// Main entry point that runs the copy operation.
async fn run_copy(args: Args) -> Result<()> {
    // Validate arguments
    if args.in_flight == 0 {
        bail!("in-flight count must be at least 1");
    }

    if args.block_size == 0 {
        bail!("block size must be at least 1");
    }

    // When using polled I/O (which implies O_DIRECT), block size must be aligned
    // to the filesystem's logical block size (typically 512 bytes).
    if args.polled.any_polled() && !args.block_size.is_multiple_of(512) {
        bail!(
            "block size must be a multiple of 512 bytes when using polled I/O (got {})",
            args.block_size
        );
    }

    let input_path = args
        .input
        .to_str()
        .context("Input path contains invalid UTF-8")?;
    let output_path = args
        .output
        .to_str()
        .context("Output path contains invalid UTF-8")?;

    println!("Copying '{}' to '{}'", input_path, output_path);
    println!("  Block size: {} bytes", args.block_size);
    println!(
        "  Blocks to copy: {}",
        args.count
            .map(|c| c.to_string())
            .unwrap_or_else(|| "all".to_string())
    );
    println!("  Max in-flight I/O: {}", args.in_flight);
    println!(
        "  Polled I/O: {:?} (O_DIRECT: {})",
        args.polled,
        args.polled.any_polled()
    );

    // Open files with O_DIRECT if using any polled I/O
    let use_direct_read = args.polled.poll_reads();
    let src_fd = Rc::new(
        open_source_file(input_path, use_direct_read)
            .await
            .context("Failed to open source file")?,
    );
    let use_direct_write = args.polled.poll_writes();
    let dst_fd = Rc::new(
        open_dest_file(output_path, use_direct_write)
            .await
            .context("Failed to open destination file")?,
    );

    // Perform the copy
    let start_time = std::time::Instant::now();
    let bytes_copied = copy_file(
        src_fd.clone(),
        dst_fd.clone(),
        args.block_size,
        args.count,
        args.in_flight,
        args.polled,
    )
    .await?;
    let elapsed = start_time.elapsed();

    // Ensure all data is flushed to disk.
    // For O_DIRECT, data goes directly to the device, but metadata (file size, etc.)
    // may still be cached. fsync ensures everything is persisted.
    operations::fsync(dst_fd.as_ref())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to sync destination file: {}", e))?;

    // Report results
    let elapsed_secs = elapsed.as_secs_f64();
    let throughput = if elapsed_secs > 0.0 {
        (bytes_copied as f64) / elapsed_secs / 1_000_000.0
    } else {
        0.0
    };

    println!("\nCopy complete:");
    println!("  Bytes copied: {}", bytes_copied);
    println!("  Time elapsed: {:.3} seconds", elapsed_secs);
    println!("  Throughput: {:.2} MB/s", throughput);

    // Print io_uring statistics if requested.
    // These stats help understand the runtime's behavior and can be useful for
    // performance tuning. For example, max_in_flight_io shows how well we're
    // utilizing the configured in-flight limit.
    if args.stats {
        let task_state = kimojio::task::TaskState::get();
        let stats = &task_state.stats;
        println!("\nio_uring statistics:");
        println!("  Max in-flight I/O: {}", stats.max_in_flight_io.get());
        println!("  Current in-flight I/O: {}", stats.in_flight_io.get());
        println!(
            "  Max in-flight polled I/O: {}",
            stats.max_in_flight_io_poll.get()
        );
        println!(
            "  Current in-flight polled I/O: {}",
            stats.in_flight_io_poll.get()
        );
        println!(
            "  Tasks polled (I/O priority): {}",
            stats.tasks_polled_io.get()
        );
        println!(
            "  Tasks polled (CPU priority): {}",
            stats.tasks_polled_cpu.get()
        );
    }

    Ok(())
}

/// Sets up kimojio runtime and runs the copy operation.
#[kimojio::main]
async fn main() -> Result<(), kimojio::Errno> {
    let args = Args::parse();

    // Verify input file exists
    if !args.input.exists() {
        eprintln!("Error: Input file not found: {}", args.input.display());
        std::process::exit(1);
    }

    // Run the copy operation and handle any errors
    if let Err(e) = run_copy(args).await {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }

    Ok(())
}
