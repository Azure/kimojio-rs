// Copyright (c) Microsoft Corporation. All rights reserved.

use std::{any::Any, cell::Cell, pin::Pin, rc::Rc};

use crate::{
    Errno, OwnedFd, TaskHandleError, message_pipe::IdMessagePipe, pointer_buffer::ZERO_ID,
};
use futures::Future;
use uuid::Uuid;

use crate::{AsyncLock, MessagePipe, operations, pipe::bipipe, task::TaskState};

/// `RuntimeHandle` is a handle to an instance of the async runtime that lets
/// you spawn tasks on it from another thread. Use `open_channel`,
/// `open_channel_sync` or `uringtokio::UringRuntimeHandle::open_channel` to
/// create a channel to spawn tasks on the runtime.  You will pass a closure
/// that will receive values from the client thread but will be executed on the
/// uringruntime thread.  Call `invoke` on the resulting channel to schedule
/// work on uringruntime and get the result.
///
/// `invoke` and `invoke_sync` calls are serialized. If you need multiple calls
/// to be in parallel, create multiple channels.
///
/// `open_channel` is for use from uringruntime tasks. `open_channel_sync` can
/// be used from any thread but is a block and should not be used from any async
/// task.  Given a `RuntimeHandle`, `uringtokio::UringRuntimeHandle::new` can be
/// used to create a handle that can be used from tokio.
///
/// # Implementation
///
/// Each runtime creates a pipe pair. The runtime listens on the server end of the
/// pipe for open channel requests. Channel requests consist of the server side of
/// a pipe to listen on, and a closure from T to R. The runtime spawns a task that
/// listens on the pipe for pointers to T, calls the closure to get R, and writes
/// a pointer to R back to the pipe. On the client side, a ClientPipeHandle wraps
/// the client side of the pipe, which is used to write pointers to T and read
/// resulting pointers to R.
///
/// The client pipe handle serializes access to the pipe so that only one request
/// at a time can be outstanding at the same time.
#[derive(Clone)]
pub struct RuntimeHandle {
    client_pipe: MessagePipe<RuntimeServerRequestEnvelope, ()>,
}

impl RuntimeHandle {
    pub fn new(client_pipe: OwnedFd) -> Self {
        Self {
            client_pipe: MessagePipe::new(client_pipe),
        }
    }

    pub fn into_inner(self) -> OwnedFd {
        self.client_pipe.into_inner()
    }

    /// Open a channel to the runtime. The closure `f` can be invoked on the
    /// uringruntime by calling `invoke` on the resulting `RuntimeClientPipe`.
    /// The parameter to invoke will be passed to `f` and the result from `f`
    /// will be returned from invoke.
    ///
    /// This method should only be called from a task running on the uringruntime.
    pub async fn open_channel<F, T, R>(&self, f: F) -> Result<RuntimeClientPipe<T, R>, Errno>
    where
        F: AsyncFnMut(T) -> R + 'static + Send + Clone,
        T: Send + 'static,
        R: Send + 'static,
    {
        let (client, server) = bipipe();
        let request = create_open_request(server, f);
        self.client_pipe.send_message(request).await?;
        Ok(RuntimeClientPipe {
            pipe: AsyncLock::new(Some(IdMessagePipe::new(client))),
            closed: Cell::new(false),
        })
    }

    /// Like `open_channel` but can be called from any thread. This call blocks
    /// and should not be called from any async task.
    pub fn open_channel_sync<F, T, R>(&self, f: F) -> Result<RuntimeClientPipeSync<T, R>, Errno>
    where
        F: AsyncFnMut(T) -> R + 'static + Send + Clone,
        T: Send + 'static,
        R: Send + 'static,
    {
        let (client, server) = bipipe();
        let request = create_open_request(server, f);
        self.client_pipe.send_message_sync(request)?;
        Ok(RuntimeClientPipeSync {
            pipe: std::sync::Mutex::new(IdMessagePipe::new(client)),
        })
    }

    pub(crate) fn close_sync(&self) {
        let message = Box::new(RuntimeServerRequestEnvelope::Shutdown);
        let _ = self.client_pipe.send_message_sync(message);
    }
}

/// This pipe sends and receives messages from uringruntime server.
/// The protocol on the pipe needs to match the logic in `fn invoke_loop`.
pub struct RuntimeClientPipe<T: Send, R: Send> {
    pipe: AsyncLock<Option<ReqClientPipe<T, R>>>,
    closed: Cell<bool>,
}

impl<T: Send, R: Send> RuntimeClientPipe<T, R> {
    pub async fn invoke(&mut self, value: T) -> Result<R, Errno> {
        let pipe = self.pipe.lock().await?;
        let pipe = pipe.as_ref().expect("Do not call invoke after close");
        // We are not utilizing the id here. We could get rid the pipe lock using id tracking.
        pipe.send_message(ZERO_ID, Box::new(Some(value))).await?;
        let (id, result) = pipe.recv_message().await?;
        debug_assert_eq!(ZERO_ID, id);
        let result = match *result {
            Ok(result) => result,
            // propagate panic from the uringruntime here...
            Err(panic) => std::panic::resume_unwind(panic),
        };
        Ok(result)
    }

    pub async fn close(&mut self) -> Result<(), Errno> {
        let mut pipe = self.pipe.lock().await?;
        let pipe = pipe
            .take()
            .expect("You must close a runtime client pipe only once");
        pipe.send_message(ZERO_ID, Box::new(None)).await?;
        self.closed.set(true);
        Ok(())
    }
}

/// Similar to RuntimeClientPipe but with sync apis.
pub struct RuntimeClientPipeSync<T: Send, R: Send> {
    pipe: std::sync::Mutex<ReqClientPipe<T, R>>,
}

impl<T: Send, R: Send> RuntimeClientPipeSync<T, R> {
    pub fn invoke(&self, value: T) -> Result<R, Errno> {
        let pipe = self.pipe.lock().unwrap();
        pipe.send_message_sync(ZERO_ID, Box::new(Some(value)))?;
        let (id, result) = pipe.recv_message_sync()?;
        debug_assert_eq!(id, ZERO_ID);
        let result = match *result {
            Ok(result) => result,
            // propagate panic from the uringruntime here...
            Err(panic) => std::panic::resume_unwind(panic),
        };
        Ok(result)
    }
}

impl<T: Send, R: Send> Drop for RuntimeClientPipeSync<T, R> {
    fn drop(&mut self) {
        let pipe = &*self.pipe.lock().unwrap();
        let _ = pipe.send_message_sync(ZERO_ID, Box::new(None));
    }
}

pub enum RuntimeServerRequestEnvelope {
    Open(OpenRequest),
    Shutdown,
}

pub trait OpenRequestHandler: Send {
    fn handle(&mut self, fd: OwnedFd) -> Pin<Box<dyn Future<Output = ()> + '_>>;
}

pub struct OpenRequest {
    pub init: Box<dyn OpenRequestHandler>,
    pub fd: OwnedFd,
}

pub struct OpenRequestHandlerImpl<T, R, F>
where
    T: Send,
    R: Send,
    F: FnOnce(Rc<ReqSeverPipe<T, R>>) -> Pin<Box<dyn Future<Output = ()>>> + 'static + Send,
{
    pub f: Option<F>,
    pub _marker: std::marker::PhantomData<(T, R)>,
}

impl<T, R, F> OpenRequestHandler for OpenRequestHandlerImpl<T, R, F>
where
    T: Send,
    R: Send,
    F: FnOnce(Rc<ReqSeverPipe<T, R>>) -> Pin<Box<dyn Future<Output = ()>>> + 'static + Send,
{
    fn handle(&mut self, fd: OwnedFd) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        let pipe = Rc::new(IdMessagePipe::new(fd));
        let f = self.f.take().unwrap();
        Box::pin(async { f(pipe).await })
    }
}

/// Pipe for reading argument T from client, and write result R back.
/// Result box contains panic information of the execution of a task.
type ReqSeverPipe<T, R> = IdMessagePipe<Result<R, Box<dyn Any + Send>>, Option<T>>;
/// The reverse of ReqSeverPipe used by client.
type ReqClientPipe<T, R> = IdMessagePipe<Option<T>, Result<R, Box<dyn Any + Send>>>;

/// fd is the one end of the pipe that uringruntime read and wite messages.
/// f is the task that can be invoked multiple times on uringruntim.
pub fn create_open_request<F, T, R>(fd: OwnedFd, f: F) -> Box<RuntimeServerRequestEnvelope>
where
    F: AsyncFnMut(T) -> R + 'static + Send + Clone,
    T: Send + 'static,
    R: Send + 'static,
{
    let wrapper = |pipe: Rc<ReqSeverPipe<T, R>>| {
        let pinned_future: Pin<Box<dyn Future<Output = ()>>> = Box::pin(async move {
            if let Err(e) = invoke_loop(pipe, f).await
                && e.raw_os_error() != libc::EPIPE
            {
                panic!("Error reading from pipe: {e}");
            }
        });
        pinned_future
    };

    let init = Box::new(OpenRequestHandlerImpl {
        f: Some(wrapper),
        _marker: std::marker::PhantomData,
    });
    Box::new(RuntimeServerRequestEnvelope::Open(OpenRequest { init, fd }))
}

pub async fn invoke_loop<F, T, R>(pipe: Rc<ReqSeverPipe<T, R>>, f: F) -> Result<(), Errno>
where
    F: AsyncFnMut(T) -> R + 'static + Send + Clone,
    T: Send + 'static,
    R: Send + 'static,
{
    loop {
        // read the id from request and pass it back in the response.
        let (id, request) = pipe.recv_message().await?;
        match *request {
            Some(value) => {
                let mut f_cp = f.clone();
                let pipe_cp = pipe.clone();
                // Because each function may take a long time to execute,
                // we spawn them in the background to achieve concurrency.
                operations::spawn_task(async move {
                    let result = operations::spawn_task(async move { f_cp(value).await }).await;
                    let result = match result {
                        Ok(result) => Ok(result),
                        Err(TaskHandleError::Canceled) => panic!("canceled"),
                        Err(TaskHandleError::Panic(panic)) => Err(panic),
                    };
                    let result = Box::new(result);
                    if let Err(ec) = pipe_cp.send_message(id, result).await {
                        if ec.raw_os_error() != libc::EPIPE {
                            panic!("fail to write to pipe.")
                        } else {
                            // the channel is closed.
                        }
                    }
                });
            }
            None => break,
        }
    }
    Ok(())
}

pub(crate) fn schedule_runtime_server(
    server_pipe: OwnedFd,
    task_state: &mut TaskState,
    activity_id: Uuid,
    tenant_id: uuid::Uuid,
) {
    task_state.schedule_new(runtime_server(server_pipe), activity_id, tenant_id);
}

async fn runtime_server(server_pipe: OwnedFd) -> Result<(), Errno> {
    let server = MessagePipe::<(), RuntimeServerRequestEnvelope>::new(server_pipe);
    loop {
        match *server.recv_message().await? {
            RuntimeServerRequestEnvelope::Shutdown => return Ok(()),
            RuntimeServerRequestEnvelope::Open(mut request) => {
                operations::spawn_task(async move { request.init.handle(request.fd).await });
            }
        };
    }
}

#[cfg(test)]
mod test {
    use std::cell::Cell;

    use crate::{Runtime, configuration::Configuration, operations};

    #[test]
    fn invoke_test() {
        thread_local! {
            static VALUE: Cell<i32> = const { Cell::new(0) };
        }

        let mut runtime1 = Runtime::new(0, Configuration::new());
        let thread = {
            let handle = runtime1.get_handle();
            std::thread::spawn(move || {
                let mut runtime2 = Runtime::new(0, Configuration::new());
                let result = runtime2.block_on(async move {
                    let mut client = handle
                        .open_channel(async move |value: i32| {
                            // this runs on uring runtime
                            VALUE.with(|v| v.set(value));
                            value * 2
                        })
                        .await
                        .unwrap();
                    let result = client.invoke(42).await.unwrap();
                    assert_eq!(result, 84);
                });
                assert!(matches!(result, Some(Ok(()))));
            })
        };
        let result = runtime1.block_on(async move {
            loop {
                let value = VALUE.with(|v| v.get());
                if value != 0 {
                    return value;
                }
                operations::sleep(std::time::Duration::from_millis(100))
                    .await
                    .unwrap();
            }
        });
        assert!(matches!(result, Some(Ok(42))));
        thread.join().unwrap();
    }

    #[test]
    fn invoke_sync_test() {
        thread_local! {
            static VALUE: Cell<i32> = const { Cell::new(0) };
        }

        let mut runtime = Runtime::new(0, Configuration::new());
        let thread = {
            let handle = runtime.get_handle();
            let client = handle
                .open_channel_sync(async move |value: i32| {
                    // this runs on uring runtime
                    VALUE.with(|v| v.set(value));
                    value * 2
                })
                .unwrap();
            std::thread::spawn(move || {
                let result = client.invoke(42).unwrap();
                assert_eq!(result, 84);
            })
        };
        let result = runtime.block_on(async move {
            loop {
                let value = VALUE.with(|v| v.get());
                if value != 0 {
                    return value;
                }
                operations::sleep(std::time::Duration::from_millis(100))
                    .await
                    .unwrap();
            }
        });
        assert!(matches!(result, Some(Ok(42))));
        thread.join().unwrap();
    }
}
