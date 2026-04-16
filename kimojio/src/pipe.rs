// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `bipipe()` returns a pair of read-write pipes where the read
//! of one is connected to the write of the other.
//!
//! # Usage
//!
//! ```
//! use kimojio::{Errno, pipe};
//! async fn create_some_pipes() {
//!     let (readwrite_pipe_client, readwrite_pipe_server) = pipe::bipipe();
//! }
//! ```
//!
#[cfg(not(target_os = "windows"))]
use crate::Errno;
use rustix::fd::OwnedFd;

#[cfg(not(target_os = "windows"))]
use std::io::Error;

#[cfg(not(target_os = "windows"))]
use libc::pipe2;
#[cfg(not(target_os = "windows"))]
use rustix::{
    fd::FromRawFd,
    net::{AddressFamily, SocketFlags, SocketType, socketpair},
};

/// Creates a pair of bidirectional connected pipes.
///
/// Returns two file descriptors where data written to one can be read from
/// the other.
#[cfg(not(target_os = "windows"))]
pub fn bipipe() -> (OwnedFd, OwnedFd) {
    socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .unwrap()
}

/// Creates a pair of bidirectional connected pipes using a TCP loopback pair.
///
/// Windows does not have `AF_UNIX` socketpairs available on all versions, so
/// we fall back to a localhost TCP pair.
#[cfg(target_os = "windows")]
pub fn bipipe() -> (OwnedFd, OwnedFd) {
    use std::net::TcpListener;
    use std::os::windows::io::{FromRawSocket, IntoRawSocket};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bipipe: bind failed");
    let port = listener.local_addr().unwrap().port();
    let client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("bipipe: connect failed");
    let (server, _) = listener.accept().expect("bipipe: accept failed");

    let client_raw = client.into_raw_socket();
    let server_raw = server.into_raw_socket();

    // SAFETY: client_raw and server_raw are valid Windows SOCKET handles.
    unsafe {
        (
            OwnedFd::from_raw_socket(client_raw),
            OwnedFd::from_raw_socket(server_raw),
        )
    }
}

/// Create a unidirectional pipe for IPC/inter-thread communication. The first
/// OwnedFd is the read end of the pipe, while the second OwnedFd is the write
/// end of the pipe.
#[cfg(not(target_os = "windows"))]
pub fn pipe() -> Result<(OwnedFd, OwnedFd), Errno> {
    let mut fds = [0i32; 2];
    // SAFETY: Safe because the fds array is 2 elements long as requires to
    // safely call pipe2()
    if unsafe { pipe2(fds.as_mut_ptr(), 0) } < 0 {
        Err(Errno::from_io_error(&Error::last_os_error()).unwrap())
    } else {
        Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
    }
}

#[cfg(test)]
mod test {
    use std::{ffi::c_void, io::IoSlice};

    #[cfg(io_uring_backend)]
    use rustix::io_uring::iovec;

    use crate::{operations, pipe::pipe};

    #[cfg(io_uring_backend)]
    #[crate::test]
    async fn pipe_test() {
        let (read, write) = pipe().unwrap();

        let output1 = [b'h', b'e', b'l', b'l', b'o'];
        let output2 = [b'w', b'o', b'r', b'l', b'd'];
        let output = [IoSlice::new(&output1), IoSlice::new(&output2)];
        operations::writev(&write, &output[..], None).await.unwrap();
        let mut buf1 = [0u8; 3];
        let mut buf2 = [0u8; 7];
        let ptr1 = buf1.as_mut_ptr();
        let ptr2 = buf2.as_mut_ptr();
        let iovec1 = iovec {
            iov_base: ptr1 as *mut c_void,
            iov_len: buf1.len(),
        };
        let iovec2 = iovec {
            iov_base: ptr2 as *mut c_void,
            iov_len: buf2.len(),
        };
        let mut input = &mut [iovec1, iovec2][..];
        while !input.is_empty() {
            let mut amount = operations::readv(&read, input, None).await.unwrap();
            while amount > 0 {
                if amount >= input[0].iov_len {
                    amount -= input[0].iov_len;
                    input = &mut input[1..];
                } else {
                    input[0].iov_base = unsafe { input[0].iov_base.add(amount) };
                    input[0].iov_len -= amount;
                }
            }
        }
    }
}
