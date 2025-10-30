# kimojio

A thread-per-core Linux io_uring async runtime for Rust optimized for latency.

[Documentation](http://Azure.github.io/kimojio)

Kimojio uses a single-threaded, cooperatively scheduled runtime. Task scheduling is fast and consistent because tasks do not migrate between threads. This design works well for I/O-bound workloads with fine-grained tasks and minimal CPU-bound work.

Disk I/O in Kimojio is handled through io_uring, allowing asynchronous operations without relying on additional background threads. In some cases, the kernel may introduce a helper thread, but this is not part of the runtime itself.

Because Kimojio does not include automatic load balancing, developers have full control over concurrency and task distribution. This reduces implicit synchronization overhead but requires manual handling if multi-threaded execution is needed.

Key characteristics:

- Single-threaded, cooperative scheduling.
- Consistent task scheduling overhead.
- Asynchronous disk I/O via io_uring.
- Explicit control over concurrency and load balancing.
- No locks, atomics, or other thread synchronization

## Getting Started

### Prerequisites

Kimojio requires Linux Kernel of at least version 5.15 for sufficient I/O uring support.

### Installing

1. Add kimojio to your project:

    ```sh
    cargo add kimojio
    ```

### Hello World

```rust
use kimojio::{
    Errno,
    configuration::Configuration,
    operations::{self, OFlags},
};

async fn kimojio_main() -> Result<(), Errno> {
    let flags = OFlags::CREATE | OFlags::RDWR;
    let fd = operations::open(c"/tmp/example.txt", flags, 0o644.into()).await?;
    let written = operations::write(&fd, b"hello world").await?;
    assert_eq!(written, 11);
    operations::close(fd).await?;
    Ok(())
}

pub fn main() {
    let result = kimojio::run_with_configuration(0, kimojio_main(), Configuration::new());
    result
        .expect("shutdown_loop called")
        .expect("run panicked")
        .expect("run failed");
}
```