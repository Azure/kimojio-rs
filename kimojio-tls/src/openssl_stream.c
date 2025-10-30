// Copyright (c) Microsoft Corporation. All rights reserved.

#include <assert.h>
#include <stdbool.h>
#include <string.h>

#include <openssl/err.h>
#include <openssl/ssl.h>
#include <openssl/tls1.h>
#include <openssl/x509v3.h>

typedef enum
{
    ResponseType_Success = 0,
    ResponseType_SslStackFail = 1,
    ResponseType_SslFail = 2,
    ResponseType_ErrnoFail = 3,
    ResponseType_Eof = 4,
    ResponseType_WantWrite = 5,
    ResponseType_WantRead = 6,
} ResponseType;

typedef struct
{
    int32_t type; // ResponseType
    int32_t value;
} Response;

typedef enum
{
    TlsHandleState_HandshakeWantWrite,
    TlsHandleState_Handshake,
    TlsHandleState_Started,
} TlsHandleState;

typedef struct
{
    SSL *ssl;
    BIO *server;
    BIO *server_io;
    TlsHandleState state;
} TlsHandle;

static Response ssl_stack_error()
{
    return (Response){
        .type = ResponseType_SslStackFail,
        .value = 0,
    };
}

typedef struct
{
    char *buf;
    size_t size;
} Slice;

static Response negative_error(int ret_code)
{
    return (Response){
        .type = ResponseType_ErrnoFail,
        .value = -ret_code,
    };
}

static Response oom_error()
{
    return negative_error(-ENOMEM);
}

static Response success()
{
    Response error = {0};
    return error;
}

void tls_handle_close(TlsHandle *stream)
{
    if (stream)
    {
        if (stream->ssl)
            SSL_free(stream->ssl);
        free(stream);
    }
}

static void handle_cleanup(TlsHandle **ptr)
{
    TlsHandle *server = *ptr;
    tls_handle_close(server);
}

Response tls_handle_dup(TlsHandle *stream, TlsHandle** result) {
    if (stream == NULL || result == NULL || stream->ssl == NULL)
    {
        return negative_error(-EINVAL);
    }

    // This attribute is scoped cleanup to allow safe early return.
    //  will be called on server at any exit.
    __attribute__((cleanup(handle_cleanup))) TlsHandle *handle = malloc(sizeof(TlsHandle));
    if (!handle)
    {
        return oom_error();
    }
    memcpy(handle, stream, sizeof(TlsHandle));

    if (SSL_up_ref(handle->ssl) != 1) {
        return oom_error();
    }

    // Only for success exit, set server to NULL so scoped cleanup
    // will not free the resource.
    *result = handle;
    handle = NULL;
    return success();
}

void tls_handle_ctx_close(SSL_CTX *ctx)
{
    if (ctx)
        SSL_CTX_free(ctx);
}

void* tls_get_ssl(TlsHandle *tls)
{
    assert(tls);
    return (void *)tls->ssl;
}

static Response handle_io_failure(SSL *ssl, int result)
{
    if (result > 0)
    {
        return (Response){.type = ResponseType_Success, .value = result};
    }
    else if (result == 0)
    {
        return (Response){.type = ResponseType_Eof, .value = 0};
    }
    else
    {
        int error = SSL_get_error(ssl, result);
        switch (error)
        {
        case SSL_ERROR_WANT_WRITE:
            return (Response){.type = ResponseType_WantWrite, .value = 0};
        case SSL_ERROR_WANT_READ:
            return (Response){.type = ResponseType_WantRead, .value = 0};
        case SSL_ERROR_ZERO_RETURN:
            return (Response){.type = ResponseType_Eof, .value = 0};
        default:
            return (Response){.type = ResponseType_SslFail, .value = error};
        }
    }
}

Response tls_handle_create(SSL_CTX *ctx, size_t bufsize, bool is_server, TlsHandle **result)
{
    int res;

    // This attribute is scoped cleanup to allow safe early return.
    //  will be called on server at any exit.
    __attribute__((cleanup(handle_cleanup))) TlsHandle *handle = malloc(sizeof(TlsHandle));
    if (!handle)
    {
        return oom_error();
    }
    memset(handle, 0, sizeof(TlsHandle));

    handle->ssl = SSL_new(ctx);
    if (!handle->ssl)
    {
        return ssl_stack_error();
    }

    res = BIO_new_bio_pair(&handle->server, bufsize, &handle->server_io, bufsize);
    if (res != 1)
    {
        return ssl_stack_error();
    }

    if (is_server)
    {
        SSL_set_accept_state(handle->ssl);
    }
    else
    {
        SSL_set_connect_state(handle->ssl);
    }

    SSL_set_bio(handle->ssl, handle->server, handle->server);

    // Only for success exit, set server to NULL so scoped cleanup
    // will not free the resource.
    *result = handle;
    handle = NULL;
    return success();
}

int tls_handle_push_get_buffer(TlsHandle *server, Slice *buffer)
{
    char *buf = NULL;
    int size = BIO_nwrite0(server->server_io, &buf);
    buffer->buf = buf;
    buffer->size = size;
    return size;
}

int tls_handle_push_advance(TlsHandle *server, size_t amount)
{
    char *buf;
    return BIO_nwrite(server->server_io, &buf, amount);
}

int tls_handle_pull_get_buffer(TlsHandle *server, Slice *buffer)
{
    char *buf = NULL;
    int size = BIO_nread0(server->server_io, &buf);
    buffer->buf = buf;
    buffer->size = size;
    return size;
}

int tls_handle_pull_advance(TlsHandle *server, size_t amount)
{
    char *buf;
    return BIO_nread(server->server_io, &buf, amount);
}

Response tls_handle_read(TlsHandle *tls, void *data, size_t length)
{
    int result = SSL_read(tls->ssl, data, length);
    if (result < 0)
    {
        int error = SSL_get_error(tls->ssl, result);
        switch (error)
        {
        case SSL_ERROR_WANT_WRITE:
            return (Response){.type = ResponseType_WantWrite, .value = 0};
        case SSL_ERROR_WANT_READ:
            return (Response){.type = ResponseType_WantRead, .value = 0};
        case SSL_ERROR_ZERO_RETURN:
            return (Response){.type = ResponseType_Eof, .value = 0};
        default:
            return (Response){.type = ResponseType_SslFail, .value = error};
        }
    }
    return (Response){.type = result > 0 ? ResponseType_Success : ResponseType_Eof, .value = result};
}

Response tls_handle_write(TlsHandle *tls, const void *data, size_t length)
{
    int result = SSL_write(tls->ssl, data, length);
    if (result < 0)
    {
        int error = SSL_get_error(tls->ssl, result);
        switch (error)
        {
        case SSL_ERROR_WANT_WRITE:
            return (Response){.type = ResponseType_WantWrite, .value = 0};
        case SSL_ERROR_WANT_READ:
            return (Response){.type = ResponseType_WantRead, .value = 0};
        case SSL_ERROR_ZERO_RETURN:
            return (Response){.type = ResponseType_Eof, .value = 0};
        default:
            return (Response){.type = ResponseType_SslFail, .value = error};
        }
    }

    return (Response){.type = ResponseType_Success, .value = result};
}

Response tls_handle_server_side_handshake(TlsHandle *tls)
{
    if (!tls)
    {
        return ssl_stack_error();
    }
    int result = SSL_do_handshake(tls->ssl);
    if (result == 1)
    {
        /* a return of 1 from SSL_do_handshake is success, and other return codes
           indicate failure.
           See https://docs.openssl.org/master/man3/SSL_do_handshake/ for details
        */
        tls->state = TlsHandleState_Started;
        return (Response){.type = ResponseType_Success, .value = 0};
    }
    int error = SSL_get_error(tls->ssl, result);
    switch (tls->state)
    {
    case TlsHandleState_HandshakeWantWrite:
        tls->state = TlsHandleState_Handshake;
        return (Response){.type = ResponseType_WantWrite, .value = 0};
    case TlsHandleState_Handshake:
        tls->state = TlsHandleState_HandshakeWantWrite;
        switch (error)
        {
        case SSL_ERROR_WANT_WRITE:
            return (Response){.type = ResponseType_WantWrite, .value = 0};
        case SSL_ERROR_WANT_READ:
            return (Response){.type = ResponseType_WantRead, .value = 0};
        default:
            break;
        }
        break;
    case TlsHandleState_Started:
        return (Response){.type = ResponseType_ErrnoFail, .value = EINVAL};
    default:
        // invalid state
        assert(false);
        break;
    }
    return (Response){.type = ResponseType_SslFail, .value = error};
}

Response tls_handle_client_side_handshake(TlsHandle *tls)
{
    if (!tls)
    {
        return ssl_stack_error();
    }
    int result = SSL_do_handshake(tls->ssl);
    if (result == 1)
    {
        /* a return of 1 from SSL_do_handshake is success, and other return codes
           indicate failure.
           See https://docs.openssl.org/master/man3/SSL_do_handshake/ for details
        */
        tls->state = TlsHandleState_Started;
        return (Response){.type = ResponseType_Success, .value = 0};
    }

    int error = SSL_get_error(tls->ssl, result);
    switch (tls->state)
    {
    case TlsHandleState_HandshakeWantWrite:
        tls->state = TlsHandleState_Handshake;
        return (Response){.type = ResponseType_WantWrite, .value = 0};
    case TlsHandleState_Handshake:
        tls->state = TlsHandleState_HandshakeWantWrite;
        switch (error)
        {
        case SSL_ERROR_WANT_WRITE:
            return (Response){.type = ResponseType_WantWrite, .value = 0};
        case SSL_ERROR_WANT_READ:
            return (Response){.type = ResponseType_WantRead, .value = 0};
        default:
            break;
        }
        break;
    case TlsHandleState_Started:
        return (Response){.type = ResponseType_ErrnoFail, .value = EINVAL};
    default:
        // invalid state
        assert(false);
        break;
    }
    return (Response){.type = ResponseType_SslFail, .value = error};
}

Response tls_handle_shutdown(TlsHandle *stream)
{
    int result = SSL_shutdown(stream->ssl);
    if (result == 0)
    {
        return (Response){.type = ResponseType_WantWrite, .value = 0};
    }
    return handle_io_failure(stream->ssl, result);
}

/* Test function to verify minimum TLS version is set correctly */
int tls_get_min_proto_version(SSL_CTX *ctx)
{
    return SSL_CTX_get_min_proto_version(ctx);
}
