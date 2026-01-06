// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Allow the crate to refer to itself as `::kimojio` for macro expansion
extern crate self as kimojio;

use std::rc::Rc;
use std::task::Waker;

use futures::Future;
pub use libc::{
    EAGAIN, EALREADY, EBADFD, EBUSY, EEXIST, EINTR, EINVAL, EIO, ENOENT, ENOTCONN, ENOTDIR,
    ENOTRECOVERABLE, ENOTSUP, EPERM, EPIPE, EPROTO, ESHUTDOWN, ETIMEDOUT,
};
pub use libc::{S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, c_int};
pub use libc::{SIGINT, SIGTERM};

mod async_channel;
mod async_event;
mod async_lock;
mod async_oneshot;
mod async_reader_writer_lock;
mod async_semaphore;
mod async_stream;
mod buffer_pipe;
mod cancellation_token;
pub mod configuration;
mod errors;
mod handle_table;
pub mod io_type;
mod message_pipe;
mod mut_in_place_cell;
pub mod operations;
pub mod pipe;
mod pointer_buffer;
mod prefix_buffer;
pub(crate) mod ring_future;
mod runtime;
mod runtime_handle;
pub mod socket_helpers;
#[cfg(feature = "ssl2")]
pub mod ssl2;
pub mod task;
pub mod task_pool;
mod task_ref;
mod task_state_cell;
pub mod timer;
#[cfg(feature = "tls")]
pub mod tlscontext;
#[cfg(feature = "tls")]
pub mod tlsstream;
mod tracing;
mod uring_stats;

pub use async_channel::{
    Receiver, ReceiverUnbounded, Sender, SenderUnbounded, async_channel, async_channel_unbounded,
    async_channel_unbounded_with_capacity,
};
pub use async_event::AsyncEvent;
pub use async_lock::{AsyncLock, AsyncLockRef};
pub use async_oneshot::{ReceiverOneshot, SenderOneshot, oneshot};
pub use async_reader_writer_lock::AsyncReaderWriterLock;
pub use async_semaphore::AsyncSemaphore;
pub use async_stream::{
    AsyncStreamRead, AsyncStreamWrite, OwnedFdStream, OwnedFdStreamRead, OwnedFdStreamWrite,
    SplittableStream,
};
pub use buffer_pipe::{BufferPipe, BufferReadStream, BufferStream, BufferWriteStream};
pub use cancellation_token::CancellationToken;
use configuration::Configuration;
pub use errors::{
    CanceledError, ChannelError, TaskHandleError, TimeoutError, errno_from_task_handle_result,
};
pub use handle_table::{HandleTable, Index};
pub use message_pipe::{
    MessagePipe, MessagePipeReceiver, MessagePipeSender, make_message_pipe,
    make_message_pipe_oneway, make_message_pipe_oneway_sync,
};
pub use mut_in_place_cell::MutInPlaceCell;
use operations::kernel_version;
use pointer_buffer::{pointer_from_buffer, pointer_to_buffer};
pub use prefix_buffer::{BufferView, OwnedBuffer, PrefixBuffer, StaticBuffer};
pub use runtime::Runtime;
pub use runtime_handle::{
    OpenRequest, OpenRequestHandlerImpl, RuntimeHandle, RuntimeServerRequestEnvelope,
    create_open_request,
};
pub use rustix::fd::OwnedFd;
use rustix::io_uring::io_uring_user_data;
pub use rustix_uring::Errno;
use rustix_uring::opcode::AsyncCancel;
use task::Task;
pub use tracing::{EventEnvelope, Events, TraceConfiguration};
use uuid::Uuid;

use crate::configuration::BusyPoll;

pub use kimojio_macros::{main, test};

const MAX_TASK_STACK_SIZE: usize = 65536;

enum CompletionState {
    /// The future is created but SQE is not yet submitted to the kernel
    Idle {
        #[cfg(feature = "io_uring_cmd")]
        entry: Option<rustix_uring::squeue::Entry128>,
        #[cfg(not(feature = "io_uring_cmd"))]
        entry: Option<rustix_uring::squeue::Entry>,
        timespec: bool,
    },
    /// The I/O is submitted to the kernel
    Submitted {
        waker: Waker,
        activity_id: Uuid,
        tag: u32,
        canceled: bool,
    },
    /// The I/O is completed
    Completed {
        result: Result<u32, Errno>,
        #[cfg(feature = "io_uring_cmd")]
        big_cqe: [u64; 2],
    },
    Terminated,
}

pub(crate) struct Completion {
    state: MutInPlaceCell<CompletionState>,
    owned_resources: CompletionResources,
    // memory for timeouts
    timespec: rustix_uring::types::Timespec,
    tag: u32,
    task_index: u16,
    iopoll: bool,
}

#[allow(dead_code)]
enum CompletionResources {
    None,
    Timespec(rustix_uring::types::Timespec),
    Box(Box<dyn std::any::Any>),
    Rc(Rc<dyn std::any::Any>),
    InlineBuffer([u8; 8]),
}

impl Completion {
    pub fn cancel(self: &Rc<Self>, task_state: &mut task::TaskState) {
        let should_cancel = self.state.use_mut(|state| match state {
            CompletionState::Idle { .. } => {
                // cancel immediately but skipping submission
                *state = CompletionState::Completed {
                    result: Err(Errno::CANCELED),
                    #[cfg(feature = "io_uring_cmd")]
                    big_cqe: [0; 2],
                };
                false
            }
            CompletionState::Submitted { canceled, .. } => {
                if !*canceled {
                    *canceled = true;
                    true
                } else {
                    false
                }
            }
            CompletionState::Terminated | CompletionState::Completed { .. } => false,
        });

        if should_cancel {
            let user_data_ptr = Rc::as_ptr(self) as *mut libc::c_void;
            let user_data = io_uring_user_data::from_ptr(user_data_ptr);
            #[allow(clippy::useless_conversion)]
            let entry = AsyncCancel::new(user_data).build().into();
            if self.iopoll {
                task_state.submit_poll(&[entry]);
                task_state.stats.increment_in_flight_io_poll(1);
            } else {
                task_state.submit(&[entry]);
                task_state.stats.increment_in_flight_io(1);
            }
        }
    }
}

/// Runs the given future until completion using the default configuration.
/// Returns None if shutdown_loop() is called.
/// Returns Some(Err) if the future panics.
/// Returns Some(Ok(Fut::Output)) if the future completes successfully.
pub fn run<Fut>(
    thread_index: u8,
    main: Fut,
) -> Option<Result<Fut::Output, Box<dyn std::any::Any + Send + 'static>>>
where
    Fut: Future + 'static,
{
    run_with_configuration(thread_index, main, Configuration::new())
}

// only used by tests
pub fn run_test<Fut>(test_name: &str, main: Fut)
where
    Fut: Future<Output = ()> + 'static,
{
    run_test_with_handle(test_name, move |_| main, false, BusyPoll::Never);
}

pub fn run_test_with_trace<Fut>(test_name: &str, main: Fut)
where
    Fut: Future<Output = ()> + 'static,
{
    run_test_with_handle(test_name, move |_| main, true, BusyPoll::Never);
}

// only used by tests
pub fn run_test_with_handle(
    _test_name: &str,
    main: impl AsyncFnOnce(RuntimeHandle) + 'static,
    trace: bool,
    busy_poll: BusyPoll,
) {
    let mut configuration = Configuration::new().set_busy_poll(busy_poll);
    if trace {
        configuration = configuration.set_trace_buffer_manager(Box::new(TestTraceConfiguration));
    }

    let thread_index = 0;
    let mut runtime = Runtime::new(thread_index, configuration);
    let handle = runtime.get_handle();
    let result = runtime.block_on(main(handle));

    if let Some(Err(payload)) = result {
        std::panic::resume_unwind(payload);
    }
}

#[cfg(feature = "tls")]
pub fn tls_version() -> (u64, u64, u64) {
    kimojio_tls::version()
}

// only used by tests
#[cfg(test)]
pub fn run_test_with_post_validate<Fut>(
    main: Fut,
    post_validate: impl FnOnce(&uring_stats::URingStats),
) where
    Fut: Future<Output = ()> + 'static,
{
    let configuration = Configuration::new();
    let result = run_with_configuration(
        0,
        async move {
            main.await;
            task::TaskState::get().stats.clone()
        },
        configuration,
    );
    match result {
        None => panic!("The test shut down without returning a result"),
        Some(Err(payload)) => std::panic::resume_unwind(payload),
        Some(Ok(stats)) => post_validate(&stats),
    }
}

/// Runs the given future until completion.
/// Returns None if shutdown_loop() is called.
/// Returns Some(Err) if the future panics.
/// Returns Some(Ok(Fut::Output)) if the future completes successfully.
pub fn run_with_configuration<Fut>(
    thread_index: u8,
    main: Fut,
    configuration: Configuration,
) -> Option<Result<Fut::Output, Box<dyn std::any::Any + Send + 'static>>>
where
    Fut: Future + 'static,
{
    let kernel_version = kernel_version();
    if kernel_version < (5, 15) {
        panic!(
            "The minimum supported kernel version is 5.15, but we have found {}.{}. Please upgrade your kernel. If you are using WSL, you may need to run 'WSL --update'.",
            kernel_version.0, kernel_version.1
        );
    }

    let mut runtime = Runtime::new(thread_index, configuration);
    runtime.block_on(main)
}

struct TestTraceConfiguration;

impl TraceConfiguration for TestTraceConfiguration {
    fn trace(&self, event: tracing::EventEnvelope) {
        let EventEnvelope {
            thread_index,
            task_index,
            event,
        } = event;
        match event {
            tracing::Events::TaskScheduled {
                activity_id,
                tenant_id,
                tag,
                io,
            } => println!(
                "task_scheduled: thread_id: {thread_index}, task_id: {task_index}, tag: {tag}, activity_id: {activity_id}, tenant_id: {tenant_id}, io: {io}"
            ),
            tracing::Events::TaskRunEnd {
                activity_id,
                start_time,
                tag,
                cpu,
                action,
                complete,
            } => println!(
                "task_run_end: thread_id: {thread_index}, task_id: {task_index}, action: {action}, complete: {complete}, tag: {tag}, start_time: {start_time}, activity_id: {activity_id}, cpu: {cpu}"
            ),
            tracing::Events::RingEnterEnd {
                submissions,
                in_flight_io,
                start_time,
                completions,
                tag,
                task_count,
                want,
                iopoll,
            } => println!(
                "ring_enter_end: thread_id: {thread_index}, task_id: {task_index}, task_count: {task_count}, want: {want}, submissions: {submissions}, in_flight_io: {in_flight_io}, completions: {completions}, tag: {tag}, iopoll: {iopoll}, start_time: {start_time}"
            ),
            tracing::Events::AsyncWaitStart { activity_id, tag } => println!(
                "async_wait_start: thread_id: {thread_index}, task_id: {task_index}, tag: {tag}, activity_id: {activity_id}"
            ),
            tracing::Events::AsyncWaitEnd { activity_id, tag } => println!(
                "async_wait_end: thread_id: {thread_index}, task_id: {task_index}, tag: {tag}, activity_id: {activity_id}"
            ),
            tracing::Events::IoStart {
                activity_id,
                fd,
                tag,
                io_type,
            } => println!(
                "io_start: thread_id: {thread_index}, task_id: {task_index}, io_type: {io_type}, fd: {fd}, tag: {tag}, activity_id: {activity_id}"
            ),
            tracing::Events::IoEnd {
                activity_id,
                result,
                tag,
            } => println!(
                "io_end: thread_id: {thread_index}, task_id: {task_index}, result: {result}, tag: {tag}, activity_id: {activity_id}"
            ),
            tracing::Events::IoError {
                activity_id,
                tag,
                error,
            } => println!(
                "io_error: thread_id: {thread_index}, task_id: {task_index}, tag: {tag}, error: {error}, activity_id: {activity_id}"
            ),
            #[cfg(feature = "tls")]
            tracing::Events::TlsError { activity_id, code } => println!(
                "tls_error: thread_id: {thread_index}, task_id: {task_index}, code: {code}, activity_id: {activity_id}"
            ),
            tracing::Events::FutureCanceled { activity_id } => println!(
                "future_canceled: thread_id: {thread_index}, task_id: {task_index}, activity_id: {activity_id}"
            ),
        }
    }
}

pub fn try_clone_owned_fd(fd: &OwnedFd) -> Result<OwnedFd, Errno> {
    fd.try_clone().map_err(|e| {
        e.raw_os_error()
            .map(Errno::from_raw_os_error)
            .unwrap_or(Errno::INVAL)
    })
}
