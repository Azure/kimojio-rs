// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
use std::ffi::CStr;
use std::marker::PhantomData;
use std::os::raw::{c_char, c_int, c_ulong};
use std::ptr::{NonNull, null_mut};
use std::{cell::Cell, rc::Rc};

use foreign_types_shared::{ForeignType, ForeignTypeRef};
use openssl::{
    error::ErrorStack,
    ssl::{Ssl, SslContextRef, SslRef},
    version as openssl_version,
};
use openssl_sys as ffi;
use rustix_uring::Errno;

unsafe extern "C" {
    fn BIO_new_bio_pair(
        bio1: *mut *mut ffi::BIO,
        writebuf1: usize,
        bio2: *mut *mut ffi::BIO,
        writebuf2: usize,
    ) -> c_int;
    fn BIO_nwrite0(bio: *mut ffi::BIO, buf: *mut *mut c_char) -> c_int;
    fn BIO_nwrite(bio: *mut ffi::BIO, buf: *mut *mut c_char, num: c_int) -> c_int;
    fn BIO_nread0(bio: *mut ffi::BIO, buf: *mut *mut c_char) -> c_int;
    fn BIO_nread(bio: *mut ffi::BIO, buf: *mut *mut c_char, num: c_int) -> c_int;
}

const SSL_ERROR_WANT_ASYNC: c_int = 9;
const SSL_ERROR_WANT_ASYNC_JOB: c_int = 10;
const SSL_ERROR_WANT_CLIENT_HELLO_CB: c_int = 11;
const SSL_ERROR_WANT_RETRY_VERIFY: c_int = 12;

#[allow(non_camel_case_types)]
#[derive(Debug)]
pub enum OpensslErrorType {
    SSL_ERROR_NONE = 0,
    SSL_ERROR_SSL = 1,
    SSL_ERROR_WANT_READ = 2,
    SSL_ERROR_WANT_WRITE = 3,
    SSL_ERROR_WANT_X509_LOOKUP = 4,
    SSL_ERROR_SYSCALL = 5,
    SSL_ERROR_ZERO_RETURN = 6,
    SSL_ERROR_WANT_CONNECT = 7,
    SSL_ERROR_WANT_ACCEPT = 8,
    SSL_ERROR_WANT_ASYNC = 9,
    SSL_ERROR_WANT_ASYNC_JOB = 10,
    SSL_ERROR_WANT_CLIENT_HELLO_CB = 11,
    SSL_ERROR_WANT_RETRY_VERIFY = 12,

    InvalidErrorCode,
}

#[derive(Debug)]
pub enum TlsServerError {
    Errno(Errno),
    TlsError(ErrorStack),
}

impl std::error::Error for TlsServerError {}

impl std::fmt::Display for TlsServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsServerError::Errno(errno) => std::fmt::Display::fmt(errno, f),
            TlsServerError::TlsError(error_stack) => {
                for error in error_stack.errors() {
                    let message = std::fmt::format(format_args!("TlsError {error}\n"));
                    f.write_str(&message)?;
                }
                Ok(())
            }
        }
    }
}

pub fn get_error_details(code: u64) -> (String, String, Option<String>) {
    let code = to_openssl_error_code(code);
    let lib = openssl_error_string(unsafe { ffi::ERR_lib_error_string(code) });
    let func = openssl_error_string(unsafe { ffi::ERR_func_error_string(code) });
    let reason = unsafe { ffi::ERR_reason_error_string(code) };
    let reason = if reason.is_null() {
        None
    } else {
        Some(openssl_error_string(reason))
    };

    (lib, func, reason)
}

#[allow(clippy::unnecessary_cast)]
fn to_openssl_error_code(code: u64) -> c_ulong {
    code as c_ulong
}

fn openssl_error_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

pub struct TlsServer {
    inner: Box<TlsServerInner>,
}

pub struct TlsServerShared {
    inner: Rc<TlsServerInner>,
}

struct TlsServerInner {
    ssl: Ssl,
    application_bio: ApplicationBio,
    state: Cell<TlsHandleState>,
}

// SAFETY: TlsServerInner owns an SSL/BIO pair and is only Send through the
// unique TlsServer owner. Shared split handles use Rc<TlsServerInner> and remain
// !Send, so OpenSSL state cannot be accessed concurrently across threads.
unsafe impl Send for TlsServerInner {}

#[derive(Clone, Copy)]
enum TlsHandleState {
    HandshakeWantWrite,
    Handshake,
    Started,
}

pub enum Response {
    Success(usize),
    Fail(TlsServerError),
    Eof,
    WantRead,
    WantWrite,
}

fn get_ssl_error() -> ErrorStack {
    ErrorStack::get()
}

fn ssl_stack_error() -> Response {
    Response::Fail(TlsServerError::TlsError(get_ssl_error()))
}

fn ssl_error_response(error: c_int) -> Response {
    match error {
        ffi::SSL_ERROR_NONE => Response::Fail(TlsServerError::Errno(Errno::INVAL)),
        ffi::SSL_ERROR_SSL => Response::Fail(TlsServerError::TlsError(get_ssl_error())),
        ffi::SSL_ERROR_WANT_READ => Response::WantRead,
        ffi::SSL_ERROR_WANT_WRITE => Response::WantWrite,
        ffi::SSL_ERROR_WANT_X509_LOOKUP => Response::Fail(TlsServerError::Errno(Errno::PROTO)),
        ffi::SSL_ERROR_SYSCALL => Response::Fail(TlsServerError::TlsError(get_ssl_error())),
        ffi::SSL_ERROR_ZERO_RETURN => Response::Eof,
        ffi::SSL_ERROR_WANT_CONNECT
        | ffi::SSL_ERROR_WANT_ACCEPT
        | SSL_ERROR_WANT_ASYNC
        | SSL_ERROR_WANT_ASYNC_JOB
        | SSL_ERROR_WANT_CLIENT_HELLO_CB
        | SSL_ERROR_WANT_RETRY_VERIFY => Response::Fail(TlsServerError::Errno(Errno::PROTO)),
        _ => Response::Fail(TlsServerError::Errno(Errno::INVAL)),
    }
}

fn handle_io_failure(ssl: *mut ffi::SSL, result: c_int) -> Response {
    if result > 0 {
        Response::Success(result as usize)
    } else if result == 0 {
        Response::Eof
    } else {
        ssl_error_response(unsafe { ffi::SSL_get_error(ssl, result) })
    }
}

fn ssl_io_response_from_error(result: c_int, ssl_error: c_int) -> Response {
    if result > 0 {
        Response::Success(result as usize)
    } else {
        ssl_error_response(ssl_error)
    }
}

fn ssl_io_response(ssl: *mut ffi::SSL, result: c_int) -> Response {
    ssl_io_response_from_error(result, unsafe { ffi::SSL_get_error(ssl, result) })
}

fn c_int_len(len: usize) -> Result<c_int, TlsServerError> {
    c_int::try_from(len).map_err(|_| TlsServerError::Errno(Errno::INVAL))
}

struct ApplicationBio {
    bio: NonNull<ffi::BIO>,
    #[cfg(debug_assertions)]
    in_write_callback: Cell<bool>,
    in_read_callback: Cell<bool>,
}

impl ApplicationBio {
    fn new(bio: NonNull<ffi::BIO>) -> Self {
        Self {
            bio,
            #[cfg(debug_assertions)]
            in_write_callback: Cell::new(false),
            in_read_callback: Cell::new(false),
        }
    }

    fn available_write_len(&self) -> Option<usize> {
        self.with_write_buffer(|buffer| (0, buffer.len()))
    }

    fn write_lease(&self) -> Option<ApplicationBioWriteLease<'_>> {
        let guard = self.enter_callback(ApplicationBioDirection::Write);
        let mut buf = null_mut();
        let size = unsafe { BIO_nwrite0(self.bio.as_ptr(), &mut buf) };
        if size <= 0 {
            return None;
        }

        let buffer = NonNull::new(buf.cast())?;
        Some(ApplicationBioWriteLease {
            bio: self.bio,
            buffer,
            len: size as usize,
            _guard: guard,
            _not_send: PhantomData,
        })
    }

    fn with_write_buffer<R>(&self, f: impl FnOnce(&mut [u8]) -> (usize, R)) -> Option<R> {
        let _guard = self.enter_callback(ApplicationBioDirection::Write);
        let mut buf = null_mut();
        let size = unsafe { BIO_nwrite0(self.bio.as_ptr(), &mut buf) };
        if size <= 0 {
            return None;
        }

        let buffer = unsafe { std::slice::from_raw_parts_mut(buf.cast(), size as usize) };
        let (amount, result) = f(buffer);
        let amount = c_int_len(amount).expect("BIO write length exceeds c_int");
        let mut buf = null_mut();
        let written = unsafe { BIO_nwrite(self.bio.as_ptr(), &mut buf, amount) };
        assert_eq!(written, amount);
        Some(result)
    }

    fn read_lease(&self) -> Option<ApplicationBioReadLease<'_>> {
        let guard = self.enter_callback(ApplicationBioDirection::Read);
        let mut buf = null_mut();
        let size = unsafe { BIO_nread0(self.bio.as_ptr(), &mut buf) };
        if size <= 0 {
            return None;
        }

        let buffer = NonNull::new(buf.cast())?;
        Some(ApplicationBioReadLease {
            bio: self.bio,
            buffer,
            len: size as usize,
            _guard: guard,
            _not_send: PhantomData,
        })
    }

    fn enter_callback(
        &self,
        direction: ApplicationBioDirection,
    ) -> ApplicationBioCallbackGuard<'_> {
        match direction {
            ApplicationBioDirection::Read => assert!(
                !self.in_read_callback.replace(true),
                "reentrant OpenSSL BIO read-buffer access"
            ),
            ApplicationBioDirection::Write => {
                #[cfg(debug_assertions)]
                assert!(
                    !self.in_write_callback.replace(true),
                    "reentrant OpenSSL BIO write-buffer access"
                );
            }
        }
        ApplicationBioCallbackGuard {
            bio: self,
            direction,
        }
    }
}

impl Drop for ApplicationBio {
    fn drop(&mut self) {
        unsafe { ffi::BIO_free_all(self.bio.as_ptr()) };
    }
}

#[derive(Clone, Copy)]
enum ApplicationBioDirection {
    Read,
    Write,
}

struct ApplicationBioCallbackGuard<'a> {
    bio: &'a ApplicationBio,
    direction: ApplicationBioDirection,
}

impl Drop for ApplicationBioCallbackGuard<'_> {
    fn drop(&mut self) {
        match self.direction {
            ApplicationBioDirection::Read => self.bio.in_read_callback.set(false),
            ApplicationBioDirection::Write => {
                #[cfg(debug_assertions)]
                self.bio.in_write_callback.set(false);
            }
        }
    }
}

pub struct ApplicationBioWriteLease<'a> {
    bio: NonNull<ffi::BIO>,
    buffer: NonNull<u8>,
    len: usize,
    _guard: ApplicationBioCallbackGuard<'a>,
    _not_send: PhantomData<Rc<()>>,
}

impl AsMut<[u8]> for ApplicationBioWriteLease<'_> {
    fn as_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.buffer.as_ptr(), self.len) }
    }
}

impl ApplicationBioWriteLease<'_> {
    pub fn commit(self, amount: usize) {
        let amount = c_int_len(amount).expect("BIO write length exceeds c_int");
        let mut buf = null_mut();
        let written = unsafe { BIO_nwrite(self.bio.as_ptr(), &mut buf, amount) };
        assert_eq!(written, amount);
    }
}

pub struct ApplicationBioReadLease<'a> {
    bio: NonNull<ffi::BIO>,
    buffer: NonNull<u8>,
    len: usize,
    _guard: ApplicationBioCallbackGuard<'a>,
    _not_send: PhantomData<Rc<()>>,
}

impl AsRef<[u8]> for ApplicationBioReadLease<'_> {
    fn as_ref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.buffer.as_ptr(), self.len) }
    }
}

impl ApplicationBioReadLease<'_> {
    pub fn consume(self, amount: usize) {
        let amount = c_int_len(amount).expect("BIO read length exceeds c_int");
        let mut buf = null_mut();
        let read = unsafe { BIO_nread(self.bio.as_ptr(), &mut buf, amount) };
        assert_eq!(read, amount);
    }
}

impl Drop for TlsServerInner {
    fn drop(&mut self) {}
}

/// Gets the minimum TLS protocol version from an OpenSSL context.
pub fn get_min_proto_version(ctx: &SslContextRef) -> i32 {
    unsafe { ffi::SSL_CTX_get_min_proto_version(ctx.as_ptr()) }
}

impl TlsServer {
    /// Creates a TLS handle from an owned OpenSSL `Ssl`.
    pub fn from_ssl(
        mut ssl: Ssl,
        bufsize: usize,
        is_server: bool,
    ) -> Result<TlsServer, TlsServerError> {
        let mut ssl_bio = null_mut();
        let mut server_io = null_mut();
        let result = unsafe { BIO_new_bio_pair(&mut ssl_bio, bufsize, &mut server_io, bufsize) };
        if result != 1 {
            return match ssl_stack_error() {
                Response::Fail(error) => Err(error),
                _ => unreachable!(),
            };
        }
        let Some(ssl_bio) = NonNull::new(ssl_bio) else {
            return Err(TlsServerError::Errno(Errno::INVAL));
        };
        let Some(server_io) = NonNull::new(server_io) else {
            unsafe { ffi::BIO_free_all(ssl_bio.as_ptr()) };
            return Err(TlsServerError::Errno(Errno::INVAL));
        };

        if is_server {
            ssl.set_accept_state();
        } else {
            ssl.set_connect_state();
        }

        unsafe { ffi::SSL_set_bio(ssl.as_ptr(), ssl_bio.as_ptr(), ssl_bio.as_ptr()) };

        Ok(TlsServer {
            inner: Box::new(TlsServerInner {
                ssl,
                application_bio: ApplicationBio::new(server_io),
                state: Cell::new(TlsHandleState::HandshakeWantWrite),
            }),
        })
    }

    pub fn split(self) -> (TlsServerShared, TlsServerShared) {
        let TlsServer { inner } = self;
        let inner = Rc::new(*inner);
        (
            TlsServerShared {
                inner: inner.clone(),
            },
            TlsServerShared { inner },
        )
    }
}

macro_rules! impl_tls_server_methods {
    ($ty:ty) => {
        impl $ty {
            pub fn client_side_handshake(&mut self) -> Response {
                self.inner.do_handshake()
            }

            /// Represents the server side execution of a TLS handshake with a client.
            pub fn server_side_handshake(&mut self) -> Response {
                self.inner.do_handshake()
            }

            /// Gets the reference to SSL object.
            pub fn get_ssl(&self) -> &SslRef {
                self.inner.get_ssl()
            }

            pub fn shutdown(&mut self) -> Response {
                self.inner.shutdown()
            }

            pub fn read(&mut self, buffer: &mut [u8]) -> Response {
                self.inner.read(buffer)
            }

            pub fn write(&mut self, buffer: &[u8]) -> Response {
                self.inner.write(buffer)
            }

            pub fn push_buffer_capacity(&self) -> Option<usize> {
                self.inner.push_buffer_capacity()
            }

            pub fn write_lease(&self) -> Option<ApplicationBioWriteLease<'_>> {
                self.inner.write_lease()
            }

            pub fn read_lease(&self) -> Option<ApplicationBioReadLease<'_>> {
                self.inner.read_lease()
            }

            pub fn push_bytes(&self, bytes: &[u8]) -> Option<usize> {
                self.inner.push_bytes(bytes)
            }
        }
    };
}

impl_tls_server_methods!(TlsServer);
impl_tls_server_methods!(TlsServerShared);

impl TlsServerInner {
    fn do_handshake(&self) -> Response {
        let result = unsafe { ffi::SSL_do_handshake(self.ssl.as_ptr()) };
        if result == 1 {
            self.state.set(TlsHandleState::Started);
            return Response::Success(0);
        }

        let error = unsafe { ffi::SSL_get_error(self.ssl.as_ptr(), result) };
        match self.state.get() {
            TlsHandleState::HandshakeWantWrite => {
                self.state.set(TlsHandleState::Handshake);
                Response::WantWrite
            }
            TlsHandleState::Handshake => {
                self.state.set(TlsHandleState::HandshakeWantWrite);
                match error {
                    ffi::SSL_ERROR_WANT_WRITE => Response::WantWrite,
                    ffi::SSL_ERROR_WANT_READ => Response::WantRead,
                    _ => ssl_error_response(error),
                }
            }
            TlsHandleState::Started => Response::Fail(TlsServerError::Errno(Errno::INVAL)),
        }
    }

    fn get_ssl(&self) -> &SslRef {
        &self.ssl
    }

    fn shutdown(&self) -> Response {
        let ssl = self.ssl.as_ptr();
        let result = unsafe { ffi::SSL_shutdown(ssl) };
        if result == 0 {
            Response::WantWrite
        } else {
            handle_io_failure(ssl, result)
        }
    }

    fn read(&self, buffer: &mut [u8]) -> Response {
        if buffer.is_empty() {
            return Response::Success(0);
        }

        let length = match c_int_len(buffer.len()) {
            Ok(length) => length,
            Err(error) => return Response::Fail(error),
        };
        let ssl = self.ssl.as_ptr();
        let result = unsafe { ffi::SSL_read(ssl, buffer.as_mut_ptr().cast(), length) };
        ssl_io_response(ssl, result)
    }

    fn write(&self, buffer: &[u8]) -> Response {
        if buffer.is_empty() {
            return Response::Success(0);
        }

        let length = match c_int_len(buffer.len()) {
            Ok(length) => length,
            Err(error) => return Response::Fail(error),
        };
        let ssl = self.ssl.as_ptr();
        let result = unsafe { ffi::SSL_write(ssl, buffer.as_ptr().cast(), length) };
        ssl_io_response(ssl, result)
    }

    fn push_buffer_capacity(&self) -> Option<usize> {
        self.application_bio.available_write_len()
    }

    fn write_lease(&self) -> Option<ApplicationBioWriteLease<'_>> {
        self.application_bio.write_lease()
    }

    fn read_lease(&self) -> Option<ApplicationBioReadLease<'_>> {
        self.application_bio.read_lease()
    }

    fn push_bytes(&self, bytes: &[u8]) -> Option<usize> {
        self.application_bio.with_write_buffer(|buffer| {
            let amount = bytes.len().min(buffer.len());
            buffer[..amount].copy_from_slice(&bytes[..amount]);
            (amount, amount)
        })
    }
}

impl Clone for TlsServerShared {
    fn clone(&self) -> Self {
        TlsServerShared {
            inner: self.inner.clone(),
        }
    }
}

pub fn version() -> (u64, u64, u64) {
    let version = openssl_version::number() as u64;
    (
        (version >> 28) & 0xf,
        (version >> 20) & 0xff,
        (version >> 4) & 0xff,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::ssl::{Ssl, SslContext, SslMethod};
    use static_assertions::{assert_impl_all, assert_not_impl_any};

    assert_impl_all!(TlsServer: Send);
    assert_not_impl_any!(TlsServer: Sync, Clone);
    assert_not_impl_any!(TlsServerShared: Send, Sync);

    fn test_tls_server(is_server: bool) -> TlsServer {
        let ctx = SslContext::builder(SslMethod::tls()).unwrap().build();
        let ssl = Ssl::new(&ctx).unwrap();
        TlsServer::from_ssl(ssl, 1024, is_server).unwrap()
    }

    fn assert_success_amount(response: Response, expected: usize) {
        match response {
            Response::Success(amount) => assert_eq!(amount, expected),
            _ => panic!("expected success response"),
        }
    }

    #[test]
    fn zero_length_read_and_write_return_success_without_ssl_io() {
        let mut server = test_tls_server(false);
        let mut read_buffer = [];

        assert_success_amount(server.read(&mut read_buffer), 0);
        assert_success_amount(server.write(&[]), 0);
    }

    #[test]
    fn zero_ssl_io_result_uses_ssl_error_classification() {
        assert!(matches!(
            ssl_io_response_from_error(0, ffi::SSL_ERROR_WANT_READ),
            Response::WantRead
        ));
        assert!(matches!(
            ssl_io_response_from_error(0, ffi::SSL_ERROR_WANT_WRITE),
            Response::WantWrite
        ));
        assert!(matches!(
            ssl_io_response_from_error(0, ffi::SSL_ERROR_ZERO_RETURN),
            Response::Eof
        ));
    }

    #[test]
    fn positive_ssl_io_result_reports_progress() {
        assert_success_amount(ssl_io_response_from_error(17, ffi::SSL_ERROR_WANT_READ), 17);
    }

    #[test]
    fn c_int_length_guard_rejects_oversized_buffers() {
        assert!(matches!(
            c_int_len(c_int::MAX as usize + 1),
            Err(TlsServerError::Errno(Errno::INVAL))
        ));
    }

    #[test]
    fn unique_tls_server_can_move_between_threads() {
        let server = test_tls_server(false);
        std::thread::spawn(move || drop(server)).join().unwrap();
    }

    #[test]
    fn repeated_create_split_clone_and_drop_smoke_test() {
        for _ in 0..64 {
            let server = test_tls_server(false);
            let (read_half, write_half) = server.split();
            let clone = read_half.clone();
            drop(read_half);
            drop(write_half);
            drop(clone);
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "reentrant OpenSSL BIO write-buffer access")]
    fn debug_rejects_reentrant_bio_access() {
        let server = test_tls_server(false);
        let _guard = server
            .inner
            .application_bio
            .enter_callback(ApplicationBioDirection::Write);
        let _reentrant = server
            .inner
            .application_bio
            .enter_callback(ApplicationBioDirection::Write);
    }
}
