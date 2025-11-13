// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
use std::ffi::{CStr, c_char, c_ulonglong, c_void};
use std::num::NonZeroU64;
use std::ptr::null_mut;

use rustix_uring::Errno;

#[repr(C)]
struct RawTlsServer {
    _data: [u8; 0],
    _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,
}

/// This is the same as SSL_CTX
#[repr(C)]
struct RawTlsServerContext {
    _data: [u8; 0],
    _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,
}

#[repr(C)]
struct RawError {
    error_type: i32,
    error_code: i32,
}

#[repr(C)]
struct Slice {
    buf: *mut u8,
    size: usize,
}

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
    TlsError(Vec<u64>),
}

impl std::error::Error for TlsServerError {}

impl std::fmt::Display for TlsServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsServerError::Errno(errno) => std::fmt::Display::fmt(errno, f),
            TlsServerError::TlsError(errors) => {
                for e in errors {
                    let e = *e;
                    let lib = unsafe { ERR_lib_error_string(e) };
                    let func = unsafe { ERR_func_error_string(e) };
                    let reason = unsafe { ERR_reason_error_string(e) };
                    let empty: [i8; 1] = [0; 1];
                    let lib = unsafe {
                        CStr::from_ptr(if lib.is_null() {
                            empty.as_ptr() as *const c_char
                        } else {
                            lib
                        })
                    };
                    let func = unsafe {
                        CStr::from_ptr(if func.is_null() {
                            empty.as_ptr() as *const c_char
                        } else {
                            func
                        })
                    };
                    let reason = unsafe {
                        CStr::from_ptr(if reason.is_null() {
                            empty.as_ptr() as *const c_char
                        } else {
                            reason
                        })
                    };

                    let message = std::fmt::format(format_args!(
                        "TlsError error:{e} lib:{lib:?} func:{func:?} reason:{reason:?}\n"
                    ));
                    f.write_str(&message)?;
                }
                Ok(())
            }
        }
    }
}

pub fn get_error_details(code: u64) -> (String, String, Option<String>) {
    let lib = unsafe { ERR_lib_error_string(code) };
    let func = unsafe { ERR_func_error_string(code) };
    let reason = unsafe { ERR_reason_error_string(code) };
    let empty: [i8; 1] = [0; 1];
    let lib = String::from(
        unsafe {
            CStr::from_ptr(if lib.is_null() {
                empty.as_ptr() as *const c_char
            } else {
                lib
            })
        }
        .to_string_lossy(),
    );
    let func = String::from(
        unsafe {
            CStr::from_ptr(if func.is_null() {
                empty.as_ptr() as *const c_char
            } else {
                func
            })
        }
        .to_string_lossy(),
    );
    let reason = if reason.is_null() {
        None
    } else {
        Some(String::from(
            unsafe { CStr::from_ptr(reason) }.to_string_lossy(),
        ))
    };
    (lib, func, reason)
}

unsafe extern "C" {
    // "fat" wrapper methods
    fn tls_handle_close(tls: *mut RawTlsServer);
    fn tls_handle_dup(tls: *mut RawTlsServer, server: &mut *mut RawTlsServer) -> RawError;
    fn tls_handle_ctx_close(tls_ctx: *mut RawTlsServerContext);

    fn tls_handle_create(
        server_ctx: *mut RawTlsServerContext,
        bufsize: usize,
        is_server: bool,
        server: &mut *mut RawTlsServer,
    ) -> RawError;

    // tls_handle_push_get_buffer should be called to get access to openssl buffer
    // and then the actual amounts written should be passed to tls_handle_push_advance.
    fn tls_handle_push_get_buffer(tls: *mut RawTlsServer, slice: &mut Slice) -> i32;
    fn tls_handle_push_advance(tls: *mut RawTlsServer, amount: usize) -> i32;

    // tls_handle_pull_get_buffer should be called to get access to openssl buffer
    // and then the actual amounts read should be passed to tls_handle_pull_advance.
    fn tls_handle_pull_get_buffer(tls: *mut RawTlsServer, slice: &mut Slice) -> i32;
    fn tls_handle_pull_advance(tls: *mut RawTlsServer, amount: usize) -> i32;
    fn tls_handle_read(tls: *mut RawTlsServer, buffer: *mut u8, length: isize) -> RawError;
    fn tls_handle_write(tls: *mut RawTlsServer, buffer: *const u8, length: isize) -> RawError;
    fn tls_handle_server_side_handshake(tls: *mut RawTlsServer) -> RawError;
    fn tls_handle_client_side_handshake(tls: *mut RawTlsServer) -> RawError;
    fn tls_handle_shutdown(tls: *mut RawTlsServer) -> RawError;

    // Extra openssl methods for getting information about errors
    fn ERR_get_error() -> c_ulonglong;
    fn ERR_lib_error_string(e: c_ulonglong) -> *const c_char;
    fn ERR_func_error_string(e: c_ulonglong) -> *const c_char;
    fn ERR_reason_error_string(e: c_ulonglong) -> *const c_char;

    fn OpenSSL_version_num() -> c_ulonglong;

    // This gets the reference of the SSL object.
    fn tls_get_ssl(tls: *mut RawTlsServer) -> *mut c_void;

    fn tls_get_min_proto_version(ctx: *mut RawTlsServerContext) -> i32;
}

pub struct TlsServerContext {
    ctx: *mut RawTlsServerContext,
}

// SAFETY: OpenSSL is safe to call from different
// threads as long as not the same time (Send is
// ok, but not Sync)
unsafe impl Send for TlsServerContext {}

pub struct TlsServer {
    server: *mut RawTlsServer,
}

// SAFETY: OpenSSL is safe to call from different
// threads as long as not the same time (Send is
// ok, but not Sync)
unsafe impl Send for TlsServer {}

pub enum Response {
    Success(usize),
    Fail(TlsServerError),
    Eof,
    WantRead,
    WantWrite,
}

fn get_response(error: RawError) -> Response {
    match error.error_type {
        0 => Response::Success(error.error_code as usize),
        1 => Response::Fail(TlsServerError::TlsError(get_ssl_error())),
        2 => match error.error_code {
            0 => Response::Fail(TlsServerError::Errno(Errno::INVAL)),
            // SSL_ERROR_SSL
            1 => Response::Fail(TlsServerError::TlsError(get_ssl_error())),
            // SSL_ERROR_WANT_READ
            2 => Response::WantRead,
            // SSL_ERROR_WANT_WRITE
            3 => Response::WantWrite,
            // SSL_ERROR_WANT_X509_LOOKUP
            4 => Response::Fail(TlsServerError::Errno(Errno::PROTO)),
            // SSL_ERROR_SYSCALL
            5 => Response::Fail(TlsServerError::TlsError(get_ssl_error())),
            // SSL_ERROR_ZERO_RETURN
            6 => Response::Eof,
            // SSL_ERROR_WANT_CONNECT
            7 => Response::Fail(TlsServerError::Errno(Errno::PROTO)),
            // SSL_ERROR_WANT_ACCEPT
            8 => Response::Fail(TlsServerError::Errno(Errno::PROTO)),
            // SSL_ERROR_WANT_ASYNC
            9 => Response::Fail(TlsServerError::Errno(Errno::PROTO)),
            // SSL_ERROR_WANT_ASYNC_JOB
            10 => Response::Fail(TlsServerError::Errno(Errno::PROTO)),
            // SSL_ERROR_WANT_HELLO_CB
            11 => Response::Fail(TlsServerError::Errno(Errno::PROTO)),
            // SSL_ERROR_WANT_RETRY_VERIFY
            12 => Response::Fail(TlsServerError::Errno(Errno::PROTO)),
            _ => Response::Fail(TlsServerError::Errno(Errno::INVAL)),
        },
        3 => Response::Fail(TlsServerError::Errno(Errno::from_raw_os_error(
            error.error_code,
        ))),
        4 => Response::Eof,
        5 => Response::WantWrite,
        6 => Response::WantRead,
        _ => panic!("unexpected error code"),
    }
}

fn get_error() -> Option<NonZeroU64> {
    let code = unsafe { ERR_get_error() };
    NonZeroU64::new(code)
}

fn get_ssl_error() -> Vec<u64> {
    let mut codes: Vec<u64> = Vec::with_capacity(4);
    while let Some(code) = get_error() {
        codes.push(code.into());
    }
    codes
}

impl TlsServerContext {
    pub fn server(&self, bufsize: usize) -> Result<TlsServer, TlsServerError> {
        let mut server: *mut RawTlsServer = null_mut();
        let result =
            get_response(unsafe { tls_handle_create(self.ctx, bufsize, true, &mut server) });
        match result {
            Response::Success(_) => (),
            Response::Fail(e) => return Err(e),
            _ => panic!("Unexpected response"),
        }
        Ok(TlsServer { server })
    }

    pub fn client(&self, bufsize: usize) -> Result<TlsServer, TlsServerError> {
        let mut client: *mut RawTlsServer = null_mut();
        let result =
            get_response(unsafe { tls_handle_create(self.ctx, bufsize, false, &mut client) });
        match result {
            Response::Success(_) => (),
            Response::Fail(e) => return Err(e),
            _ => panic!("Unexpected response"),
        }
        Ok(TlsServer { server: client })
    }

    /// Get the minimum TLS protocol version for this context
    pub fn get_min_proto_version(&self) -> i32 {
        unsafe { tls_get_min_proto_version(self.ctx) }
    }

    /// From the raw *mut SSL_CTX pointer.
    pub fn from_raw(ctx: *mut c_void) -> Self {
        assert!(!ctx.is_null(), "Context pointer must not be null");
        let ctx = ctx as *mut RawTlsServerContext;
        Self { ctx }
    }
}

impl Drop for TlsServerContext {
    fn drop(&mut self) {
        unsafe {
            tls_handle_ctx_close(self.ctx);
        }
    }
}

// Note: The input parameter authorized_server_name is optional.
impl TlsServer {
    pub fn client_side_handshake(&mut self) -> Response {
        get_response(unsafe { tls_handle_client_side_handshake(self.server) })
    }

    /// Represents the server side execution of a TLS handshake with a client.
    pub fn server_side_handshake(&mut self) -> Response {
        get_response(unsafe { tls_handle_server_side_handshake(self.server) })
    }

    /// Gets the reference to SSL object.
    pub fn get_ssl_raw(&self) -> *mut c_void {
        unsafe { tls_get_ssl(self.server) }
    }

    pub fn shutdown(&mut self) -> Response {
        get_response(unsafe { tls_handle_shutdown(self.server) })
    }

    pub fn read(&mut self, buffer: &mut [u8]) -> Response {
        get_response(unsafe {
            tls_handle_read(self.server, buffer.as_mut_ptr(), buffer.len() as isize)
        })
    }

    pub fn write(&mut self, buffer: &[u8]) -> Response {
        get_response(unsafe {
            tls_handle_write(self.server, buffer.as_ptr(), buffer.len() as isize)
        })
    }

    pub fn get_push_buffer(&mut self) -> Option<&mut [u8]> {
        let mut slice = Slice {
            buf: null_mut(),
            size: 0,
        };
        let result = unsafe { tls_handle_push_get_buffer(self.server, &mut slice) };
        if result > 0 {
            Some(unsafe { std::slice::from_raw_parts_mut(slice.buf, slice.size) })
        } else {
            None
        }
    }

    pub fn use_push_buffer(&mut self, amount: usize) {
        let result = unsafe { tls_handle_push_advance(self.server, amount) };
        assert!(result as usize == amount);
    }

    pub fn get_pull_buffer(&self) -> Option<&[u8]> {
        let mut slice = Slice {
            buf: null_mut(),
            size: 0,
        };
        let result = unsafe { tls_handle_pull_get_buffer(self.server, &mut slice) };
        if result > 0 {
            Some(unsafe { std::slice::from_raw_parts(slice.buf, slice.size) })
        } else {
            None
        }
    }

    pub fn use_pull_buffer(&mut self, amount: usize) {
        let result = unsafe { tls_handle_pull_advance(self.server, amount) };
        assert!(result as usize == amount);
    }
}

impl Clone for TlsServer {
    fn clone(&self) -> Self {
        let mut server: *mut RawTlsServer = null_mut();
        let result = get_response(unsafe { tls_handle_dup(self.server, &mut server) });
        match result {
            Response::Success(_) => (),
            Response::Fail(e) => panic!("dup failed {e:?}"),
            _ => panic!("Unexpected response"),
        }
        TlsServer { server }
    }
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        unsafe {
            tls_handle_close(self.server);
        }
    }
}

pub fn version() -> (u64, u64, u64) {
    let version = unsafe { OpenSSL_version_num() };
    (
        (version >> 28) & 0xf,
        (version >> 20) & 0xff,
        (version >> 4) & 0xff,
    )
}
