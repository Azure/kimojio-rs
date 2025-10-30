// Copyright (c) Microsoft Corporation. All rights reserved.
//! RingFuture implements a Future that is completed
//! via I/O URing .
//!
//! It is generic on the return type via the `MakeResult` trait
//! and a closure that creates the `Entry` which corresponds to
//! the I/O URing SQE.
//!
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use rustix::fd::FromRawFd;
use rustix::io_uring::io_uring_user_data;
use rustix_uring::opcode;
use rustix_uring::squeue::Flags;
use rustix_uring::types::Timespec;

use crate::io_type::IOType;
use crate::operations::IsIoPoll;
use crate::runtime::submit_and_complete_io;
use crate::tracing::Events;
use crate::{CompletionResources, CompletionState, Errno, MutInPlaceCell};
pub use rustix::fd::OwnedFd;

use crate::Completion;
use crate::task::{TaskReadyState, TaskState};

#[cfg(feature = "io_uring_cmd")]
use rustix_uring::squeue::Entry128 as SQE;

#[cfg(not(feature = "io_uring_cmd"))]
use rustix_uring::squeue::Entry as SQE;

// Future representing operations which return either 0 or an error value.
// Underlying result type is Result<()> which will contain () or the error
// if the operation failed
pub type UnitFuture = RingFuture<(), ResultToUnit>;

// Future for operations which return zero or positive on success,
// and a negative value on error.
pub type UsizeFuture = RingFuture<usize, ResultToUsize>;

// Future for operations which return a zero or positive file descriptor
// on success and negative value on error.
pub type OwnedFdFuture = RingFuture<OwnedFd, ResultToOwnedFd>;

#[cfg(feature = "io_uring_cmd")]
pub type UringCmdFuture = RingFuture<[u64; 2], ResultToCqe>;

pub trait MakeResult<T: Unpin>: Unpin {
    fn make_success(value: u32, cqe: &[u64; 2]) -> T;
}

pub struct RingFuture<T: Unpin, C: MakeResult<T>> {
    handle: Option<Rc<Completion>>,
    _marker: std::marker::PhantomData<(T, C)>,
}

impl<T: Unpin, C: MakeResult<T>> RingFuture<T, C> {
    pub(crate) fn new<Entry: Into<SQE>>(
        entry: Entry,
        fd: i32,
        timeout: Option<Duration>,
        io_type: IOType,
    ) -> Self {
        Self::with_polled(
            entry.into(),
            fd,
            timeout,
            io_type,
            false,
            CompletionResources::None,
        )
    }

    pub(crate) fn with_polled<Entry: Into<SQE>>(
        entry: Entry,
        fd: i32,
        timeout: Option<Duration>,
        io_type: IOType,
        iopoll: bool,
        owned_resources: CompletionResources,
    ) -> Self {
        let entry = Some(entry.into());
        let mut task_state = TaskState::get();
        let task = task_state.current_task.as_ref().unwrap().clone();
        let tag = task_state.get_next_tag();

        let timespec = timeout.map(|timeout| {
            Timespec::new()
                .nsec(timeout.subsec_nanos())
                .sec(timeout.as_secs())
        });

        let handle = task_state.new_completion(Completion {
            state: MutInPlaceCell::new(CompletionState::Idle {
                entry,
                timespec: timespec.is_some(),
            }),
            owned_resources,
            timespec: timespec.unwrap_or_default(),
            tag,
            task_index: task.task_index,
            iopoll,
        });

        task.register_io(&handle);

        let activity_id = task.activity_id.get();
        task_state.write_event(
            task.task_index,
            Events::IoStart {
                io_type,
                tag,
                fd,
                activity_id,
            },
        );

        Self {
            handle: Some(handle),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn cancel(&self) {
        if let Some(handle) = self.handle.as_ref() {
            let mut task_state = TaskState::get();
            handle.cancel(&mut task_state)
        }
    }
}

impl<T: Unpin, C: MakeResult<T>> IsIoPoll for RingFuture<T, C> {
    fn is_io_poll(&self) -> bool {
        if let Some(completion) = self.handle.as_ref() {
            completion.iopoll
        } else {
            false
        }
    }
}

impl<T: Unpin, C: MakeResult<T>> Future for RingFuture<T, C> {
    type Output = Result<T, Errno>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut task_state = TaskState::get();

        #[cfg(feature = "fault_injection")]
        if let Some((count, fault)) = &mut task_state.fault {
            if *count > 0 {
                *count -= 1;
            } else {
                let fault = *fault;
                task_state.fault = None;
                return Poll::Ready(Err(fault));
            }
        }

        let task_state_ref = &mut *task_state;
        let task = &task_state_ref.current_task;
        let task = task.as_ref().unwrap();
        let stats = &task_state_ref.stats;
        let trace_buffer = &task_state_ref.trace_buffer;
        let ring = &mut task_state_ref.ring;
        let ring_poll = &mut task_state_ref.ring_poll;
        let handle_ref = &mut self.get_mut().handle;
        let completion = handle_ref
            .as_ref()
            .expect("It is illegal to poll a completed future");

        let tag = completion.tag;
        let result = completion.state.use_mut(|state| match state {
            CompletionState::Idle { entry, timespec } => {
                let activity_id = task.activity_id.get();
                let current_task_state = task.get_state();
                if current_task_state == TaskReadyState::Aborted {
                    // If we were aborted while suspended waiting for I/O, then
                    // this is a good time to detect that and panic.
                    panic!("Task aborted");
                }

                let iopoll = completion.iopoll;
                let (flags, entry_count) = if *timespec {
                    (Flags::IO_LINK, 2)
                } else {
                    (Flags::empty(), 1)
                };

                // this clone will be undone when the CQE is processed and Rc::from_raw is called
                let pool_handle_raw_ptr = Rc::into_raw(completion.clone()) as *mut std::ffi::c_void;
                let user_data = io_uring_user_data::from_ptr(pool_handle_raw_ptr);

                let entries = [
                    #[allow(clippy::useless_conversion)]
                    entry
                        .take()
                        .unwrap()
                        .user_data(user_data)
                        .flags(flags)
                        .into(),
                    // We provide a pointer to the timespec in the Completion,
                    // because this always live longer than the pending I/O.
                    #[allow(clippy::useless_conversion)]
                    opcode::LinkTimeout::new(&completion.timespec)
                        .build()
                        // This conversion is for Entry128 when nvme_passthrue is enabled.
                        .into(),
                ];

                if !iopoll {
                    ring.submit(&entries[0..entry_count]);
                    stats.increment_in_flight_io(entry_count as u64)
                } else {
                    ring_poll.submit(&entries[0..entry_count]);
                    stats.increment_in_flight_io_poll(entry_count as u64)
                };

                *state = CompletionState::Submitted {
                    waker: cx.waker().clone(),
                    activity_id,
                    tag,
                    canceled: false,
                };
                Poll::Pending
            }
            CompletionState::Submitted { waker, .. } => {
                let current_task_state = task.get_state();
                if current_task_state == TaskReadyState::Aborted {
                    // If we were aborted while suspended waiting for I/O, then
                    // this is a good time to detect that and panic.
                    panic!("Task aborted");
                }

                // Update the waker in case we were polled from a different task.
                waker.clone_from(cx.waker());

                // still waiting for a completion
                Poll::Pending
            }
            CompletionState::Completed {
                result,
                #[cfg(feature = "io_uring_cmd")]
                big_cqe,
            } => {
                let result = *result;

                #[cfg(not(feature = "io_uring_cmd"))]
                let big_cqe = &[0u64; 2];

                let result = match result {
                    Ok(value) => Ok(C::make_success(value, big_cqe)),
                    Err(code) => {
                        trace_buffer.write_event(
                            task.task_index,
                            Events::IoError {
                                tag,
                                error: code.raw_os_error(),
                                activity_id: task.activity_id.get(),
                            },
                        );
                        Err(code)
                    }
                };
                *state = CompletionState::Terminated;
                Poll::Ready(result)
            }
            CompletionState::Terminated => {
                panic!("RingFuture polled after already returning a result.")
            }
        });

        if matches!(result, Poll::Ready(_)) {
            let completion = handle_ref.take().unwrap();
            task_state.return_completion(completion);
        }

        result
    }
}

impl<T: Unpin, C: MakeResult<T>> Drop for RingFuture<T, C> {
    fn drop(&mut self) {
        if let Some(completion) = &self.handle {
            // We got a pending_pool_handle. That means we are being dropped and the I/O
            // has not completed yet. Since no-one will see the result of the I/O, cancel
            // it immediately.
            let mut task_state = TaskState::get();
            completion.cancel(&mut task_state);

            // If we had owned resources registered with the completion, then we can return
            // right away. The resources are guaranteed to live as long as the Rc<Completion>
            // which is always at least as long as until the I/O completes.

            // However, if CompletionResources is None, then this request might have borrowed
            // resources, and we need to block until the I/O is complete.  Otherwise the kernel
            // might try and read and write to the memory for this request and after this drop
            // there is no guarantee it is still valid. We need to wait for the I/O to complete
            // before we return from the drop call.

            // TODO: when AsyncDrop lands in stable, we should see if we can make use of that to
            // improve this code to allow other tasks to continue while waiting for the cancelation.
            fn pending_io_with_borrowed_resources(
                state: &MutInPlaceCell<CompletionState>,
                owned_resources: &CompletionResources,
            ) -> bool {
                state.use_mut(|state| {
                    match state {
                        // The I/O has been submitted to the kernel but is not yet complete, not safe
                        // unless we own the resources and thus control their lifetime
                        CompletionState::Submitted { .. } => {
                            matches!(
                                owned_resources,
                                CompletionResources::None
                            )
                        },
                        // in Idle, we didn't submit the I/O yet so we are safe
                        CompletionState::Idle { .. } |
                        // Completed and Terminated, the I/O is complete so we are safe
                        CompletionState::Completed { .. } |
                        CompletionState::Terminated => false,
                    }
                })
            }

            if pending_io_with_borrowed_resources(&completion.state, &completion.owned_resources) {
                let current_task = task_state.current_task.as_ref().unwrap();
                let task_id = current_task.task_index;
                task_state.write_event(
                    task_id,
                    Events::FutureCanceled {
                        activity_id: current_task.activity_id.get(),
                    },
                );

                while pending_io_with_borrowed_resources(
                    &completion.state,
                    &completion.owned_resources,
                ) {
                    let iopoll = completion.iopoll;
                    task_state = submit_and_complete_io(task_state, false, iopoll);
                }
            }
        }
    }
}

impl<T: Unpin, C: MakeResult<T>> futures::future::FusedFuture for RingFuture<T, C> {
    fn is_terminated(&self) -> bool {
        if let Some(completion) = self.handle.as_ref() {
            completion
                .state
                .use_mut(|state| matches!(state, CompletionState::Terminated))
        } else {
            true
        }
    }
}

pub struct ResultToUnit {}

impl MakeResult<()> for ResultToUnit {
    fn make_success(_value: u32, _cqe: &[u64; 2]) {}
}

pub struct ResultToUsize {}

impl MakeResult<usize> for ResultToUsize {
    fn make_success(value: u32, _cqe: &[u64; 2]) -> usize {
        value as usize
    }
}

pub struct ResultToOwnedFd {}

impl MakeResult<OwnedFd> for ResultToOwnedFd {
    fn make_success(value: u32, _cqe: &[u64; 2]) -> OwnedFd {
        // SAFETY: origination of actual file descriptor. For safe usage
        // it is required that FdFuture only be initialized with an entry
        // top that returns a file descriptor in its result.
        unsafe { OwnedFd::from_raw_fd(value as i32) }
    }
}

#[cfg(feature = "io_uring_cmd")]
pub struct ResultToCqe {}

#[cfg(feature = "io_uring_cmd")]
impl MakeResult<[u64; 2]> for ResultToCqe {
    fn make_success(_value: u32, cqe: &[u64; 2]) -> [u64; 2] {
        *cqe
    }
}

#[cfg(test)]
mod test {
    use crate::{AsyncEvent, Errno, OwnedFd, operations};
    use std::rc::Rc;

    #[test]
    fn select_test() {
        use futures::select;
        crate::run_test("select_test", async {
            let mut f1 = crate::operations::yield_io();
            let mut f2 = crate::operations::yield_io();
            let mut f3 = crate::operations::yield_io();
            let mut sum = 0;
            loop {
                sum += select! {
                    _ = f1 => 1,
                    _ = f2 => 2,
                    _ = f3 => 4,
                    complete => break,
                };
            }
            assert_eq!(7, sum);
        })
    }

    #[test]
    fn sleep_test() {
        crate::run_test("sleep_test", async {
            crate::operations::sleep(std::time::Duration::from_secs(0))
                .await
                .unwrap()
        })
    }

    struct TestFuture {
        fd: OwnedFd,
        buf: [u8; 1],
    }

    impl TestFuture {
        async fn read(&mut self) -> Result<usize, Errno> {
            operations::read(&self.fd, &mut self.buf).await
        }
    }

    #[test]
    fn complete_future_on_different_task_test() {
        use futures::{FutureExt, select};
        crate::run_test("complete_future_on_different_task_test", async {
            let (pipe1, pipe2) = crate::pipe::bipipe();

            let mut fut1 = Box::pin(
                async move {
                    let mut test = TestFuture {
                        fd: pipe1,
                        buf: [0; 1],
                    };
                    test.read().await
                }
                .fuse(),
            );

            let fut2 = Box::pin(crate::operations::nop());

            // this will poll fut1 but complete fut2
            let _ignored = select! {
                _a = fut1 => 1,
                _b = fut2.fuse() => 2,
            };

            // now transfer fut1 into a task to complete it for real.
            let mut task = {
                let ready = Rc::new(AsyncEvent::new());
                let ready_copy = ready.clone();
                let task = operations::spawn_task(async move {
                    ready.set();
                    let result = fut1.await.unwrap();
                    assert_eq!(result, 1, "expected to read 1 byte");
                });
                ready_copy.wait().await.unwrap();
                task
            };

            operations::write(&pipe2, b"1").await.unwrap();

            let joined = select! {
                _ = task => true,
                _ = operations::sleep(std::time::Duration::from_secs(5)).fuse() => false,
            };

            assert!(joined);
        })
    }

    #[test]
    fn futures_unordered_test() {
        crate::run_test("futures_unordered_test", async {
            use futures::stream::FuturesUnordered;
            use futures::stream::StreamExt;
            let mut futures = FuturesUnordered::new();
            futures.push(crate::operations::nop());
            futures.push(crate::operations::nop());
            StreamExt::next(&mut futures).await.unwrap().unwrap();
            StreamExt::next(&mut futures).await.unwrap().unwrap();
            assert!(StreamExt::next(&mut futures).await.is_none());
        })
    }

    #[test]
    fn futures_unordered_event_test() {
        crate::run_test("futures_unordered_event_test", async {
            use futures::stream::FuturesUnordered;
            use futures::stream::StreamExt;
            let event = Rc::new(AsyncEvent::new());
            let mut futures = FuturesUnordered::new();
            futures.push(event.wait());
            let task = {
                let event = event.clone();
                operations::spawn_task(async move {
                    event.set();
                })
            };
            StreamExt::next(&mut futures).await.unwrap().unwrap();
            assert!(StreamExt::next(&mut futures).await.is_none());
            task.await.unwrap();
        })
    }

    struct WakerFuture;
    impl std::future::Future for WakerFuture {
        type Output = ();

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            cx.waker().wake_by_ref();
            std::task::Poll::Ready(())
        }
    }

    #[test]
    fn schedule_completed_test() {
        crate::run_test("schedule_completed_test", async {
            // This will schedule this task without suspending.  We then
            // immediatley complete this task by returning resulting in this
            // task being schedule but in the Complete state.
            WakerFuture.await;
        })
    }
}
