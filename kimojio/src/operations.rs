// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! This file implements the futures that I/O and utility functionality
//! for the uringruntime.
//!
//! operations calls need to associate their input lifetimes with the RingFuture
//! lifetime to ensure that the inputs are not dropped before the future. The
//! following example checks this for operations::write.
//!
//! For futures that get dropped, we cancel the I/O and black the thread
//! processing completions until the I/O is fully cancelled to ensure no
//! access-after-free issues. This will stall polling of tasks. To avoid
//! that, use io_scope for scenarios where dropping RingFuture is expected.
//!
//! ```rust,compile_fail
//! use kimojio::{
//!     Errno,
//!     configuration::Configuration,
//!     operations::{self, OFlags},
//! };
//! async fn kimojio_main() -> Result<(), Errno> {
//!     let flags = OFlags::CREATE | OFlags::RDWR;
//!     let fd = operations::open(c"/tmp/example.txt", flags, 0o644.into()).await?;
//!     let buffer = b"I am ub!".to_vec();
//!     let fut = operations::write(&fd, &buffer);
//!     drop(buffer);
//!     let written = fut.await?;
//!     Ok(())
//! }

//! ```
//!
use std::future::Future;
use std::io::IoSlice;
use std::marker::PhantomData;
use std::mem::{size_of, size_of_val};
use std::net::{SocketAddr, SocketAddrV6};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Instant;

use futures::future::FusedFuture;
use futures::{FutureExt, Stream, StreamExt};
use libc::{AF_INET, AF_INET6, AF_UNIX, sa_family_t, sockaddr_in, sockaddr_in6, sockaddr_un};
use rustix::fs::AtFlags;
use rustix::io_uring::{Mode, MsgHdr, SocketAddrOpaque, iovec};

pub use rustix::fd::OwnedFd;
pub use rustix::io_uring::Advice;
pub use rustix::net::{AddressFamily, Protocol, SocketType, ipproto};
pub use rustix::net::{RecvFlags, SendFlags, SocketAddrUnix};
pub use rustix_uring::types::{OFlags, Statx};

use crate::async_event::TaskSource;
use crate::io_type::IOType;
use crate::ring_future::{OwnedFdFuture, UnitFuture, UsizeFuture};
use crate::task::{IoScopeCompletions, Task, TaskReadyState, TaskState};
use crate::task_ref::wake_task;
use crate::tracing::Events;
use crate::{
    CanceledError, Completion, CompletionResources, CompletionState, Errno, TaskHandleError,
};
use rustix::fd::{AsFd, AsRawFd};
use rustix_uring::{
    opcode,
    types::{Fd, Timespec},
};
use std::{ffi::CStr, mem::ManuallyDrop, net::SocketAddrV4, time::Duration};

#[cfg(feature = "io_uring_cmd")]
use crate::ring_future::UringCmdFuture;

pub trait OperationFuture<T>: Future<Output = Result<T, Errno>> + FusedFuture {}
impl<T, R: Future<Output = Result<T, Errno>> + FusedFuture> OperationFuture<T> for R {}

// TODO: splice, tee, poll, cancel, epoll

/// Opens a file at the given path.
///
/// Opens or creates the file specified by `filename` with the given `flags` and `mode`.
/// This is equivalent to the `openat(2)` system call with `AT_FDCWD` as the directory fd.
///
/// Returns a future that resolves to the opened file descriptor, or an error.
pub fn open(filename: &CStr, flags: OFlags, mode: Mode) -> OwnedFdFuture<'_> {
    let dirfd = Fd(libc::AT_FDCWD);
    OwnedFdFuture::new(
        opcode::OpenAt::new(dirfd, filename.as_ptr())
            .flags(flags)
            .mode(mode)
            .build(),
        -1,
        None,
        IOType::Open,
    )
}

/// Creates a hard link to an existing file.
///
/// Creates a new hard link `newpath` that refers to the same file as `oldpath`.
/// This is equivalent to the `linkat(2)` system call.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn link<'a>(oldpath: &'a CStr, newpath: &'a CStr) -> UnitFuture<'a> {
    let dirfd = Fd(libc::AT_FDCWD);
    UnitFuture::new(
        opcode::LinkAt::new(dirfd, oldpath.as_ptr(), dirfd, newpath.as_ptr()).build(),
        -1,
        None,
        IOType::Link,
    )
}

/// Creates a symbolic link.
///
/// Creates a symbolic link at `linkpath` that points to `target`.
/// This is equivalent to the `symlinkat(2)` system call.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn symlink<'a>(target: &'a CStr, linkpath: &'a CStr) -> UnitFuture<'a> {
    let dirfd = Fd(libc::AT_FDCWD);
    UnitFuture::new(
        opcode::SymlinkAt::new(dirfd, target.as_ptr(), linkpath.as_ptr()).build(),
        -1,
        None,
        IOType::Symlink,
    )
}

/// Creates a new directory.
///
/// Creates a new directory at `pathname` with the specified permissions.
/// This is equivalent to the `mkdirat(2)` system call.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn mkdir(pathname: &CStr, mode: Mode) -> UnitFuture<'_> {
    let dirfd = Fd(libc::AT_FDCWD);
    UnitFuture::new(
        opcode::MkDirAt::new(dirfd, pathname.as_ptr())
            .mode(mode)
            .build(),
        -1,
        None,
        IOType::Mkdir,
    )
}

/// Removes an empty directory.
///
/// Removes the directory at `pathname`. The directory must be empty.
/// This is equivalent to `unlinkat(2)` with the `AT_REMOVEDIR` flag.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn rmdir(pathname: &CStr) -> UnitFuture<'_> {
    let dirfd = Fd(libc::AT_FDCWD);
    UnitFuture::new(
        opcode::UnlinkAt::new(dirfd, pathname.as_ptr())
            .flags(AtFlags::REMOVEDIR)
            .build(),
        -1,
        None,
        IOType::Unlink,
    )
}

/// Retrieves file status information for a path.
///
/// Returns metadata about the file at `filename`, including size, permissions,
/// timestamps, and other attributes. This is equivalent to the `statx(2)` system call.
///
/// Returns a `Statx` structure containing the file's metadata, or an error.
pub async fn stat(filename: &CStr) -> Result<Statx, Errno> {
    let mut statx = std::mem::MaybeUninit::<Statx>::uninit();
    stat_internal(filename, statx.as_mut_ptr()).await?;
    Ok(unsafe { statx.assume_init() })
}

fn stat_internal<'a>(filename: &'a CStr, statx: *mut Statx) -> UnitFuture<'a> {
    let dirfd = Fd(libc::AT_FDCWD);
    UnitFuture::new(
        opcode::Statx::new(dirfd, filename.as_ptr(), statx).build(),
        -1,
        None,
        IOType::Stat,
    )
}

/// Retrieves file status information for an open file descriptor.
///
/// Returns metadata about the file referred to by `fd`, including size, permissions,
/// timestamps, and other attributes. This is equivalent to the `statx(2)` system call
/// with `AT_EMPTY_PATH`.
///
/// Returns a `Statx` structure containing the file's metadata, or an error.
pub async fn fstat(fd: &impl AsFd) -> Result<Statx, Errno> {
    let mut statx = std::mem::MaybeUninit::<Statx>::uninit();
    fstat_internal(fd, statx.as_mut_ptr()).await?;
    Ok(unsafe { statx.assume_init() })
}

/// statx(2) - fstat
/// AT_EMPTY_PATH: If pathname is an empty string, operate on the file referred to by dirfd
fn fstat_internal<'a>(fd: &impl AsFd, statx: *mut Statx) -> UnitFuture<'a> {
    let fd = Fd(fd.as_fd().as_raw_fd());
    let empty_path = c"";
    let statx_op = opcode::Statx::new(fd, empty_path.as_ptr(), statx)
        .flags(AtFlags::EMPTY_PATH)
        .build();
    UnitFuture::new(statx_op, -1, None, IOType::Stat)
}

/// Removes a file.
///
/// Deletes the file at `filename`. This is equivalent to the `unlinkat(2)` system call.
/// If the file has other hard links, the file data remains until all links are removed.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn unlink(filename: &CStr) -> UnitFuture<'_> {
    let dirfd = Fd(libc::AT_FDCWD);
    UnitFuture::new(
        opcode::UnlinkAt::new(dirfd, filename.as_ptr()).build(),
        -1,
        None,
        IOType::Unlink,
    )
}

/// Renames a file or directory.
///
/// Moves or renames `oldpath` to `newpath`. If `newpath` already exists, it will be
/// atomically replaced. This is equivalent to the `renameat(2)` system call.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn rename<'a>(oldpath: &'a CStr, newpath: &'a CStr) -> UnitFuture<'a> {
    let dirfd = Fd(libc::AT_FDCWD);
    UnitFuture::new(
        opcode::RenameAt::new(dirfd, oldpath.as_ptr(), dirfd, newpath.as_ptr()).build(),
        -1,
        None,
        IOType::Rename,
    )
}

/// Provides advice to the kernel about file access patterns.
///
/// Announces an intention to access file data in a specific pattern, allowing the kernel
/// to optimize accordingly. This is equivalent to the `fadvise64(2)` system call.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn fadvise(fd: &impl AsFd, offset: u64, len: u64, advice: Advice) -> UnitFuture<'_> {
    let fd = fd.as_fd().as_raw_fd();
    UnitFuture::new(
        opcode::Fadvise::new(Fd(fd), len as u32, advice)
            .offset(offset)
            .build(),
        fd,
        None,
        IOType::FAdvise,
    )
}

/// Provides advice to the kernel about memory access patterns.
///
/// Announces an intention to access memory in a specific pattern, allowing the kernel
/// to optimize accordingly. This is equivalent to the `madvise(2)` system call.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn madvise(addr: *const libc::c_void, len: u64, advice: Advice) -> UnitFuture<'static> {
    UnitFuture::new(
        opcode::Madvise::new(addr, len as u32, advice).build(),
        -1,
        None,
        IOType::MAdvise,
    )
}

/// Pre-allocates or deallocates space for a file.
///
/// Manipulates the allocated disk space for the file. This can be used to pre-allocate
/// space to avoid fragmentation, or to deallocate space (punch holes). This is equivalent
/// to the `fallocate(2)` system call.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn fallocate(fd: &impl AsFd, mode: i32, offset: u64, len: u64) -> UnitFuture<'_> {
    let fd = fd.as_fd().as_raw_fd();
    UnitFuture::new(
        opcode::Fallocate::new(Fd(fd), len)
            .mode(mode)
            .offset(offset)
            .build(),
        fd,
        None,
        IOType::FAllocate,
    )
}

/// Creates a new socket.
///
/// Creates a socket with the specified domain, type, and protocol. If the kernel supports
/// `io_uring` socket creation, it will be used; otherwise, falls back to synchronous creation.
///
/// Returns the created socket file descriptor, or an error.
pub async fn socket(
    domain: AddressFamily,
    socket_type: SocketType,
    protocol: Option<Protocol>,
) -> Result<OwnedFd, Errno> {
    let task_state = TaskState::get();
    if task_state.probe.is_supported(opcode::Socket::CODE) {
        let domain = u32::from(domain.as_raw()) as i32;
        let socket_type = socket_type.as_raw() as i32;
        let protocol = match protocol {
            Some(p) => u32::from(p.as_raw()) as i32,
            None => 0i32,
        };
        // OwnedFdFuture internally will access task_state
        drop(task_state);
        let socket_fut = OwnedFdFuture::new(
            opcode::Socket::new(domain, socket_type, protocol).build(),
            -1,
            None,
            IOType::Socket,
        );
        socket_fut.await
    } else {
        // Our kernel does not yet support the api call.  Lets leave this function async anyway for
        // future proof
        rustix::net::socket(domain, socket_type, protocol)
    }
}

/// Accepts an incoming connection on a listening socket.
///
/// Extracts the first pending connection from the queue of pending connections
/// for the listening socket and returns a new file descriptor for the accepted socket.
/// This is equivalent to the `accept(2)` system call.
///
/// Returns a future that resolves to the new connected socket file descriptor, or an error.
pub fn accept(fd: &impl AsFd) -> OwnedFdFuture<'_> {
    let fd = fd.as_fd().as_raw_fd();
    OwnedFdFuture::new(
        opcode::Accept::new(Fd(fd), std::ptr::null_mut(), std::ptr::null_mut()).build(),
        fd,
        None,
        IOType::Accept,
    )
}

/// Shuts down part or all of a full-duplex connection.
///
/// Causes all or part of a full-duplex connection on the socket to be shut down.
/// This is equivalent to the `shutdown(2)` system call.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn shutdown(fd: &impl AsFd, how: i32) -> UnitFuture<'_> {
    let fd = fd.as_fd().as_raw_fd();
    UnitFuture::new(
        opcode::Shutdown::new(Fd(fd), how).build(),
        fd,
        None,
        IOType::Shutdown,
    )
}

/// Synchronizes a file's in-core state with storage.
///
/// Transfers all modified data and metadata of the file to the underlying storage device.
/// This ensures data durability. This is equivalent to the `fsync(2)` system call.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn fsync(fd: &impl AsFd) -> UnitFuture<'_> {
    let fd = fd.as_fd().as_raw_fd();
    UnitFuture::new(opcode::Fsync::new(Fd(fd)).build(), fd, None, IOType::Fsync)
}

/// Synchronizes a range of a file's data with storage.
///
/// Synchronizes a specific range of the file to the underlying storage device.
/// This is more efficient than `fsync` when only part of the file needs to be synchronized.
/// This is equivalent to the `sync_file_range(2)` system call.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn sync_file_range(fd: &impl AsFd, offset: u64, len: u32) -> UnitFuture<'_> {
    let fd = fd.as_fd().as_raw_fd();
    UnitFuture::new(
        opcode::SyncFileRange::new(Fd(fd), len)
            .offset(offset)
            .build(),
        fd,
        None,
        IOType::SyncFileRange,
    )
}

/// Binds a socket to a local address.
///
/// Assigns the address specified by `address` to the socket. This is a synchronous
/// operation using `rustix::net::bind`.
///
/// Returns `Ok(())` on success, or an error.
pub fn bind(fd: &impl AsFd, address: &SocketAddr) -> Result<(), Errno> {
    rustix::net::bind(fd, address)
}

/// Marks a socket as a passive socket for accepting connections.
///
/// Marks the socket as a passive socket that will be used to accept incoming
/// connections using `accept()`. This is a synchronous operation using `rustix::net::listen`.
///
/// Returns `Ok(())` on success, or an error.
pub fn listen(fd: &impl AsFd, backlog: i32) -> Result<(), Errno> {
    rustix::net::listen(fd, backlog)
}

fn sockaddr_from_socketaddr(addr: &SocketAddrV4) -> sockaddr_in {
    sockaddr_in {
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(addr.ip().octets()),
        },
        sin_port: u16::to_be(addr.port()),
        sin_family: AF_INET as u16,
        sin_zero: [0; 8],
    }
}

fn sockaddr6_from_socketaddrv6(addr: &SocketAddrV6) -> sockaddr_in6 {
    sockaddr_in6 {
        sin6_addr: libc::in6_addr {
            s6_addr: addr.ip().octets(),
        },
        sin6_family: AF_INET6 as u16,
        sin6_flowinfo: addr.flowinfo(),
        sin6_port: u16::to_be(addr.port()),
        sin6_scope_id: addr.scope_id(),
    }
}

/// Simple utility function to convert from abstract SocketAddrUnix
/// representation into a sockaddr_un struct. The connect() syscall takes a
/// pointer to a sockaddr struct which is type punned from sockaddr_* + an addr
/// length value. SocketAddrUnix needs two pieces of information extracted:
/// 1) The raw sockaddr_un struct with sun_path filled in based on the address
/// 2) The length of the address, the length is variable based on path length
///
/// Will panic() if the address is neither a valid path nor a valid abstract
/// name which can only happen in programmer error to construct a
/// SocketAddrUnix that is not correctly initialized
fn sockaddr_from_socketaddr_unix(addr: &SocketAddrUnix) -> (sockaddr_un, usize) {
    #[cfg(target_arch = "x86_64")]
    let mut sun_path = [0i8; 108];
    #[cfg(not(target_arch = "x86_64"))]
    let mut sun_path = [0u8; 108];
    let addrlen = if let Some(addr) = addr.abstract_name() {
        // Safety: Transmute from &[u8] -> &[i8]
        #[cfg(target_arch = "x86_64")]
        let addr = unsafe { core::slice::from_raw_parts(addr.as_ptr().cast::<i8>(), addr.len()) };

        // Abstract names place a null byte at index 0 and the rest of the
        // identifier after the null byte
        sun_path[1..1 + addr.len()].copy_from_slice(addr);
        addr.len() + 1 + size_of::<sa_family_t>()
    } else if let Some(addr) = addr.path() {
        let addr = addr.to_bytes();
        // Safety: Transmute from &[u8] -> &[i8]
        #[cfg(target_arch = "x86_64")]
        let addr = unsafe { core::slice::from_raw_parts(addr.as_ptr().cast::<i8>(), addr.len()) };

        sun_path[0..addr.len()].copy_from_slice(addr);
        addr.len() + size_of::<sa_family_t>()
    } else {
        panic!("Impossible for Unix Socket Address to be neither path nor abstract name");
    };

    (
        sockaddr_un {
            sun_family: AF_UNIX as u16,
            sun_path,
        },
        addrlen,
    )
}

/// Connects a Unix domain socket to a peer.
///
/// Initiates a connection on the Unix domain socket to the address specified by `addr`.
/// This is used for connecting to Unix domain sockets (both path-based and abstract).
///
/// Returns `Ok(())` on successful connection, or an error.
pub async fn connect_unix(fd: &impl AsFd, addr: &SocketAddrUnix) -> Result<(), Errno> {
    let fd = fd.as_fd().as_raw_fd();
    let (addr, addrlen) = sockaddr_from_socketaddr_unix(addr);
    let addr = core::ptr::addr_of!(addr) as *const SocketAddrOpaque;
    UnitFuture::new(
        opcode::Connect::new(Fd(fd), addr, addrlen as u32).build(),
        fd,
        None,
        IOType::Connect,
    )
    .await
}

/// Connects a socket to a peer address.
///
/// Initiates a connection on the socket to the address specified by `addr`.
/// Supports both IPv4 and IPv6 addresses.
///
/// Returns `Ok(())` on successful connection, or an error.
pub async fn connect(fd: &impl AsFd, addr: &SocketAddr) -> Result<(), Errno> {
    let fd = fd.as_fd().as_raw_fd();
    match addr {
        SocketAddr::V4(addr) => {
            let addr = sockaddr_from_socketaddr(addr);
            let addrlen = size_of_val(&addr) as u32;
            let addr = core::ptr::addr_of!(addr) as *const SocketAddrOpaque;
            UnitFuture::new(
                opcode::Connect::new(Fd(fd), addr, addrlen).build(),
                fd,
                None,
                IOType::Connect,
            )
            .await
        }
        SocketAddr::V6(addr) => {
            let addr = sockaddr6_from_socketaddrv6(addr);
            let addrlen = size_of_val(&addr) as u32;
            let addr = core::ptr::addr_of!(addr) as *const SocketAddrOpaque;
            UnitFuture::new(
                opcode::Connect::new(Fd(fd), addr, addrlen).build(),
                fd,
                None,
                IOType::Connect,
            )
            .await
        }
    }
}

/// Writes data from multiple buffers to a file descriptor.
///
/// Performs a scatter-gather write, writing the contents of multiple buffers
/// to the file descriptor in a single operation. This is equivalent to the
/// `writev(2)` or `pwritev(2)` system call.
///
/// Returns a future that resolves to the number of bytes written, or an error.
pub fn writev<'a>(
    fd: &impl AsFd,
    iovec: &'a [IoSlice<'_>],
    offset: Option<u64>,
) -> ErrnoOrFuture<UsizeFuture<'a>> {
    writev_with_deadline(fd, iovec, offset, None)
}

/// Writes data from multiple buffers with a deadline.
///
/// Like [`writev`], but with an optional deadline. If the operation does not complete
/// before the deadline, it will fail with `Errno::TIMEDOUT`.
///
/// **Note:** When a virtual clock is active, the deadline comparison uses virtual
/// time, but the actual I/O timeout submitted to io_uring uses real kernel time.
/// Advancing virtual time will **not** cause an in-flight I/O operation to time out
/// early. This is by design — real I/O latency is not virtualized.
///
/// Returns a future that resolves to the number of bytes written, or an error.
pub fn writev_with_deadline<'a>(
    fd: &impl AsFd,
    iovec: &'a [IoSlice<'_>],
    offset: Option<u64>,
    deadline: Option<Instant>,
) -> ErrnoOrFuture<UsizeFuture<'a>> {
    let timeout = if let Some(deadline) = deadline {
        if let Some(duration) = deadline.checked_duration_since(crate::clock_now()) {
            Some(duration)
        } else {
            return ErrnoOrFuture::Error {
                errno: Errno::TIMEDOUT,
            };
        }
    } else {
        None
    };

    ErrnoOrFuture::Future {
        fut: writev_with_timeout(fd, iovec, offset, timeout),
    }
}

/// Writes data from multiple buffers with a timeout.
///
/// Like [`writev`], but with an optional timeout duration. If the operation does not
/// complete within the timeout, it will fail with `Errno::TIME`.
///
/// Returns a future that resolves to the number of bytes written, or an error.
pub fn writev_with_timeout<'a>(
    fd: &impl AsFd,
    buffers: &'a [IoSlice<'_>],
    offset: Option<u64>,
    timeout: Option<Duration>,
) -> UsizeFuture<'a> {
    let fd = fd.as_fd().as_raw_fd();
    // IoSlice is guaranteed ABI compatible with iovec
    let iovec = buffers.as_ptr() as *const iovec;
    UsizeFuture::new(
        opcode::Writev::new(Fd(fd), iovec, buffers.len() as u32)
            .offset(offset.unwrap_or(u64::MAX))
            .build(),
        fd,
        timeout,
        IOType::Write,
    )
}

/// Writes data from a buffer to a file descriptor.
///
/// Writes the contents of `buf` to the file descriptor at the current position.
/// The file position is advanced by the number of bytes written.
///
/// Returns a future that resolves to the number of bytes written, or an error.
pub fn write<'a>(fd: &impl AsFd, buf: &'a [u8]) -> ErrnoOrFuture<UsizeFuture<'a>> {
    write_with_deadline(fd, buf, None)
}

/// Writes data from a buffer with a deadline.
///
/// Like [`write`], but with an optional deadline. If the operation does not complete
/// before the deadline, it will fail with `Errno::TIMEDOUT`.
///
/// **Note:** When a virtual clock is active, the deadline comparison uses virtual
/// time, but the actual I/O timeout submitted to io_uring uses real kernel time.
/// Advancing virtual time will **not** cause an in-flight I/O operation to time out
/// early. This is by design — real I/O latency is not virtualized.
///
/// Returns a future that resolves to the number of bytes written, or an error.
pub fn write_with_deadline<'a>(
    fd: &impl AsFd,
    buf: &'a [u8],
    deadline: Option<Instant>,
) -> ErrnoOrFuture<UsizeFuture<'a>> {
    let timeout = if let Some(deadline) = deadline {
        if let Some(duration) = deadline.checked_duration_since(crate::clock_now()) {
            Some(duration)
        } else {
            return ErrnoOrFuture::Error {
                errno: Errno::TIMEDOUT,
            };
        }
    } else {
        None
    };

    ErrnoOrFuture::Future {
        fut: write_with_timeout(fd, buf, timeout),
    }
}

/// Writes data from a buffer with a timeout.
///
/// Like [`write`], but with an optional timeout duration. If the operation does not
/// complete within the timeout, it will fail with `Errno::TIME`.
///
/// Returns a future that resolves to the number of bytes written, or an error.
pub fn write_with_timeout<'a>(
    fd: &impl AsFd,
    buf: &'a [u8],
    timeout: Option<Duration>,
) -> UsizeFuture<'a> {
    let fd = fd.as_fd().as_raw_fd();
    UsizeFuture::new(
        opcode::Write::new(Fd(fd), buf.as_ptr(), buf.len() as u32)
            .offset(u64::MAX)
            .build(),
        fd,
        timeout,
        IOType::Write,
    )
}

/// Sends data on a socket.
///
/// Transmits data from `buf` to the connected peer on the socket.
/// This is equivalent to the `send(2)` system call.
///
/// Returns a future that resolves to the number of bytes sent, or an error.
pub fn send<'a>(
    fd: &impl AsFd,
    buf: &'a [u8],
    flags: SendFlags,
    timeout: Option<Duration>,
) -> UsizeFuture<'a> {
    let fd = fd.as_fd().as_raw_fd();
    UsizeFuture::new(
        opcode::Send::new(Fd(fd), buf.as_ptr(), buf.len() as u32)
            .flags(flags)
            .build(),
        fd,
        timeout,
        IOType::Send,
    )
}

/// Receives data from a socket.
///
/// Receives data from the connected peer into `buf`.
/// This is equivalent to the `recv(2)` system call.
///
/// Returns a future that resolves to the number of bytes received, or an error.
pub fn recv<'a>(
    fd: &impl AsFd,
    buf: &'a mut [u8],
    flags: RecvFlags,
    timeout: Option<Duration>,
) -> UsizeFuture<'a> {
    let fd = fd.as_fd().as_raw_fd();
    UsizeFuture::new(
        opcode::Recv::new(Fd(fd), buf.as_mut_ptr(), buf.len() as u32)
            .flags(flags)
            .build(),
        fd,
        timeout,
        IOType::Recv,
    )
}

/// Receives a message from a socket.
///
/// Receives a message from the socket, optionally including ancillary data.
/// This is equivalent to the `recvmsg(2)` system call.
///
/// Returns a future that resolves to the number of bytes received, or an error.
pub fn recvmsg<'a>(
    fd: &impl AsFd,
    msghdr: &'a mut MsgHdr,
    flags: RecvFlags,
    timeout: Option<Duration>,
) -> UsizeFuture<'a> {
    let fd = fd.as_fd().as_raw_fd();
    UsizeFuture::new(
        // layouts between libc and iouring msghdr are identical
        opcode::RecvMsg::new(Fd(fd), msghdr as *mut MsgHdr)
            .flags(flags)
            .build(),
        fd,
        timeout,
        IOType::Recv,
    )
}

/// Sends a message on a socket.
///
/// Sends a message on the socket, optionally including ancillary data.
/// This is equivalent to the `sendmsg(2)` system call.
///
/// Returns a future that resolves to the number of bytes sent, or an error.
pub fn sendmsg<'a>(
    fd: &impl AsFd,
    msghdr: &'a mut MsgHdr,
    flags: SendFlags,
    timeout: Option<Duration>,
) -> UsizeFuture<'a> {
    let fd = fd.as_fd().as_raw_fd();
    UsizeFuture::new(
        opcode::SendMsg::new(Fd(fd), msghdr as *mut MsgHdr)
            .flags(flags)
            .build(),
        fd,
        timeout,
        IOType::Send,
    )
}

/// Writes data from a buffer to a file descriptor at a specific offset.
///
/// This performs a positioned write (pwrite), writing the contents of `buf` to
/// the file descriptor `fd` starting at the given byte `offset`. Unlike regular
/// writes, pwrite does not modify the file's current position.
///
/// if `polled` is true, uses io_uring's polled mode for potentially lower
/// latency on devices that support polling (e.g., NVMe drives with polling
/// enabled). Polled mode spins on completion rather than using interrupts.
///
/// Returns the number of bytes written, or an error.
pub fn pwrite_polled<'a>(
    fd: impl AsFd,
    buf: &'a [u8],
    offset: u64,
    polled: bool,
) -> UsizeFuture<'a> {
    let fd = fd.as_fd().as_raw_fd();
    UsizeFuture::with_polled(
        opcode::Write::new(Fd(fd), buf.as_ptr(), buf.len() as u32)
            .offset(offset)
            .build(),
        fd,
        None,
        IOType::Write,
        polled,
        CompletionResources::None,
    )
}

/// Writes data from a buffer to a file descriptor at a specific offset.
///
/// This performs a positioned write (pwrite), writing the contents of `buf` to
/// the file descriptor `fd` starting at the given byte `offset`. Unlike regular
/// writes, pwrite does not modify the file's current position.
///
/// Does not use polled I/O mode.
///
/// Returns the number of bytes written, or an error.
pub fn pwrite<'a>(fd: &impl AsFd, buf: &'a [u8], offset: u64) -> UsizeFuture<'a> {
    pwrite_polled(fd, buf, offset, false)
}

/// Reads data from a file descriptor into multiple buffers.
///
/// Performs a scatter-gather read, reading data from the file descriptor into
/// multiple buffers in a single operation. This is equivalent to the
/// `readv(2)` or `preadv(2)` system call.
///
/// Returns a future that resolves to the number of bytes read, or an error.
pub fn readv<'a>(fd: &impl AsFd, iovec: &'a [iovec], offset: Option<u64>) -> UsizeFuture<'a> {
    let fd = fd.as_fd().as_raw_fd();
    UsizeFuture::new(
        opcode::Readv::new(Fd(fd), iovec.as_ptr(), iovec.len() as u32)
            .offset(offset.unwrap_or(u64::MAX))
            .build(),
        fd,
        None,
        IOType::Read,
    )
}

/// Reads data from a file descriptor into a buffer.
///
/// Reads data from the file descriptor at the current position into `buf`.
/// The file position is advanced by the number of bytes read.
///
/// Returns a future that resolves to the number of bytes read, or an error.
pub fn read<'a>(fd: &impl AsFd, buf: &'a mut [u8]) -> UsizeFuture<'a> {
    read_with_timeout(fd, buf, None)
}

/// Reads data from a file descriptor with a deadline.
///
/// Like [`read`], but with an optional deadline. If the operation does not complete
/// before the deadline, it will fail with `Errno::TIMEDOUT`.
///
/// **Note:** When a virtual clock is active, the deadline comparison uses virtual
/// time, but the actual I/O timeout submitted to io_uring uses real kernel time.
/// Advancing virtual time will **not** cause an in-flight I/O operation to time out
/// early. This is by design — real I/O latency is not virtualized.
///
/// Returns a future that resolves to the number of bytes read, or an error.
pub fn read_with_deadline<'a>(
    fd: &impl AsFd,
    buf: &'a mut [u8],
    deadline: Option<Instant>,
) -> ErrnoOrFuture<UsizeFuture<'a>> {
    let timeout = if let Some(deadline) = deadline {
        if let Some(duration) = deadline.checked_duration_since(crate::clock_now()) {
            Some(duration)
        } else {
            return ErrnoOrFuture::Error {
                errno: Errno::TIMEDOUT,
            };
        }
    } else {
        None
    };

    ErrnoOrFuture::Future {
        fut: read_with_timeout(fd, buf, timeout),
    }
}

/// Reads data from a file descriptor with a timeout.
///
/// Like [`read`], but with an optional timeout duration. If the operation does not
/// complete within the timeout, it will fail with `Errno::TIME`.
///
/// Returns a future that resolves to the number of bytes read, or an error.
pub fn read_with_timeout<'a>(
    fd: &impl AsFd,
    buf: &'a mut [u8],
    timeout: Option<Duration>,
) -> UsizeFuture<'a> {
    let fd = fd.as_fd().as_raw_fd();
    UsizeFuture::new(
        opcode::Read::new(Fd(fd), buf.as_mut_ptr(), buf.len() as u32)
            .offset(u64::MAX)
            .build(),
        fd,
        timeout,
        IOType::Read,
    )
}

/// Reads data from a file descriptor at a specific offset into a buffer.
///
/// This performs a positioned read (pread), reading from the file descriptor `fd`
/// starting at the given byte `offset` into `buf`. Unlike regular reads, pread does
/// not modify the file's current position.
///
/// If `polled` is true, uses io_uring's polled mode for potentially lower latency
/// on devices that support polling (e.g., NVMe drives with polling enabled).
/// Polled mode spins on completion rather than using interrupts.
///
/// Returns the number of bytes read, or an error.
pub fn pread_polled<'a>(
    fd: impl AsFd,
    buf: &'a mut [u8],
    offset: u64,
    polled: bool,
) -> UsizeFuture<'a> {
    let fd = fd.as_fd().as_raw_fd();
    UsizeFuture::with_polled(
        opcode::Read::new(Fd(fd), buf.as_mut_ptr(), buf.len() as u32)
            .offset(offset)
            .build(),
        fd,
        None,
        IOType::Read,
        polled,
        CompletionResources::None,
    )
}

/// Reads data from a file descriptor at a specific offset into a buffer.
///
/// This performs a positioned read (pread), reading from the file descriptor `fd`
/// starting at the given byte `offset` into `buf`. Unlike regular reads, pread does
/// not modify the file's current position.
///
/// Returns a the number of bytes read, or an error.
pub fn pread<'a>(fd: &impl AsFd, buf: &'a mut [u8], offset: u64) -> UsizeFuture<'a> {
    pread_polled(fd, buf, offset, false)
}

/// Closes a file descriptor.
///
/// Closes the file descriptor, releasing any associated resources. The file descriptor
/// is consumed and cannot be used after this operation.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn close(fd: OwnedFd) -> UnitFuture<'static> {
    // we are consuming the fd ourselves, so suppress the Drop trait
    let fd = ManuallyDrop::new(fd);
    let fd = fd.as_fd().as_raw_fd();
    UnitFuture::new(opcode::Close::new(Fd(fd)).build(), fd, None, IOType::Close)
}

/// Performs a no-operation.
///
/// Submits a no-op entry to io_uring. This can be useful for testing or for
/// ensuring ordering between other operations.
///
/// Returns a future that resolves to `()` on success, or an error.
pub fn nop() -> UnitFuture<'static> {
    UnitFuture::new(opcode::Nop::new().build(), -1, None, IOType::Nop)
}

/// Support direct command passthrough to underlying devices behind io_uring.
/// This is useful for fast NVMe command processing bypassing the Linux Block
/// layer and leveraging specialized NVMe extensions. Some details on passthru
/// can be found [here](https://lpc.events/event/11/contributions/989/attachments/747/1723/lpc-2021-building-a-fast-passthru.pdf)
///
/// A few caveats:
/// 1) The file descriptor should be a FD for the /dev/ngDn1 NVMe generic dev
/// 2) The op value is a special value for NVMe passthru
#[cfg(feature = "io_uring_cmd")]
pub fn uring_cmd(fd: &impl AsFd, op: u32, cmd: [u8; 80]) -> UringCmdFuture<'_> {
    let fd = fd.as_fd().as_raw_fd();
    UringCmdFuture::new(
        opcode::UringCmd80::new(Fd(fd), op).cmd(cmd).build(),
        fd,
        None,
        IOType::NvmeCmd,
    )
}

/// Support direct command passthrough to underlying devices behind io_uring, with polled mode.
///
/// Like [`uring_cmd`], but uses io_uring's polled mode for potentially lower latency
/// on devices that support polling. Polled mode spins on completion rather than using interrupts.
///
/// See [`uring_cmd`] for more details on NVMe passthru commands.
#[cfg(feature = "io_uring_cmd")]
pub fn uring_cmd_polled(fd: &impl AsFd, op: u32, cmd: [u8; 80]) -> UringCmdFuture<'_> {
    let fd = fd.as_fd().as_raw_fd();
    UringCmdFuture::with_polled(
        opcode::UringCmd80::new(Fd(fd), op).cmd(cmd).build(),
        fd,
        None,
        IOType::NvmeCmd,
        true,
        CompletionResources::None,
    )
}

/// yield_io will cause execution to stop and return to the main loop.
/// By default, tasks run at IO priority. Each iteration of the main
/// loop, the currently ready IO priority tasks will be run, and then
/// their resulting I/O submissions will be sent to IO URing in a
/// batch and completions will be processed. Only if there are no
/// IO tasks pending then CPU tasks will run.
pub fn yield_io() -> SetYieldCpuFuture {
    SetYieldCpuFuture {
        state: YieldFutureState::CreatedIo,
    }
}

/// yield_cpu will cause execution of the task to pause. Execution
/// of this task will continue only when IO priority tasks have
/// completed.  This would be good to call just before doing a
/// piece of work that will use the CPU for a significant amount
/// of time (e.g. more than a couple microseconds).  The CPU
/// priority is not durable. As soon as the task running at CPU
/// priority pauses for I/O it will resume executing at IO priority.
pub fn yield_cpu() -> SetYieldCpuFuture {
    SetYieldCpuFuture {
        state: YieldFutureState::CreatedCpu,
    }
}

#[derive(PartialEq)]
enum YieldFutureState {
    CreatedIo,
    CreatedCpu,
    Polled,
    Complete,
}

pub struct SetYieldCpuFuture {
    state: YieldFutureState,
}

impl Future for SetYieldCpuFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut task_state = TaskState::get();
        let current_task = task_state.get_current_task();
        let current_task_state = current_task.get_state();
        if current_task_state == TaskReadyState::Aborted {
            // If we were aborted between the time we yielded and now, then
            // detect that and panic.
            panic!("Task aborted");
        }

        match self.state {
            YieldFutureState::CreatedIo => {
                task_state.schedule_io(current_task);
                self.get_mut().state = YieldFutureState::Polled;
                Poll::Pending
            }
            YieldFutureState::CreatedCpu => {
                task_state.schedule_cpu(current_task);
                self.get_mut().state = YieldFutureState::Polled;
                Poll::Pending
            }
            YieldFutureState::Polled => {
                self.get_mut().state = YieldFutureState::Complete;
                Poll::Ready(())
            }
            YieldFutureState::Complete => panic!("Do not poll completed futures"),
        }
    }
}

impl FusedFuture for SetYieldCpuFuture {
    fn is_terminated(&self) -> bool {
        self.state == YieldFutureState::Complete
    }
}

/// Maps an io_uring timeout completion result to the `SleepFuture` contract:
/// `Errno::TIME` (normal expiry) becomes `Ok(())`.
fn map_timeout_poll(poll: Poll<Result<(), Errno>>) -> Poll<Result<(), Errno>> {
    match poll {
        Poll::Ready(Err(Errno::TIME)) => Poll::Ready(Ok(())),
        Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        Poll::Ready(Ok(_)) => panic!("Timeout should never return Ok"),
        Poll::Pending => Poll::Pending,
    }
}

/// When the `virtual-clock` feature is enabled, attempts to create a virtual
/// sleep future for the given deadline. Returns `None` if no virtual clock is
/// active.
#[cfg(feature = "virtual-clock")]
fn try_virtual_sleep(deadline: Instant) -> Option<SleepFuture<'static>> {
    let task_state = TaskState::get();
    if task_state.clock.is_some() {
        Some(SleepFuture {
            inner: SleepFutureInner::Virtual(crate::virtual_clock::VirtualSleepFuture::new(
                deadline,
            )),
        })
    } else {
        None
    }
}

/// Polls a future exactly once and returns its output if immediately ready.
///
/// This is a test utility for the poll-advance-await pattern common in
/// virtual clock tests. It registers any internal timers (by polling) and
/// returns `None` if the future is pending, or `Some(output)` if it
/// completed immediately.
///
/// **Note**: This is a one-shot probe. If it returns `Some`, the future
/// has been consumed — do not `.await` or `poll_once` it again (doing so
/// on a `VirtualSleepFuture` will panic).
///
/// # Example
///
/// ```rust,no_run
/// use std::pin::pin;
/// use std::time::Duration;
/// use kimojio::operations;
///
/// # async fn example() {
/// let mut sleep = pin!(operations::sleep(Duration::from_secs(10)));
/// assert!(operations::poll_once(sleep.as_mut()).await.is_none(), "should be pending");
/// operations::virtual_clock_advance(Duration::from_secs(10));
/// let result = operations::poll_once(sleep.as_mut()).await;
/// assert!(result.is_some(), "should be ready");
/// # }
/// ```
#[cfg(feature = "virtual-clock")]
pub async fn poll_once<F: Future>(mut fut: Pin<&mut F>) -> Option<F::Output> {
    futures::future::poll_fn(|cx| match fut.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(None),
        Poll::Ready(val) => Poll::Ready(Some(val)),
    })
    .await
}

/// Suspends the task for the specified duration (or longer).
///
/// When a virtual clock is enabled via
/// [`virtual_clock_enable(true)`](virtual_clock_enable), the sleep completes
/// when virtual time is advanced past the deadline — no real wall-clock time
/// passes.
///
/// # Cancel safety
///
/// This method is cancel safe.
pub fn sleep(duration: Duration) -> SleepFuture<'static> {
    #[cfg(feature = "virtual-clock")]
    {
        let now = crate::clock_now();
        let deadline = now.checked_add(duration).unwrap_or_else(|| {
            // Saturate to the maximum representable Instant rather than panicking.
            now + Duration::from_secs(365 * 24 * 3600 * 100) // ~100 years
        });
        if let Some(vfut) = try_virtual_sleep(deadline) {
            return vfut;
        }
    }

    let timespec = Box::new(Timespec::from(duration));
    let entry = opcode::Timeout::new(timespec.as_ref()).build();
    let fut = UnitFuture::with_polled(
        entry,
        -1,
        None,
        IOType::Timeout,
        false,
        CompletionResources::Box(timespec),
    );

    #[cfg(feature = "virtual-clock")]
    {
        SleepFuture {
            inner: SleepFutureInner::Real(fut),
        }
    }

    #[cfg(not(feature = "virtual-clock"))]
    {
        SleepFuture { fut }
    }
}

/// Suspends the task until the specified deadline.
///
/// When a virtual clock is enabled via
/// [`virtual_clock_enable(true)`](virtual_clock_enable), completes when
/// virtual time is advanced to or past `deadline`. With real system time,
/// computes the remaining duration and delegates to an io_uring timeout.
///
/// If `deadline` is already in the past, the future completes immediately
/// on first poll.
///
/// # Cancel safety
///
/// This method is cancel safe.
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::{Duration, Instant};
/// use kimojio::operations;
///
/// async fn example() {
///     let deadline = Instant::now() + Duration::from_secs(5);
///     operations::sleep_until(deadline).await.unwrap();
/// }
/// ```
pub fn sleep_until(deadline: Instant) -> SleepFuture<'static> {
    #[cfg(feature = "virtual-clock")]
    {
        if let Some(vfut) = try_virtual_sleep(deadline) {
            return vfut;
        }
    }

    let now = crate::clock_now();
    let duration = deadline.saturating_duration_since(now);
    sleep(duration)
}

/// Runs `future` with a deadline, returning a timeout error if the deadline
/// is reached before the future completes.
///
/// When a virtual clock is enabled, the timeout fires when virtual time is
/// advanced past `deadline` — no real wall-clock time passes.
///
/// # Polling Order
///
/// The inner future is polled before the timeout timer. If both the inner
/// future and the timeout are immediately ready (e.g., the deadline has
/// already passed but the inner future completes synchronously), the inner
/// future's result takes priority and `Ok(value)` is returned.
///
/// # Cancel safety
///
/// The inner future is dropped if the timeout fires.
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::{Duration, Instant};
/// use kimojio::{TimeoutError, operations};
///
/// async fn example() {
///     let deadline = Instant::now() + Duration::from_secs(5);
///     match operations::timeout_at(deadline, async { 42 }).await {
///         Ok(value) => assert_eq!(value, 42),
///         Err(TimeoutError::Timeout) => panic!("timed out"),
///         Err(TimeoutError::Canceled) => panic!("canceled"),
///     }
/// }
/// ```
pub async fn timeout_at<F: Future>(
    deadline: Instant,
    future: F,
) -> Result<F::Output, crate::TimeoutError> {
    use futures::future::{Either, select};
    use std::pin::pin;

    let future = pin!(future);
    let timer = pin!(sleep_until(deadline));

    match select(future, timer).await {
        Either::Left((result, _)) => Ok(result),
        Either::Right((Ok(()), _)) => Err(crate::TimeoutError::Timeout),
        Either::Right((Err(_), _)) => Err(crate::TimeoutError::Canceled),
    }
}

// --- SleepFuture: dual type definitions for feature gating ---
//
// pin_project_lite does NOT support #[cfg] on enum variants, so we use
// two separate type definitions: the original pin_project_lite struct when
// virtual-clock is off, and an enum when it's on.

#[cfg(not(feature = "virtual-clock"))]
pin_project_lite::pin_project! {
    /// A future that resolves after a timeout duration.
    ///
    /// Created by [`sleep()`] or [`sleep_until()`].
    pub struct SleepFuture<'a> {
        #[pin]
        fut: UnitFuture<'a>,
    }
}

#[cfg(not(feature = "virtual-clock"))]
impl<'a> SleepFuture<'a> {
    /// Cancels this sleep, causing it to resolve with an error.
    pub fn cancel(self: Pin<&mut Self>) {
        self.project().fut.cancel();
    }
}

#[cfg(not(feature = "virtual-clock"))]
impl<'a> Future for SleepFuture<'a> {
    type Output = Result<(), Errno>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        map_timeout_poll(self.project().fut.poll(cx))
    }
}

#[cfg(not(feature = "virtual-clock"))]
impl<'a> FusedFuture for SleepFuture<'a> {
    fn is_terminated(&self) -> bool {
        self.fut.is_terminated()
    }
}

/// A future that resolves after a timeout duration.
///
/// Created by [`sleep()`] or [`sleep_until()`]. When the `virtual-clock`
/// feature is enabled and a virtual clock is active, resolves based on
/// virtual time advancement instead of real wall-clock time.
#[cfg(feature = "virtual-clock")]
pub struct SleepFuture<'a> {
    inner: SleepFutureInner<'a>,
}

#[cfg(feature = "virtual-clock")]
enum SleepFutureInner<'a> {
    Real(UnitFuture<'a>),
    Virtual(crate::virtual_clock::VirtualSleepFuture),
}

#[cfg(feature = "virtual-clock")]
impl<'a> SleepFuture<'a> {
    /// Cancels this sleep, causing it to resolve with an error.
    pub fn cancel(self: Pin<&mut Self>) {
        // SAFETY: Pin projection through the SleepFutureInner enum.
        //
        // Invariants upheld:
        // 1. The enum variant is set at construction and never changes — there
        //    is no code path that switches between Real and Virtual after init.
        // 2. The Real variant contains a !Unpin UnitFuture and must remain
        //    pinned; we project through Pin::new_unchecked.
        // 3. VirtualSleepFuture is Unpin (all fields are Unpin: Instant,
        //    Option<TimerId>, Option<Waker>, bool), so mutable
        //    access through Pin is safe.
        unsafe {
            match &mut self.get_unchecked_mut().inner {
                SleepFutureInner::Real(fut) => Pin::new_unchecked(fut).cancel(),
                SleepFutureInner::Virtual(fut) => fut.cancel(),
            }
        }
    }
}

// Compile-time assertion that VirtualSleepFuture is Unpin, which is required
// for the unsafe pin projection in SleepFuture to be sound.
#[cfg(feature = "virtual-clock")]
static_assertions::assert_impl_all!(crate::virtual_clock::VirtualSleepFuture: Unpin);

#[cfg(feature = "virtual-clock")]
impl<'a> Future for SleepFuture<'a> {
    type Output = Result<(), Errno>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: Same pin projection invariants as cancel() — see above.
        unsafe {
            match &mut self.get_unchecked_mut().inner {
                SleepFutureInner::Real(fut) => map_timeout_poll(Pin::new_unchecked(fut).poll(cx)),
                SleepFutureInner::Virtual(fut) => Pin::new(fut).poll(cx),
            }
        }
    }
}

#[cfg(feature = "virtual-clock")]
impl<'a> FusedFuture for SleepFuture<'a> {
    fn is_terminated(&self) -> bool {
        match &self.inner {
            SleepFutureInner::Real(fut) => fut.is_terminated(),
            SleepFutureInner::Virtual(fut) => fut.is_terminated(),
        }
    }
}

/// with_timeout_warning wrap `future` without changing its behavior or return value at all.
/// However, if `duration` time passes before the future completes, then warning will be
/// called and `future` will continue to be polled until it is completed
pub async fn with_timeout_warning<F>(
    future: F,
    duration: Duration,
    warning: impl FnOnce(),
) -> F::Output
where
    F: FusedFuture,
{
    use std::pin::pin;

    // We own future - pin it locally
    let mut future = pin!(future);

    // Create a timeout future and pin it locally
    let mut timer = pin!(sleep(duration).fuse());

    // wait for the future to complete, or a timeout to expire.
    futures::select! {
        a = future => return a,
        _ = timer => warning(),
    }

    future.await
}

/// Writes a tracing event for the current task.
///
/// Records a tracing event associated with the currently executing task.
/// This is useful for debugging and performance analysis.
pub fn write_event(event: Events) {
    let task_state = TaskState::get();
    let task_id = task_state.current_task.as_ref().unwrap().task_index;
    task_state.write_event(task_id, event)
}

/// Logs a message with the current task's identity.
///
/// Prints a formatted message prefixed with the current thread ID and task index.
/// Useful for debugging multi-task scenarios.
pub fn log(args: std::fmt::Arguments<'_>) {
    let TaskIdentity {
        thread_id,
        task_index,
        ..
    } = task_identity();
    print!("{:?}:{} {}", thread_id, task_index, std::fmt::format(args))
}

/// Information identifying the current task.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TaskIdentity {
    /// The thread ID of the thread executing the task.
    pub thread_id: std::thread::ThreadId,
    /// The index of the thread within the runtime.
    pub thread_index: u8,
    /// The index of the task within the runtime.
    pub task_index: u16,
}

/// Returns the identity of the currently executing task.
///
/// Provides information about the current task including thread ID,
/// thread index, and task index. Returns a `TaskIdentity` struct.
pub fn task_identity() -> TaskIdentity {
    let task_state = TaskState::get();
    let thread_id = task_state.get_current_thread_id();
    if let Some(current_task) = task_state.current_task.as_ref() {
        let task_index = current_task.task_index;
        let thread_index = task_state.trace_buffer.thread_idx;
        TaskIdentity {
            thread_id,
            thread_index,
            task_index,
        }
    } else {
        // There is no task - could happen if task is dropped after loop exits
        // because of shutdown_loop().
        TaskIdentity {
            thread_id,
            thread_index: 0,
            task_index: 0,
        }
    }
}

/// Returns the activity ID of the current task.
///
/// Activity IDs are used for tracing and correlating operations
/// across tasks and services. Returns the UUID representing the current task's activity ID.
#[inline(always)]
pub fn get_activity_id() -> uuid::Uuid {
    let task_state = TaskState::get();
    task_state.get_current_activity_id()
}

/// Returns the tenant ID of the current task.
///
/// Tenant IDs are used for identifying the tenant or context
/// associated with the current task. Returns the UUID representing the current task's tenant ID.
#[inline(always)]
pub fn get_tenant_id() -> uuid::Uuid {
    let task_state = TaskState::get();
    task_state.get_current_tenant_id()
}

/// Sets the activity ID and tenant ID for the current task.
///
/// Updates the current task's activity and tenant IDs, which are used
/// for tracing and correlation purposes.
pub fn set_activity_id_and_tenant_id(activity_id: uuid::Uuid, tenant_id: uuid::Uuid) {
    let mut task_state = TaskState::get();
    task_state.set_current_activity_id_and_tenant_id(activity_id, tenant_id);
}

/// Sets the high priority flag for the current task.
///
/// High priority tasks may be scheduled preferentially by the runtime.
pub fn set_high_priority(high_priority: bool) {
    let task_state = TaskState::get();
    task_state
        .current_task
        .as_ref()
        .unwrap()
        .high_priority
        .set(high_priority);
}

/// `spawn_task` creates a new task that will poll the provided future to
/// completion.
pub fn spawn_task<Fut>(future: Fut) -> TaskHandle<Fut::Output>
where
    Fut: Future + 'static,
{
    // propagate current activity id to next task by default
    let mut task_state = TaskState::get();
    let activity_id = task_state.get_current_activity_id();
    let tenant_id = task_state.get_current_tenant_id();

    let task = task_state.schedule_new(future, activity_id, tenant_id);
    TaskHandle::new(task)
}

pin_project_lite::pin_project! {
    pub struct TaskHandle<T: 'static> {
        #[pin]
        wait: crate::async_event::WaitFuture<TaskSource>,
        _marker: PhantomData<T>,
    }
}

impl<T: 'static> TaskHandle<T> {
    pub fn new(task: Rc<Task>) -> Self {
        TaskHandle {
            wait: crate::async_event::WaitFuture::new(TaskSource::new(task)),
            _marker: Default::default(),
        }
    }

    /// Wait for a task to complete, ignoring the result.
    pub fn wait(&self) -> WaitFuture {
        WaitFuture::new(self.wait.source().unwrap().task())
    }

    /// `abort` will schedule a task to run and then cause it to panic.  Panic
    /// will occur when the task wakes up.  This will currently be detected in
    /// the poll method of I/O, wait, and yield futures.
    pub fn abort(&self) {
        let mut task_state = TaskState::get();
        task_state.abort_task(self.wait.source().unwrap().task())
    }
}

// Ensure that TaskHandle is always !Clone
static_assertions::const_assert!(impls::impls!(TaskHandle<()>: !Clone));

impl<T> FusedFuture for TaskHandle<T> {
    fn is_terminated(&self) -> bool {
        self.wait.is_terminated()
    }
}

impl<T> Future for TaskHandle<T> {
    type Output = Result<T, TaskHandleError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().wait.poll(cx) {
            Poll::Ready(source) => match source {
                Ok(source) => {
                    let result = source.task.result::<T>().expect("Task result");
                    let result = result.map_err(TaskHandleError::Panic);
                    Poll::Ready(result)
                }
                Err(_canceled_error) => Poll::Ready(Err(TaskHandleError::Canceled)),
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

pin_project_lite::pin_project! {
    /// WaitFuture wraps the inner WaitFuture but returns ()
    /// instead of TaskSource.  It also propagates the FusedFuture
    /// trait impl from inner.
    pub struct WaitFuture {
        #[pin]
        wait: crate::async_event::WaitFuture<TaskSource>,
    }
}

impl WaitFuture {
    pub fn new(task: Rc<Task>) -> Self {
        let wait = crate::async_event::WaitFuture::new(TaskSource::new(task));
        WaitFuture { wait }
    }
}

impl FusedFuture for WaitFuture {
    fn is_terminated(&self) -> bool {
        self.wait.is_terminated()
    }
}

impl Future for WaitFuture {
    type Output = Result<(), CanceledError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().wait.poll(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Inform the event loop it should make an immediate shutdown and stop
/// processing any remaining work in the queues. The `Runtime::block_on()` call
/// driving the event loop will return None instead of Some(Ok(result)) as soon
/// as the next task is polled.
pub fn shutdown_loop() {
    let mut task_state = TaskState::get();
    task_state.shutdown();
}

/// returns the prefix of version consisting of only digits and periods
fn version_prefix(version: &str) -> &str {
    for (index, char) in version.char_indices() {
        if !char.is_ascii_digit() && char != '.' {
            return &version[..index];
        }
    }
    version
}

/// Parses a string in the form of "5.15.0-rc1" into a tuple of (5, 15).
/// If the string does not contain a valid version, then return an error.
/// Any trailing characters after the version are ignored.
fn parse_version(version: &str) -> Result<(u32, u32), std::num::ParseIntError> {
    let version = version_prefix(version);
    let mut version = version.split('.');
    let version = [version.next(), version.next()];
    let [major, minor] = version.map(|v| v.unwrap_or("").parse::<u32>());
    Ok((major?, minor?))
}

/// Returns the kernel version as a tuple of (major, minor).
/// If the kernel version cannot be determined, then return (5, 15).
pub fn kernel_version() -> (u32, u32) {
    let uname = rustix::system::uname();
    let version_str = uname.release().to_string_lossy();
    let version = parse_version(&version_str);
    // if uname does not return a valid version, then report 5.15
    // as that is the minimum we support.
    version.unwrap_or((5, 15))
}

/// Drains the futures in the stream
///
/// If any result is an error, pending I/O are canceled. The
/// rest of the stream will be drained and the first error
/// will be returned. In this way you can ensure that none of
/// the futures are dropped when one returns an error.
pub fn io_scope_drain_futures_void<E>(
    result: Result<(), E>,
    stream: impl Stream<Item = Result<(), E>>,
) -> impl Future<Output = Result<(), E>> {
    io_scope_drain_futures(result, stream, |_, _| ())
}

/// Drains the futures in the stream, accumulating the results
///
/// If any result is an error, pending I/O are canceled. The
/// rest of the stream will be drained and the first error
/// will be returned. In this way you can ensure that none of
/// the futures are dropped when one returns an error.
pub async fn io_scope_drain_futures<Acc, T, E>(
    mut result: Result<Acc, E>,
    stream: impl Stream<Item = Result<T, E>>,
    mut acc: impl FnMut(Acc, T) -> Acc,
) -> Result<Acc, E> {
    if result.is_err() {
        io_scope_cancel();
    }

    let mut stream = std::pin::pin!(stream);
    while let Some(next_result) = stream.next().await {
        match next_result {
            Ok(value) => {
                if let Ok(current_value) = result {
                    result = Ok(acc(current_value, value));
                }
            }
            Err(e) => {
                if result.is_ok() {
                    io_scope_cancel();
                    result = Err(e);
                }
            }
        }
    }

    result
}

/// If called within an io_scope callback, will cancel all the pending
/// I/O accumulated since the start of the scope. The scope remains
/// open and will continue gathering I/O until it exits.
///
/// This does not wait for the I/O to complete, so the caller should
/// continue to poll the canceled tasks until they are done.
pub fn io_scope_cancel() {
    let mut task_state = TaskState::get();
    let task = task_state.get_current_task();

    // 1. ensure all I/O is submitted
    task_state = crate::runtime::submit_and_complete_io_all(task_state, true);

    task.cancel_io_scope_completions(task_state);
}

/// If called with an io_scope callback, will cancel all the pending
/// I/O accumulated since the start of the scope. The scope remains
/// open and will continue gathering I/O until it exits.
///
/// Calling io_scope_cancel before returning from an io_scope is a
/// way to ensure that futures in the scope have completed before
/// they drop.
///
/// Note that this is a blocking call. Other tasks in this uringruntime
/// thread will not make progress until this call returns. It should
/// most likely be the last call in the io_scope callback. If there are
/// I/O that do not respond quickly to cancellation then that could cause
/// stalls.
fn io_scope_cancel_and_wait_internal(new_io_scope_completions: Option<IoScopeCompletions>) {
    let mut task_state = TaskState::get();
    let task = task_state.get_current_task();

    // restore remembered completions and get the I/O gathered for this task while f was running
    let gathered_completions = task.replace_io_scope_completions(new_io_scope_completions);

    // now we have to wait until all gathered completions are done.
    if let Some(mut gathered_completions) = gathered_completions {
        // 1. ensure all I/O is submitted
        task_state = crate::runtime::submit_and_complete_io_all(task_state, true);

        // 2. cancel the gathered completions
        retain_incomplete(&mut gathered_completions.completions, &mut task_state);

        for completion in &gathered_completions.completions {
            completion.cancel(&mut task_state);
        }

        for wait in gathered_completions.waits.drain(..) {
            wait.canceled.set(true);
            wait.waker.use_mut(|waker| {
                if let Some(waker) = waker {
                    wake_task(&mut task_state, waker)
                }
            });
        }

        // 3. wait for them to finish.
        while !gathered_completions.completions.is_empty() {
            task_state = crate::runtime::submit_and_complete_io_all(task_state, true);

            retain_incomplete(&mut gathered_completions.completions, &mut task_state);
        }
    }
}

/// Calls the function f, ensuring that any IO operations
/// initiated by the current task while calling f are
/// complete by the time f returns. Any IO operations
/// that are in progress when f returns will be cancelled.
pub fn io_scope<'a, T>(f: impl AsyncFnOnce() -> T + 'a) -> impl Future<Output = T> + 'a {
    // remember completions from io_scope further up the stack and clear the current list
    let old_completions = {
        let task_state = TaskState::get();
        let task = task_state.get_current_task();
        task.replace_io_scope_completions(Some(IoScopeCompletions::default()))
    };

    CatchUnwindFuture { f: f() }.map(|result| {
        io_scope_cancel_and_wait_internal(old_completions);

        match result {
            Ok(result) => result,
            Err(e) => std::panic::resume_unwind(e),
        }
    })
}

pin_project_lite::pin_project! {
    /// Catches any panics that occur while polling the future.
    struct CatchUnwindFuture<F: Future> {
        #[pin]
        f: F,
    }
}

impl<F: Future> Future for CatchUnwindFuture<F> {
    type Output = Result<F::Output, Box<dyn std::any::Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match std::panic::catch_unwind(AssertUnwindSafe(|| this.f.poll(cx))) {
            Ok(Poll::Ready(result)) => Poll::Ready(Ok(result)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

fn retain_incomplete(vec: &mut Vec<Rc<Completion>>, task_state: &mut TaskState) {
    let mut index = 0;
    while index < vec.len() {
        let completion = vec.get(index).unwrap();
        let retain = completion.state.use_mut(|state| {
            matches!(
                state,
                CompletionState::Idle { .. } | CompletionState::Submitted { .. }
            )
        });
        if !retain {
            let completion = vec.swap_remove(index);
            task_state.return_completion(completion);
        } else {
            index += 1;
        }
    }
}

// A future that either returns an error,
// or delegates to a future
pin_project_lite::pin_project! {
    #[project = ErrnoOrFutureProj]
    pub enum ErrnoOrFuture<Fut> {
        Error { errno: Errno },
        Future { #[pin] fut: Fut },
    }
}

impl<T, Fut: Future<Output = Result<T, Errno>>> Future for ErrnoOrFuture<Fut> {
    type Output = Result<T, Errno>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            ErrnoOrFutureProj::Error { errno } => Poll::Ready(Err(*errno)),
            ErrnoOrFutureProj::Future { fut } => fut.poll(cx),
        }
    }
}

impl<T, Fut: Future<Output = Result<T, Errno>> + IsIoPoll> IsIoPoll for ErrnoOrFuture<Fut> {
    fn is_io_poll(&self) -> bool {
        match self {
            ErrnoOrFuture::Error { errno: _ } => false,
            ErrnoOrFuture::Future { fut } => fut.is_io_poll(),
        }
    }
}

impl<T, Fut: FusedFuture<Output = Result<T, Errno>>> FusedFuture for ErrnoOrFuture<Fut> {
    fn is_terminated(&self) -> bool {
        match self {
            ErrnoOrFuture::Error { errno: _ } => true,
            ErrnoOrFuture::Future { fut } => fut.is_terminated(),
        }
    }
}

/// A trait that indicates that the future is an I/O poll future.
pub trait IsIoPoll: FusedFuture {
    fn is_io_poll(&self) -> bool;
}

/// `submit` wraps a future and ensures that it is submitted to the kernel
/// immediately when it is polled without waiting for other tasks to be polled.
/// This is useful for ensuring minimum possible latency at the expense of
/// missing out on batching submissions.
///
/// # Usage
///
/// ```rust
/// use kimojio::{operations, Errno};
///
/// async fn nop_later() -> Result<(), Errno> {
///     operations::nop().await
/// }
///
/// async fn nop_right_now() -> Result<(), Errno> {
///     operations::submit(operations::nop()).await
/// }
/// ```
pub fn submit<F: IsIoPoll>(fut: F) -> SubmitFuture<F> {
    SubmitFuture {
        future: fut,
        polled: false,
    }
}

pin_project_lite::pin_project! {
    /// SubmitFuture wraps a future, and if it returns Pending when first polled,
    /// immediately submits any pending SQE to the kernel without waiting for
    /// other tasks to be polled first.
    pub struct SubmitFuture<F: IsIoPoll> {
        #[pin] future: F,
        polled: bool,
    }
}

impl<F: IsIoPoll> Future for SubmitFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let result = this.future.as_mut().poll(cx);
        match result {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => {
                if !*this.polled {
                    *this.polled = true;
                    // if it was not polled, then it is now polled and we can submit the
                    // staged SQE to the kernel
                    let iopoll = this.future.is_io_poll();
                    let task_state = TaskState::get();
                    crate::runtime::submit_and_complete_io(task_state, false, iopoll);
                }
                Poll::Pending
            }
        }
    }
}

/// After the next `operation_count` calls to RingFuture::poll(),
/// the next call `fault` will be returned.  For example, if you pass
/// zero, then the next call to RingFuture::poll() will fail. The
/// underlying I/O will not be affected.
#[cfg(feature = "fault_injection")]
pub fn inject_fault(operation_count: usize, fault: Errno) {
    let mut task_state = TaskState::get();
    task_state.inject_fault(operation_count, fault)
}

// --- Virtual Clock Operations ---

/// Enables or disables the virtual clock for the current runtime.
///
/// When enabled, a new virtual clock is created with the current real time as
/// its epoch. All subsequent timing operations ([`sleep`], [`sleep_until`],
/// [`timeout_at`]) use virtual time — they will not resolve until time is
/// explicitly advanced via [`virtual_clock_advance`] or [`virtual_clock_advance_to`].
///
/// When disabled, the virtual clock is removed and timing operations revert to
/// real system time.
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
/// use kimojio::operations;
///
/// # async fn example() {
/// operations::virtual_clock_enable(true);
/// // All sleeps now use virtual time
/// operations::virtual_clock_advance(Duration::from_secs(60));
/// operations::virtual_clock_enable(false);
/// // Back to real system time
/// # }
/// ```
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_enable(enabled: bool) {
    let mut task_state = TaskState::get();
    if enabled {
        // No-op if already enabled — prevents dropping in-flight
        // VirtualSleepFuture timers registered with the current clock.
        if task_state.clock.is_none() {
            task_state.clock = Some(crate::virtual_clock::VirtualClockState::new(
                std::time::Instant::now(),
            ));
        }
    } else {
        task_state.clock = None;
    }
}

/// Advances the virtual clock by the given duration, firing any expired timers.
///
/// Returns the number of timers that fired. Timers fire in deadline order;
/// ties are broken by registration order (deterministic).
///
/// # Panics
///
/// Panics if the virtual clock is not enabled.
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
/// use kimojio::operations;
///
/// # async fn example() {
/// operations::virtual_clock_enable(true);
/// let fired = operations::virtual_clock_advance(Duration::from_secs(60));
/// # }
/// ```
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_advance(duration: std::time::Duration) -> usize {
    let (fired, wakers) = {
        let mut task_state = TaskState::get();
        let clock = task_state
            .clock
            .as_mut()
            .expect("virtual clock not enabled; call virtual_clock_enable(true) first");
        clock.advance(duration)
    }; // TaskState borrow dropped before waking
    for w in wakers {
        w.wake();
    }
    fired
}

/// Advances the virtual clock to a specific instant, firing any expired timers.
///
/// If `target` is before the current virtual time, no timers fire and the
/// clock is not moved backward. Returns the number of timers that fired.
///
/// # Panics
///
/// Panics if the virtual clock is not enabled.
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_advance_to(target: std::time::Instant) -> usize {
    let (fired, wakers) = {
        let mut task_state = TaskState::get();
        let clock = task_state
            .clock
            .as_mut()
            .expect("virtual clock not enabled; call virtual_clock_enable(true) first");
        clock.advance_to(target)
    }; // TaskState borrow dropped before waking
    for w in wakers {
        w.wake();
    }
    fired
}

/// Returns the current virtual time.
///
/// # Panics
///
/// Panics if the virtual clock is not enabled. Use
/// [`clock::clock_now()`](crate::clock::clock_now) for the dual-mode function
/// that falls back to real time.
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_now() -> std::time::Instant {
    TaskState::get()
        .clock
        .as_ref()
        .expect("virtual clock not enabled; call virtual_clock_enable(true) first")
        .now()
}

/// Returns the epoch (start time) of the virtual clock.
///
/// # Panics
///
/// Panics if the virtual clock is not enabled.
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_epoch() -> std::time::Instant {
    TaskState::get()
        .clock
        .as_ref()
        .expect("virtual clock not enabled; call virtual_clock_enable(true) first")
        .epoch()
}

/// Returns the deadline of the next pending virtual timer, or `None` if
/// there are no pending timers.
///
/// Useful for step-by-step time advancement:
///
/// ```rust,no_run
/// use kimojio::operations;
///
/// # async fn example() {
/// while let Some(next) = operations::virtual_clock_next_deadline() {
///     operations::virtual_clock_advance_to(next);
///     // Check state after each timer fires
/// }
/// # }
/// ```
///
/// # Panics
///
/// Panics if the virtual clock is not enabled.
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_next_deadline() -> Option<std::time::Instant> {
    TaskState::get()
        .clock
        .as_ref()
        .expect("virtual clock not enabled; call virtual_clock_enable(true) first")
        .next_deadline()
}

/// Returns the number of pending (unfired) virtual timers.
///
/// # Panics
///
/// Panics if the virtual clock is not enabled.
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_pending_timers() -> usize {
    TaskState::get()
        .clock
        .as_ref()
        .expect("virtual clock not enabled; call virtual_clock_enable(true) first")
        .pending_timers()
}

/// Installs a callback that controls virtual time advancement when the
/// runtime is idle (no tasks ready to poll).
///
/// The callback receives `(current_virtual_time, next_pending_timer_deadline)`
/// and returns how far to advance, or `None` to stop advancing and let the
/// runtime block in io_uring normally.
///
/// Common patterns:
/// - **Advance to next timer**: `|now, next| next.map(|d| d.saturating_duration_since(now))`
/// - **Fixed duration**: `|_, _| Some(Duration::from_millis(1))`
/// - **Conditional**: `move |_, _| if active.get() { Some(dur) } else { None }`
///
/// Only one callback is active at a time — calling this replaces any
/// previously installed callback. Use [`virtual_clock_clear_idle_advance`]
/// to remove the callback entirely.
///
/// # Panics
///
/// Panics if the virtual clock is not enabled.
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
/// use kimojio::operations;
///
/// # async fn example() {
/// operations::virtual_clock_enable(true);
///
/// // Advance to the next timer on every idle point
/// operations::virtual_clock_set_idle_advance(|now, next| {
///     next.map(|deadline| deadline.saturating_duration_since(now))
/// });
///
/// // Sleeps resolve automatically
/// operations::sleep(Duration::from_secs(60)).await.unwrap();
/// # }
/// ```
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_set_idle_advance(
    f: impl FnMut(std::time::Instant, Option<std::time::Instant>) -> Option<std::time::Duration>
    + 'static,
) {
    TaskState::get()
        .clock
        .as_mut()
        .expect("virtual clock not enabled; call virtual_clock_enable(true) first")
        .set_idle_advance_fn(Some(Box::new(f)));
}

/// Removes the idle advance callback, stopping automatic time advancement
/// when the runtime is idle.
///
/// After clearing, the runtime will block in io_uring when no tasks are
/// ready, just as it would without a virtual clock. Explicit advancement
/// via [`virtual_clock_advance`] and [`virtual_clock_advance_to`] is
/// unaffected.
///
/// # Panics
///
/// Panics if the virtual clock is not enabled.
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_clear_idle_advance() {
    TaskState::get()
        .clock
        .as_mut()
        .expect("virtual clock not enabled; call virtual_clock_enable(true) first")
        .set_idle_advance_fn(None);
}

/// Returns `true` if an idle advance callback is currently installed.
///
/// # Panics
///
/// Panics if the virtual clock is not enabled.
#[cfg(feature = "virtual-clock")]
pub fn virtual_clock_has_idle_advance() -> bool {
    TaskState::get()
        .clock
        .as_ref()
        .expect("virtual clock not enabled; call virtual_clock_enable(true) first")
        .has_idle_advance_fn()
}

#[cfg(test)]
mod test {
    use core::panic;
    use futures::future::join_all;
    use futures::stream::FuturesUnordered;
    use futures::{FutureExt, StreamExt, select};
    use rustix::fs::{Mode, OFlags};
    use rustix::net::{
        AddressFamily, RecvFlags, SendFlags, SocketAddrUnix, SocketType, bind, listen, socket,
    };
    use std::collections::HashMap;
    use std::num::ParseIntError;
    use std::time::{Duration, Instant};
    use std::{cell::Cell, rc::Rc};
    use uuid::Uuid;

    use crate::task::IO_URING_SUBMISSION_ENTRIES;

    use crate::operations::{self, TaskHandleError, io_scope, parse_version};
    use crate::{AsyncEvent, CanceledError, Errno, MutInPlaceCell};

    use super::{accept, recv, send, spawn_task};

    #[crate::test]
    async fn drop_futures_test() {
        let (read, write) = crate::pipe::bipipe();

        let mut buf1 = [0; 1];

        let mut read1 = operations::read(&read, &mut buf1);
        // this should poll both but return 2
        let result = select! {
            _ = read1 => 1,
            _ = operations::yield_io() => 2,
        };
        assert_eq!(result, 2);

        // do the write so the completion will arrive before it is dropped but
        // don't poll the read.
        operations::write(&write, b"53").await.unwrap();
        let mut buf2 = [0; 1];
        operations::read(&read, &mut buf2).await.unwrap();
        // read buf 2 to be sure the first part of the write completed, make sure
        // we got 3 and not 5
        assert_eq!('3', buf2[0] as char);

        // drop the lost read, which should drop after completion arrival
        drop(read1);

        // buf1 is available again
        let mut read3 = operations::read(&read, &mut buf1);
        let result = select! {
            _ = read3 => 1,
            _ = operations::yield_io() => 2,
        };
        assert_eq!(result, 2);
        // drop read3 which should cause it to cancel and we continue
        drop(read3);

        // now make sure we can do a read on the same fd and get all the expected results
        // the previous read is gone.
        operations::write(&write, b"64").await.unwrap();
        operations::read(&read, &mut buf2).await.unwrap();
        assert_eq!('6', buf2[0] as char);
    }

    #[test]
    fn yield_cpu_test() {
        crate::run_test_with_post_validate(operations::yield_cpu(), |stats| {
            assert!(stats.tasks_polled_cpu.get() > 0);
        });
    }

    #[test]
    fn yield_io_test() {
        crate::run_test_with_post_validate(operations::yield_io(), |stats| {
            assert!(stats.tasks_polled_io.get() > 0);
        });
    }

    #[crate::test]
    async fn spawn_test() {
        let shared = Rc::new(Cell::new(0i32));
        let shared1 = shared.clone();
        let task1 = operations::spawn_task(async move {
            shared1.set(shared1.get() + 1);
        });
        task1.await.unwrap();
        assert_eq!(1, shared.get());
    }

    #[crate::test]
    async fn spawn_io_test() {
        let shared = Rc::new(Cell::new(0i32));
        let event = Rc::new(AsyncEvent::new());
        event.reset();
        let task = {
            let event = event.clone();
            let shared = shared.clone();
            operations::spawn_task(async move {
                shared.set(shared.get() + 1);
                event.wait().await.unwrap();
                shared.set(shared.get() + 1);
            })
        };
        assert_eq!(0, shared.get());
        operations::yield_io().await;
        assert_eq!(1, shared.get());
        event.set();
        task.await.unwrap();
        assert_eq!(2, shared.get());
    }

    #[crate::test]
    async fn spawn_60k_tasks() {
        {
            let mut tasks: Vec<_> = Vec::new();
            for _ in 0..60000 {
                tasks.push(operations::spawn_task(async {}))
            }

            operations::yield_io().await;
        }

        for _ in 0..60000 {
            let handle = operations::spawn_task(async {});
            handle.await.unwrap();
        }
    }

    #[crate::test]
    async fn create_10000_pending_io_test() {
        const TASK_COUNT: usize = IO_URING_SUBMISSION_ENTRIES * 10;
        let count = Rc::new(Cell::new(0usize));
        let mut tasks = Vec::new();
        for _task_index in 0..TASK_COUNT {
            let count = count.clone();
            tasks.push(operations::spawn_task(async move {
                operations::sleep(Duration::from_millis(250)).await.unwrap();
                count.set(count.get() + 1);
            }));
        }

        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(TASK_COUNT, count.get());
    }

    #[crate::test]
    async fn unix_domain_socket_test() {
        let listener_socket = socket(AddressFamily::UNIX, SocketType::STREAM, None)
            .expect("Failed to create new UDS");
        bind(
            &listener_socket,
            &SocketAddrUnix::new_abstract_name("unix_domain_socket_test".as_bytes())
                .expect("Failed to create abstract name"),
        )
        .expect("Failed to bind socket");
        listen(&listener_socket, 1).expect("Failed to listen");

        let handle = spawn_task(async move {
            let client_socket = socket(AddressFamily::UNIX, SocketType::STREAM, None)
                .expect("Failed to create client socket");
            operations::connect_unix(
                &client_socket,
                &SocketAddrUnix::new_abstract_name("unix_domain_socket_test".as_bytes()).unwrap(),
            )
            .await
            .unwrap();

            let buf = [1u8; 8];
            send(&client_socket, &buf, SendFlags::empty(), None)
                .await
                .expect("Failed to write to socket");
        });

        let socket = accept(&listener_socket).await.expect("Failed to accept");
        let mut buf = [0u8; 8];
        recv(&socket, &mut buf, RecvFlags::empty(), None)
            .await
            .expect("Failed to recv");

        handle.await.unwrap();
    }

    #[crate::test]
    async fn recv_timeout_test() {
        let listener_socket = socket(AddressFamily::UNIX, SocketType::STREAM, None)
            .expect("Failed to create new UDS");
        bind(
            &listener_socket,
            &SocketAddrUnix::new_abstract_name("recv_timeout_test".as_bytes())
                .expect("Failed to create abstract name"),
        )
        .expect("Failed to bind socket");
        listen(&listener_socket, 1).expect("Failed to listen");

        let handle = spawn_task(async move {
            let client_socket = socket(AddressFamily::UNIX, SocketType::STREAM, None)
                .expect("Failed to create client socket");
            operations::connect_unix(
                &client_socket,
                &SocketAddrUnix::new_abstract_name("recv_timeout_test".as_bytes()).unwrap(),
            )
            .await
            .unwrap();
            // Return the socket so it stays alive until the caller drops it
            client_socket
        });

        let socket = accept(&listener_socket).await.expect("Failed to accept");
        let mut buf = [0u8; 8];
        assert_eq!(
            Errno::TIME,
            recv(
                &socket,
                &mut buf,
                RecvFlags::empty(),
                Some(Duration::from_millis(100)),
            )
            .await
            .expect_err("No timeout as expected")
        );

        // Keep client socket alive until after the recv timeout assertion
        let _client_socket = handle.await.unwrap();
    }

    #[test]
    fn parse_version_test() {
        assert_eq!(parse_version("6.2").unwrap(), (6, 2));
        assert_eq!(parse_version("5.17").unwrap(), (5, 17));
        assert_eq!(parse_version("5.17GARBAGE").unwrap(), (5, 17));
        assert_eq!(parse_version("5.17.8").unwrap(), (5, 17));
        assert_eq!(parse_version("5.17.8GARBAGE").unwrap(), (5, 17));
        assert_eq!(parse_version("5.17 More").unwrap(), (5, 17));
        assert_eq!(parse_version("5.17.8 More").unwrap(), (5, 17));
        assert_eq!(parse_version("5.17.").unwrap(), (5, 17));
        assert_eq!(parse_version("5.17. ").unwrap(), (5, 17));
        assert_eq!(parse_version("5.17. more").unwrap(), (5, 17));

        fn failed(x: Result<(u32, u32), ParseIntError>) -> bool {
            x.is_err()
        }

        assert!(failed(parse_version("5")));
        assert!(failed(parse_version("5.")));
        assert!(failed(parse_version("5.x")));
        assert!(failed(parse_version("y")));
        assert!(failed(parse_version("")));
        assert!(failed(parse_version("y.5")));
    }

    #[crate::test]
    async fn task_error_test() {
        let task_handle =
            operations::spawn_task(
                async move { Err(Errno::from_raw_os_error(1)) as Result<(), Errno> },
            );

        assert_eq!(1, task_handle.await.unwrap().unwrap_err().raw_os_error());
    }

    #[crate::test]
    async fn starvation_test() {
        let terminate_time = Instant::now() + Duration::from_secs(10);

        let done = Rc::new(Cell::new(false));
        let yield_count = Rc::new(Cell::new(0usize));
        let infinite_yield_task = {
            let done = done.clone();
            let yield_count = yield_count.clone();
            operations::spawn_task(async move {
                while !done.get() {
                    operations::yield_io().await;
                    yield_count.set(yield_count.get() + 1);

                    assert!(
                        Instant::now() < terminate_time,
                        "Short sleep should complete well before terminate_time"
                    );
                }
            })
        };

        let respawn_count = Rc::new(Cell::new(0usize));
        {
            let done = done.clone();
            let respawn_count = respawn_count.clone();

            async fn respawn(
                done: Rc<Cell<bool>>,
                respawn_count: Rc<Cell<usize>>,
                terminate_time: Instant,
            ) {
                assert!(
                    Instant::now() < terminate_time,
                    "Short sleep should complete well before terminate_time"
                );
                if !done.get() {
                    respawn_count.set(respawn_count.get() + 1);
                    operations::spawn_task(respawn(done, respawn_count, terminate_time));
                }
            }

            operations::spawn_task(respawn(done, respawn_count, terminate_time));
        }

        // sleep will create a pending I/O that wakes up this task.  If the
        // tasks above starve the loop, then this sleep will not complete
        // and the starvation inducing tasks will panic when they reach
        // terminate_time.
        operations::sleep(Duration::from_millis(100)).await.unwrap();
        done.set(true);

        infinite_yield_task.await.unwrap();

        assert!(yield_count.get() > 0);
        assert!(respawn_count.get() > 0);
    }

    #[crate::test]
    async fn test_abort() {
        let io_forever_task = operations::spawn_task(async {
            loop {
                operations::nop().await.unwrap()
            }
        });
        let wait_forever_task = operations::spawn_task(async {
            let event = AsyncEvent::new();
            event.wait().await.unwrap();
        });
        let yield_forever_task = operations::spawn_task(async {
            loop {
                operations::yield_io().await;
            }
        });

        // yield and let tasks start
        operations::yield_io().await;

        // abort them all
        io_forever_task.abort();
        wait_forever_task.abort();
        yield_forever_task.abort();

        fn get_abort_result(result: Result<(), TaskHandleError>) -> &'static str {
            match result {
                Ok(_) => panic!("Task should have been aborted"),
                Err(TaskHandleError::Canceled) => panic!("Task should have been aborted"),
                Err(TaskHandleError::Panic(payload)) => *payload.downcast::<&str>().unwrap(),
            }
        }

        // wait for them to complete with panic message
        let result = io_forever_task.await;
        assert_eq!(get_abort_result(result), "Task aborted");
        let result = wait_forever_task.await;
        assert_eq!(get_abort_result(result), "Task aborted");
        let result = yield_forever_task.await;
        assert_eq!(get_abort_result(result), "Task aborted");
    }

    #[crate::test]
    async fn test_wait_for_multiple_tasks_with_futures_unordered() {
        let task_count = 10;
        let mut futures = FuturesUnordered::new();
        for _ in 0..task_count {
            futures.push(operations::spawn_task(async {
                operations::sleep(Duration::from_millis(100)).await.unwrap();
            }));
        }

        while futures.next().await.is_some() {}
    }

    #[crate::test]
    async fn test_wait_and_join_task_handle() {
        let task = operations::spawn_task(async { 5 });

        let (_f1, f2) = futures::join!(task.wait(), task);

        assert_eq!(f2.unwrap(), 5);
    }

    #[crate::test]
    #[allow(clippy::async_yields_async)]
    async fn test_cancel_sleep_with_io_scope() {
        let sleep_a_long_time = io_scope(async move || {
            let mut sleep_a_long_time =
                operations::sleep(Duration::from_secs(3600)).map(|result| {
                    assert_eq!(result, Err(Errno::CANCELED));
                    "it was canceled"
                });

            futures::select! {
                _ = sleep_a_long_time => panic!("sleep should not return"),
                default => {}
            }
            sleep_a_long_time
        })
        .await;

        let result = sleep_a_long_time.await;

        assert_eq!(result.to_string(), "it was canceled".to_string());
    }

    #[crate::test]
    async fn test_cancel_wait_with_io_scope() {
        let event = AsyncEvent::new();
        io_scope(async move || {
            let mut wait = event.wait();
            futures::select! {
                _ = wait => panic!("wait should not return"),
                default => {}
            }
            operations::io_scope_cancel();

            assert_eq!(wait.await, Err(CanceledError {}));

            event.set();
            assert_eq!(event.wait().await, Ok(()));
        })
        .await;
    }

    #[crate::test]
    async fn test_nested_io_scope_wait() {
        let event1 = AsyncEvent::new();
        let event2 = AsyncEvent::new();
        io_scope(async move || {
            let mut wait = event1.wait();
            futures::select! {
                _ = wait => panic!("wait should not return"),
                default => {}
            }

            io_scope(async move || {
                let mut wait2 = event2.wait();
                futures::select! {
                    _ = wait2 => panic!("wait should not return"),
                    default => {}
                }

                operations::io_scope_cancel();

                assert_eq!(wait2.await, Err(CanceledError {}));

                event2.set();
                assert_eq!(event2.wait().await, Ok(()));
            })
            .await;

            // still not complete
            futures::select! {
                _ = wait => panic!("wait should not return"),
                default => {}
            }

            operations::io_scope_cancel();

            assert_eq!(wait.await, Err(CanceledError {}));

            event1.set();
            assert_eq!(event1.wait().await, Ok(()));
        })
        .await;
    }

    #[crate::test]
    async fn test_nested_io_scope() {
        io_scope(async move || {
            let mut wait1 = operations::sleep(Duration::from_secs(3600));
            futures::select! {
                _ = wait1 => panic!("wait should not return"),
                default => {}
            }

            io_scope(async move || {
                let mut wait2 = operations::sleep(Duration::from_secs(3600));
                futures::select! {
                    _ = wait2 => panic!("wait should not return"),
                    default => {}
                }

                operations::io_scope_cancel();

                assert_eq!(wait2.await, Err(Errno::CANCELED));
            })
            .await;

            // still not complete
            futures::select! {
                _ = wait1 => panic!("wait should not return"),
                default => {}
            }

            operations::io_scope_cancel();

            assert_eq!(wait1.await, Err(Errno::CANCELED));
        })
        .await;
    }

    #[crate::test]
    async fn test_cancel_too_many_pending_io() {
        let nops = io_scope(async move || {
            let mut requests = Vec::new();
            for _ in 0..IO_URING_SUBMISSION_ENTRIES * 4 {
                requests.push(operations::sleep(Duration::from_secs(3600)));
            }

            operations::io_scope_cancel();

            // after cancelling we should be able to issue more I/O
            let nops = (0..IO_URING_SUBMISSION_ENTRIES).map(|_| operations::nop());

            let results = join_all(requests).await;
            for result in results {
                assert_eq!(result, Err(Errno::CANCELED));
            }

            nops
        })
        .await;

        for result in join_all(nops).await {
            assert_eq!(result, Ok(()));
        }
    }

    #[crate::test]
    async fn test_overflow_via_submit() {
        let mut ops = Vec::new();
        for _x in 0..IO_URING_SUBMISSION_ENTRIES * 100 {
            ops.push(operations::submit(operations::nop()));
        }
        for op in ops {
            let result = op.await;
            assert!(result.is_ok());
        }
    }

    #[crate::test]
    async fn test_too_many_pending_io() {
        let requests: Vec<_> = (0..IO_URING_SUBMISSION_ENTRIES * 100)
            .map(|_| operations::sleep(Duration::from_millis(100)))
            .collect();

        let results = join_all(requests).await;
        for result in results {
            result.unwrap();
        }
    }

    #[crate::test]
    async fn test_spawn_return_future() {
        #[allow(clippy::async_yields_async)]
        let task = operations::spawn_task(async {
            // don't await this one
            operations::nop()
        });
        let result = task.await.unwrap();
        assert_eq!(result.await, Ok(()));
    }

    #[test]
    fn test_ioend_activity_id() {
        const ACTIVITY_ID: Uuid = Uuid::from_u128(1);
        const TENANT_ID: Uuid = Uuid::from_u128(2);
        struct Tracer {
            id: MutInPlaceCell<HashMap<u32, Uuid>>,
        }
        impl Tracer {
            pub fn new() -> Self {
                Self {
                    id: MutInPlaceCell::new(HashMap::new()),
                }
            }
        }
        impl crate::TraceConfiguration for Tracer {
            fn trace(&self, event: crate::EventEnvelope) {
                match event.event {
                    crate::Events::IoStart {
                        activity_id, tag, ..
                    } => {
                        self.id.use_mut(|id| id.insert(tag, activity_id));
                    }
                    crate::Events::IoEnd {
                        activity_id, tag, ..
                    } => {
                        let expected_activity_id = self.id.use_mut(|id| id.remove(&tag));
                        assert_eq!(expected_activity_id, Some(activity_id));
                    }
                    _ => {}
                }
            }
        }
        let configuration = crate::configuration::Configuration::default()
            .set_trace_buffer_manager(Box::new(Tracer::new()));
        crate::run_with_configuration(
            0,
            async {
                operations::set_activity_id_and_tenant_id(ACTIVITY_ID, TENANT_ID);
                let mut sleep = operations::sleep(Duration::from_millis(100)).fuse();
                select! {
                    _ = sleep => panic!("not expected"),
                    default => {},
                }
                operations::set_activity_id_and_tenant_id(Uuid::nil(), Uuid::nil());
                sleep.await.unwrap();
            },
            configuration,
        )
        .unwrap()
        .unwrap();
    }

    #[cfg(feature = "fault_injection")]
    #[crate::test]
    async fn test_inject_fault() {
        operations::inject_fault(0, Errno::FAULT);
        assert_eq!(operations::nop().await, Err(Errno::FAULT));

        // after a fault, we are ok
        assert_eq!(operations::nop().await, Ok(()));

        // It takes too poll() calls to complete any I/O so
        // with a fault count of 1, this will still fail.
        operations::inject_fault(1, Errno::FAULT);
        assert_eq!(operations::nop().await, Err(Errno::FAULT));

        // after a fault, we are ok
        assert_eq!(operations::nop().await, Ok(()));

        // But with fault count of 2, we will skip the first and next will be Ok
        operations::inject_fault(2, Errno::FAULT);
        assert_eq!(operations::nop().await, Ok(()));
        assert_eq!(operations::nop().await, Err(Errno::FAULT));
        assert_eq!(operations::nop().await, Ok(()));
    }

    #[crate::test]
    async fn file_tests() {
        let root = c"/tmp/file_tests";
        let filename = c"/tmp/file_tests/file.txt";
        let newpath1 = c"/tmp/file_tests/file.txt-1.link";
        let newpath2 = c"/tmp/file_tests/file.txt-2.link";

        match operations::mkdir(root, 0o775.into()).await {
            Ok(()) => {}
            Err(Errno::EXIST) => println!("Directory {root:?} already exists"),
            Err(e) => panic!("Failed to create directory {root:?}: {e}"),
        }

        for name in [filename, newpath1, newpath2] {
            let stat = operations::stat(name).await;
            if stat.is_ok() {
                operations::unlink(name).await.unwrap();
            }
        }

        let file = operations::open(
            filename,
            OFlags::CREATE | OFlags::RDWR,
            Mode::from_raw_mode(0o666),
        )
        .await
        .unwrap();
        operations::link(filename, newpath1).await.unwrap();
        operations::rename(newpath1, newpath2).await.unwrap();
        operations::write(&file, b"hello, world!").await.unwrap();
        operations::pwrite(&file, b"Gdday", 0).await.unwrap();
        operations::pwrite_polled(&file, b"mate!", 7, false)
            .await
            .unwrap();
        operations::fsync(&file).await.unwrap();
        let mut buf = [0u8; 1024];
        let amount = operations::pread(&file, &mut buf, 0).await.unwrap();
        assert_eq!(amount, 13);
        assert_eq!(&buf[..13], b"Gdday, mate!!");
        operations::close(file);
        operations::unlink(filename).await.unwrap();

        let file = operations::open(newpath2, OFlags::RDONLY, Mode::from_raw_mode(0o666))
            .await
            .unwrap();
        let stat = operations::fstat(&file).await.unwrap();
        assert_eq!(stat.stx_size, 13);
        let amount = operations::read_with_deadline(
            &file,
            &mut buf[..13],
            Some(Instant::now() + Duration::from_secs(120)),
        )
        .await
        .unwrap();
        assert_eq!(amount, 13);
        assert_eq!(&buf[..13], b"Gdday, mate!!");
        operations::close(file);
        operations::unlink(newpath2).await.unwrap();
        operations::rmdir(root).await.unwrap();
    }

    // --- Virtual Clock Tests ---
    // These tests use a local run_virtual helper that sets up a virtual clock
    // via operations::virtual_clock_enable(true).

    #[cfg(feature = "virtual-clock")]
    mod virtual_clock_tests {
        use crate::configuration::Configuration;
        use crate::operations;
        use crate::{Runtime, TimeoutError};
        use std::cell::Cell;
        use std::rc::Rc;
        use std::task::Poll;
        use std::time::Duration;

        /// Helper: run a test with a virtual clock runtime.
        fn run_virtual<Fut>(test: Fut)
        where
            Fut: std::future::Future<Output = ()> + 'static,
        {
            let mut runtime = Runtime::new(0, Configuration::new());
            let result = runtime.block_on(async {
                operations::virtual_clock_enable(true);
                test.await;
            });
            if let Some(Err(payload)) = result {
                std::panic::resume_unwind(payload);
            }
        }

        // --- P1: Virtual sleep completes on advance ---

        #[test]
        fn virtual_sleep_completes_on_advance() {
            run_virtual(async {
                use std::pin::pin;

                let mut sleep = pin!(operations::sleep(Duration::from_secs(60)));

                // Poll once to register timer
                let completed = futures::future::poll_fn(|cx| match sleep.as_mut().poll(cx) {
                    Poll::Pending => Poll::Ready(false),
                    Poll::Ready(_) => Poll::Ready(true),
                })
                .await;
                assert!(!completed, "should be pending before advance");

                // Advance past the deadline
                operations::virtual_clock_advance(Duration::from_secs(60));

                // Now await — should complete immediately
                sleep.await.unwrap();
            });
        }

        #[test]
        fn virtual_sleep_stays_pending_without_advance() {
            run_virtual(async {
                use std::pin::pin;

                let mut sleep = pin!(operations::sleep(Duration::from_secs(10)));

                // Poll multiple times without advancing — should stay Pending
                for _ in 0..3 {
                    let completed = futures::future::poll_fn(|cx| match sleep.as_mut().poll(cx) {
                        Poll::Pending => Poll::Ready(false),
                        Poll::Ready(_) => Poll::Ready(true),
                    })
                    .await;
                    assert!(!completed);
                }

                // Advance partially — still pending
                operations::virtual_clock_advance(Duration::from_secs(5));
                let completed = futures::future::poll_fn(|cx| match sleep.as_mut().poll(cx) {
                    Poll::Pending => Poll::Ready(false),
                    Poll::Ready(_) => Poll::Ready(true),
                })
                .await;
                assert!(!completed);

                // Advance to exact deadline — now completes
                operations::virtual_clock_advance(Duration::from_secs(5));
                sleep.await.unwrap();
            });
        }

        #[test]
        fn multiple_virtual_sleeps_wake_in_order() {
            run_virtual(async {
                let order = Rc::new(std::cell::RefCell::new(Vec::new()));
                let o1 = order.clone();
                let o2 = order.clone();
                let o3 = order.clone();

                operations::spawn_task(async move {
                    operations::sleep(Duration::from_secs(30)).await.unwrap();
                    o1.borrow_mut().push(30);
                });
                operations::spawn_task(async move {
                    operations::sleep(Duration::from_secs(10)).await.unwrap();
                    o2.borrow_mut().push(10);
                });
                operations::spawn_task(async move {
                    operations::sleep(Duration::from_secs(20)).await.unwrap();
                    o3.borrow_mut().push(20);
                });

                // Let all tasks register their timers
                operations::yield_io().await;

                // Advance past all deadlines at once
                operations::virtual_clock_advance(Duration::from_secs(30));
                // Yield enough times for all tasks to run
                for _ in 0..5 {
                    operations::yield_io().await;
                }

                assert_eq!(*order.borrow(), vec![10, 20, 30]);
            });
        }

        #[test]
        fn virtual_sleep_60s_completes_fast() {
            let wall_start = std::time::Instant::now();
            run_virtual(async {
                use std::pin::pin;
                let mut sleep = pin!(operations::sleep(Duration::from_secs(60)));

                // Poll once to register timer
                futures::future::poll_fn(|cx| {
                    let _ = sleep.as_mut().poll(cx);
                    Poll::Ready(())
                })
                .await;

                operations::virtual_clock_advance(Duration::from_secs(60));
                sleep.await.unwrap();
            });
            // Must complete in <1s wall time (spec: <10ms, but allow margin for CI)
            assert!(wall_start.elapsed() < Duration::from_secs(1));
        }

        #[test]
        fn spawned_virtual_sleep_completes_on_advance() {
            run_virtual(async {
                let done = Rc::new(Cell::new(false));
                let d = done.clone();

                operations::spawn_task(async move {
                    operations::sleep(Duration::from_secs(60)).await.unwrap();
                    d.set(true);
                });

                // Let spawned task start and register its timer
                operations::yield_io().await;
                assert!(!done.get());

                // Advance past deadline
                operations::virtual_clock_advance(Duration::from_secs(60));

                // Extra yields: yield-once means the sleep yields once
                // before completing, then the spawned task finishes
                operations::yield_io().await;
                operations::yield_io().await;
                assert!(done.get());
            });
        }

        // --- P3: sleep_until ---

        #[test]
        fn sleep_until_completes_at_deadline() {
            run_virtual(async {
                use std::pin::pin;
                let deadline = operations::virtual_clock_now() + Duration::from_secs(5);
                let mut sleep = pin!(operations::sleep_until(deadline));

                let completed = futures::future::poll_fn(|cx| match sleep.as_mut().poll(cx) {
                    Poll::Pending => Poll::Ready(false),
                    Poll::Ready(_) => Poll::Ready(true),
                })
                .await;
                assert!(!completed);

                operations::virtual_clock_advance(Duration::from_secs(5));
                sleep.await.unwrap();
            });
        }

        #[test]
        fn sleep_until_past_deadline_completes_immediately() {
            run_virtual(async {
                let past = operations::virtual_clock_now() - Duration::from_secs(1);
                operations::sleep_until(past).await.unwrap();
            });
        }

        // --- P4: timeout_at ---

        #[test]
        fn timeout_at_returns_inner_result_on_fast_future() {
            run_virtual(async {
                let deadline = operations::virtual_clock_now() + Duration::from_secs(10);
                let result = operations::timeout_at(deadline, async { 42 }).await;
                assert_eq!(result, Ok(42));
            });
        }

        #[test]
        fn timeout_at_returns_timeout_on_advance() {
            run_virtual(async {
                let deadline = operations::virtual_clock_now() + Duration::from_secs(5);

                let done = Rc::new(Cell::new(None::<Result<(), TimeoutError>>));
                let d = done.clone();

                operations::spawn_task(async move {
                    let result =
                        operations::timeout_at(deadline, std::future::pending::<()>()).await;
                    d.set(Some(result));
                });

                operations::yield_io().await;
                assert!(done.get().is_none(), "should still be pending");

                // Advance past deadline — should trigger timeout.
                operations::virtual_clock_advance(Duration::from_secs(5));
                // Extra yields for yield-once cooperative scheduling
                operations::yield_io().await;
                operations::yield_io().await;

                assert_eq!(done.get(), Some(Err(TimeoutError::Timeout)));
            });
        }

        #[test]
        fn timeout_at_past_deadline_immediate_timeout() {
            run_virtual(async {
                let past_deadline = operations::virtual_clock_now() - Duration::from_secs(1);
                let result =
                    operations::timeout_at(past_deadline, std::future::pending::<()>()).await;
                assert_eq!(result, Err(TimeoutError::Timeout));
            });
        }

        // --- P5: Drop cancellation ---

        #[test]
        fn dropped_virtual_sleep_cancels_timer() {
            run_virtual(async {
                assert_eq!(operations::virtual_clock_pending_timers(), 0);

                {
                    use std::pin::pin;
                    let mut sleep = pin!(operations::sleep(Duration::from_secs(100)));

                    // Poll once to register the timer
                    futures::future::poll_fn(|cx| {
                        let _ = sleep.as_mut().poll(cx);
                        Poll::Ready(())
                    })
                    .await;
                    assert_eq!(operations::virtual_clock_pending_timers(), 1);
                }
                // sleep dropped when scope ends — timer should be cancelled
                assert_eq!(operations::virtual_clock_pending_timers(), 0);
            });
        }

        #[test]
        fn dropped_virtual_sleep_no_spurious_wakeup_on_advance() {
            run_virtual(async {
                {
                    use std::pin::pin;
                    let mut sleep = pin!(operations::sleep(Duration::from_secs(10)));

                    // Poll once to register timer
                    futures::future::poll_fn(|cx| {
                        let _ = sleep.as_mut().poll(cx);
                        Poll::Ready(())
                    })
                    .await;
                }
                // sleep dropped — timer cancelled
                assert_eq!(operations::virtual_clock_pending_timers(), 0);

                // Advancing should not panic or cause issues
                let fired = operations::virtual_clock_advance(Duration::from_secs(10));
                assert_eq!(fired, 0);
            });
        }

        #[test]
        fn unpolled_virtual_sleep_drops_cleanly() {
            run_virtual(async {
                // Create and immediately drop without polling
                let sleep = operations::sleep(Duration::from_secs(10));
                drop(sleep);
                // No timer should have been registered
                assert_eq!(operations::virtual_clock_pending_timers(), 0);
            });
        }

        // --- Idle advance callback tests ---

        #[test]
        fn callback_completes_sleep() {
            run_virtual(async {
                operations::virtual_clock_set_idle_advance(|now, next| {
                    next.map(|d| d.saturating_duration_since(now))
                });
                operations::sleep(Duration::from_secs(60)).await.unwrap();
            });
        }

        #[test]
        fn callback_60s_completes_fast() {
            let wall_start = std::time::Instant::now();
            run_virtual(async {
                operations::virtual_clock_set_idle_advance(|now, next| {
                    next.map(|d| d.saturating_duration_since(now))
                });
                operations::sleep(Duration::from_secs(60)).await.unwrap();
            });
            assert!(wall_start.elapsed() < Duration::from_secs(1));
        }

        #[test]
        fn callback_fires_timers_in_sequence() {
            run_virtual(async {
                let epoch = operations::virtual_clock_epoch();

                operations::virtual_clock_set_idle_advance(|now, next| {
                    next.map(|d| d.saturating_duration_since(now))
                });

                operations::sleep(Duration::from_secs(10)).await.unwrap();
                assert_eq!(
                    operations::virtual_clock_now().duration_since(epoch),
                    Duration::from_secs(10)
                );

                operations::sleep(Duration::from_secs(20)).await.unwrap();
                assert_eq!(
                    operations::virtual_clock_now().duration_since(epoch),
                    Duration::from_secs(30)
                );
            });
        }

        #[test]
        fn callback_wakes_spawned_task() {
            run_virtual(async {
                let done = Rc::new(Cell::new(false));
                let d = done.clone();

                operations::virtual_clock_set_idle_advance(|now, next| {
                    next.map(|d| d.saturating_duration_since(now))
                });

                operations::spawn_task(async move {
                    operations::sleep(Duration::from_secs(60)).await.unwrap();
                    d.set(true);
                });

                operations::sleep(Duration::from_secs(60)).await.unwrap();
                operations::yield_io().await;

                assert!(done.get());
            });
        }

        #[test]
        fn fixed_callback_completes_sleep() {
            let wall_start = std::time::Instant::now();
            run_virtual(async {
                operations::virtual_clock_set_idle_advance(|_, _| Some(Duration::from_millis(1)));
                operations::sleep(Duration::from_secs(60)).await.unwrap();
            });
            assert!(wall_start.elapsed() < Duration::from_secs(1));
        }

        #[test]
        fn no_callback_is_noop() {
            run_virtual(async {
                // No callback installed — clock should not advance
                let epoch = operations::virtual_clock_epoch();
                assert_eq!(operations::virtual_clock_now(), epoch);
                assert!(!operations::virtual_clock_has_idle_advance());
            });
        }

        #[test]
        fn replace_callback_changes_behavior() {
            run_virtual(async {
                let epoch = operations::virtual_clock_epoch();

                // Install advance-to-next callback
                operations::virtual_clock_set_idle_advance(|now, next| {
                    next.map(|d| d.saturating_duration_since(now))
                });
                operations::sleep(Duration::from_secs(60)).await.unwrap();
                assert_eq!(
                    operations::virtual_clock_now().duration_since(epoch),
                    Duration::from_secs(60)
                );

                // Replace with fixed 1ms callback
                operations::virtual_clock_set_idle_advance(|_, _| Some(Duration::from_millis(1)));
                operations::sleep(Duration::from_millis(10)).await.unwrap();
                assert!(
                    operations::virtual_clock_now().duration_since(epoch)
                        >= Duration::from_secs(60) + Duration::from_millis(10)
                );
            });
        }
    }
}
