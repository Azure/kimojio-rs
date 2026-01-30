// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! AsyncEvent is used to suspend tasks until a condition occurs.
//!
//! AsyncEvent can be set or reset. It is possible to wait until it is set,
//! causing the waiting task to suspend. When setting an AsyncEvent, you can
//! optionally choose how many "waiters" to wake up using set_wake_one() or
//! set_wake_n() (set() will wake up all waiters)
//!
//! # Usage
//!
//! ```
//! use kimojio::AsyncEvent;
//! use std::rc::Rc;
//! async fn example() {
//!     let e = Rc::new(AsyncEvent::new());
//!     e.reset();
//!     e.set();
//!     e.wait().await;
//! }
//! ```

use crate::task::{Task, TaskReadyState, TaskState};
use crate::tracing::Events;
use crate::{CanceledError, MutInPlaceCell, TimeoutError, operations};
use futures::future::FusedFuture;
use intrusive_collections::{LinkedList, LinkedListLink, intrusive_adapter};
use rustix_uring::Errno;
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// An event that can be used to suspend tasks until a condition occurs.
///
/// `AsyncEvent` can be set or reset. Tasks can wait until it is set,
/// causing them to suspend. When setting an `AsyncEvent`, you can optionally
/// choose how many waiters to wake up using `set_wake_one()` or `set_wake_n()`
/// (plain `set()` will wake up all waiters).
#[derive(Debug, Default)]
pub struct AsyncEvent {
    state: Cell<bool>,
    waiting_tasks: WaitList,
}

// Ensure that AsyncEvent is always !Send and !Sync
static_assertions::const_assert!(impls::impls!(AsyncEvent: !Send & !Sync));

impl AsyncEvent {
    /// Creates a new `AsyncEvent` in the reset (unset) state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new `AsyncEvent` with the specified initial state.
    pub fn with_state(state: bool) -> Self {
        Self {
            state: Cell::new(state),
            waiting_tasks: WaitList::new(),
        }
    }

    /// set the state of the event to "set" and wake up all waiters (if any).
    pub fn set(&self) {
        self.state.set(true);
        self.waiting_tasks.wake_all();
    }

    /// Returns true if AsyncEvent is set, false if it is reset.
    pub fn is_set(&self) -> bool {
        self.state.get()
    }

    /// Set the state of the event to "set" and wake up one waiter (if any).
    ///
    /// # Warning
    ///
    /// If there is more than one waiter and the woken waiter does not reset the
    /// event then some waiters may remain waiting while the event remains in
    /// the set state. When in doubt, just use `set`.
    ///
    /// Also note that using set_wake_one does not guarantee that only one
    /// waiter will be woken. If a waiter is polled for any reason and it finds
    /// the event in the right state then it will proceed anyway.
    pub fn set_wake_one(&self) {
        self.set_wake_n(1)
    }

    /// Set the state of the event to "set" and wake up to at most `count`
    /// waiters (if any).
    ///
    /// # Warning
    ///
    /// If there are more than `count` waiters on one of the woken waiters does
    /// not reset the event then some waiters may remain waiting while the event
    /// remains in the set state. When in doubt, just use `set`.
    ///
    /// Also note that using set_wake_n does not guarantee that only `count`
    /// waiters will be woken. If a waiter is polled for any reason and it finds
    /// the event in the right state then it will proceed anyway.
    pub fn set_wake_n(&self, mut count: usize) {
        self.state.set(true);
        while count > 0 {
            if self.waiting_tasks.wake_one() {
                count -= 1;
            } else {
                break;
            }
        }
    }

    /// Set the state of the event to "reset". This will have no effect on
    /// waiters.
    pub fn reset(&self) {
        self.state.set(false);
        self.waiting_tasks.wake_all();
    }

    /// Wait for the event to be set. If it is already set, then this returns
    /// immediately. Otherwise it will wait until some other task sets the
    /// event.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe.
    pub fn wait(&self) -> WaitAsyncEventFuture<'_> {
        WaitAsyncEventFuture {
            wait: WaitFuture::new(AsyncEventSource {
                event: self,
                expected_state: true,
            }),
        }
    }

    /// Wait for the event to be set or return immediately if it is already set.
    ///
    /// If the optional deadline is specified and the event is not set by that
    /// time, then this will return a timeout error.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe.
    pub async fn wait_with_deadline(&self, deadline: Option<Instant>) -> Result<(), TimeoutError> {
        if let Some(deadline) = deadline {
            let wait_remaining = || match (
                self.state.get(),
                deadline.checked_duration_since(Instant::now()),
            ) {
                (true, _) => None,
                (false, Some(remaining)) => Some(remaining),
                (false, None) => None,
            };

            while let Some(remaining) = wait_remaining() {
                futures::select! {
                    result = self.wait() => if let Err(_canceled_error) = result {
                        return Err(TimeoutError::Canceled);
                    },
                    result = operations::sleep(remaining) => if result == Err(Errno::CANCELED) {
                        return Err(TimeoutError::Canceled);
                    }
                }
            }

            if self.state.get() {
                Ok(())
            } else {
                Err(TimeoutError::Timeout)
            }
        } else {
            Ok(self.wait().await?)
        }
    }

    /// Wait for the event to be set or return immediately if it is already set.
    ///
    /// If the optional timeout is specified and the event is not set after that
    /// amount of time, then this will return a timeout error.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe.
    pub async fn wait_with_timeout(&self, timeout: Option<Duration>) -> Result<(), TimeoutError> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        self.wait_with_deadline(deadline).await
    }

    /// Wait for the event to be reset. If it is already reset, then this returns
    /// immediately. Otherwise it will wait until some other task resets the
    /// event.
    pub fn wait_reset(&self) -> WaitAsyncEventFuture<'_> {
        WaitAsyncEventFuture {
            wait: WaitFuture::new(AsyncEventSource {
                event: self,
                expected_state: false,
            }),
        }
    }

    /// Returns true if any tasks are waiting on this event. This is can be used
    /// in advanced use cases for optimization.
    pub(crate) fn any_waiting(&self) -> bool {
        self.waiting_tasks.any_waiting()
    }
}

pin_project_lite::pin_project! {
    pub struct WaitAsyncEventFuture<'a> {
        #[pin]
        wait: WaitFuture<AsyncEventSource<'a>>,
    }
}

impl Future for WaitAsyncEventFuture<'_> {
    type Output = Result<(), CanceledError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().wait.poll(cx) {
            Poll::Ready(Ok(_source)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(_canceled_error)) => Poll::Ready(Err(CanceledError {})),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl FusedFuture for WaitAsyncEventFuture<'_> {
    fn is_terminated(&self) -> bool {
        self.wait.is_terminated()
    }
}

#[derive(Debug)]
pub struct WaitData {
    pub activity_id: Uuid,
    // when it is enqueued, there is a Waker
    pub waker: MutInPlaceCell<Option<Waker>>,
    pub task_id: u16,
    pub canceled: Cell<bool>,
    pub tag: u32,
    pub link: LinkedListLink,
}

intrusive_adapter!(pub WaitDataAdapter = Rc<WaitData>: WaitData { link: LinkedListLink });

impl WaitData {
    fn set_waker(&self, cx: &mut Context<'_>) {
        self.waker.use_mut(|waker| {
            *waker = Some(cx.waker().clone());
        })
    }
}

pub trait WaitSource: Unpin {
    fn is_complete(&self) -> bool;
    fn wake_all(&self);
    fn register(&self, wait_data: &Rc<WaitData>, task_id: u16, activity_id: Uuid, tag: u32);
    fn unregister(&self, wait_data: &Rc<WaitData>);
}

struct AsyncEventSource<'a> {
    event: &'a AsyncEvent,
    expected_state: bool,
}

impl WaitSource for AsyncEventSource<'_> {
    fn is_complete(&self) -> bool {
        self.event.state.get() == self.expected_state
    }

    fn wake_all(&self) {
        self.event.waiting_tasks.wake_all()
    }

    fn register(&self, wait_data: &Rc<WaitData>, task_id: u16, activity_id: Uuid, tag: u32) {
        self.event
            .waiting_tasks
            .register(wait_data, task_id, activity_id, tag)
    }

    fn unregister(&self, wait_data: &Rc<WaitData>) {
        self.event.waiting_tasks.unregister(wait_data);
    }
}

#[derive(Default)]
pub struct WaitList {
    wait_data: MutInPlaceCell<LinkedList<WaitDataAdapter>>,
}

impl WaitList {
    pub fn new() -> Self {
        Self {
            wait_data: MutInPlaceCell::new(LinkedList::new(WaitDataAdapter::new())),
        }
    }

    /// Returns true if any tasks are waiting on this event. This is can be used
    /// in advanced use cases for optimization.
    pub fn any_waiting(&self) -> bool {
        self.wait_data.use_mut(|wait_data| !wait_data.is_empty())
    }

    fn wake_one(&self) -> bool {
        if let Some(task) = self.wait_data.use_mut(|wait_data| wait_data.pop_front()) {
            {
                // Scope task_state to within this block because the waker.wake()
                // call will invoke the RawWakerVTable APIs that internally will
                // call TaskState::get() which is illegal. Instead, keep bound the
                // `TaskStateCellRef` lifetime to this block so it goes out of scope
                // before the waker.wake() call.
                let task_state = TaskState::get();
                task_state.write_event(
                    task.task_id,
                    Events::AsyncWaitEnd {
                        tag: task.tag,
                        activity_id: task.activity_id,
                    },
                );
            }

            task.waker.use_mut(|waker| {
                if let Some(waker) = waker {
                    waker.wake_by_ref()
                }
            });
            true
        } else {
            false
        }
    }

    pub fn wake_all(&self) {
        while self.wake_one() {}
    }

    pub fn register(&self, wait_data: &Rc<WaitData>, task_id: u16, activity_id: Uuid, tag: u32) {
        if !wait_data.link.is_linked() {
            self.wait_data.use_mut(|waiting_tasks| {
                waiting_tasks.push_back(wait_data.clone());
            });

            TaskState::get().write_event(task_id, Events::AsyncWaitStart { tag, activity_id });
        }
    }

    pub fn unregister(&self, wait_data: &Rc<WaitData>) {
        if wait_data.link.is_linked() {
            self.wait_data.use_mut(|waiting_tasks| {
                // SAFETY: the check for is_linked above ensures that
                // WaitData is still in the list, and waiting_tasks is
                // the only list that WaitData can be in.
                let mut cursor = unsafe { waiting_tasks.cursor_mut_from_ptr(wait_data.as_ref()) };
                cursor.remove();
            });
        }
    }
}

impl std::fmt::Debug for WaitList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaitList").finish()
    }
}

pub struct TaskSource {
    pub task: Rc<Task>,
}

impl TaskSource {
    pub fn new(task: Rc<Task>) -> Self {
        Self { task }
    }

    pub fn task(&self) -> Rc<Task> {
        self.task.clone()
    }
}

impl WaitSource for TaskSource {
    fn is_complete(&self) -> bool {
        self.task.get_state() == TaskReadyState::Complete
    }

    fn wake_all(&self) {
        self.task.joining_tasks.wake_all()
    }

    fn register(&self, wait_data: &Rc<WaitData>, task_id: u16, activity_id: Uuid, tag: u32) {
        self.task
            .joining_tasks
            .register(wait_data, task_id, activity_id, tag);
    }

    fn unregister(&self, wait_data: &Rc<WaitData>) {
        self.task.joining_tasks.unregister(wait_data);
    }
}

pub struct WaitFuture<Source: WaitSource> {
    source: Option<Source>,
    wait_data: Option<Rc<WaitData>>,
}

impl<Source: WaitSource> WaitFuture<Source> {
    pub fn new(source: Source) -> Self {
        Self {
            source: Some(source),
            wait_data: None,
        }
    }

    pub fn source(&self) -> Option<&Source> {
        self.source.as_ref()
    }
}

impl<Source: WaitSource> Future for WaitFuture<Source> {
    type Output = Result<Source, CanceledError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(wait_data) = &self.wait_data
            && wait_data.canceled.get()
        {
            wait_data.canceled.set(false);
            return Poll::Ready(Err(CanceledError {}));
        }

        // Note: do not keep TaskState around when possibly calling wake below.
        let current_task = TaskState::get().get_current_task();

        let current_task_state = current_task.get_state();
        if current_task_state == TaskReadyState::Aborted {
            // If we were aborted while suspended waiting, then this is a good
            // time to detect that and panic.

            // It is possible that the task is still in the waiting tasks list
            // because poll was called for a reason other than this task being
            // woken. To be on the safe side, we will wake all waiters.
            if let Some(source) = &self.source {
                source.wake_all();
            }

            panic!("Task aborted");
        }

        if let Some(source) = &self.source {
            if source.is_complete() {
                Poll::Ready(Ok(self.get_mut().source.take().unwrap()))
            } else {
                // Location for this future is pinned, can use source address as a tag
                let tag = source as *const Source as u32;
                if let Some(wait_data) = &self.wait_data {
                    // reset waker in case future moved to another task
                    wait_data.set_waker(cx);

                    let task_id = wait_data.task_id;
                    let activity_id = wait_data.activity_id;
                    source.register(wait_data, task_id, activity_id, tag);
                    current_task.register_wait(wait_data);
                } else {
                    let task_id = current_task.task_index;
                    let activity_id = current_task.activity_id.get();
                    let new_wait_data = Rc::new(WaitData {
                        activity_id,
                        waker: MutInPlaceCell::new(Some(cx.waker().clone())),
                        task_id,
                        canceled: Cell::new(false),
                        tag,
                        link: LinkedListLink::new(),
                    });
                    source.register(&new_wait_data, task_id, activity_id, tag);
                    current_task.register_wait(&new_wait_data);
                    self.get_mut().wait_data = Some(new_wait_data);
                };
                Poll::Pending
            }
        } else {
            panic!("It is not valid to poll a completed task.")
        }
    }
}

impl<Source: WaitSource> FusedFuture for WaitFuture<Source> {
    fn is_terminated(&self) -> bool {
        self.source.is_none()
    }
}

impl<Source: WaitSource> Drop for WaitFuture<Source> {
    fn drop(&mut self) {
        if let Some(wait_data) = &self.wait_data
            && let Some(source) = &self.source
        {
            source.unregister(wait_data);
        }
    }
}

#[cfg(test)]
mod test {
    use std::{
        cell::Cell,
        marker::PhantomData,
        pin::Pin,
        rc::Rc,
        task::{Context, Poll},
    };

    use futures::Future;

    use crate::{AsyncEvent, operations, run_test};

    #[test]
    fn event_test_set_reset() {
        run_test("event_test_set", async {
            let e = AsyncEvent::new();
            e.set();
            e.wait().await.unwrap();
            e.reset();
            e.wait_reset().await.unwrap();
        })
    }

    #[test]
    fn event_test_wait() {
        run_test("event_test_wait", async {
            let e1 = Rc::new(AsyncEvent::new());
            let e2 = Rc::new(AsyncEvent::new());
            let x = Rc::new(Cell::new(0usize));
            let task = {
                let e1 = e1.clone();
                let e2 = e2.clone();
                let x = x.clone();
                operations::spawn_task(async move {
                    x.set(1);
                    e1.set();
                    e2.wait().await.unwrap();
                    x.set(2);
                })
            };
            assert_eq!(0, x.get());
            e1.wait().await.unwrap();
            assert_eq!(1, x.get());
            e2.set();
            task.await.unwrap();
            assert_eq!(2, x.get());
        })
    }

    #[test]
    fn event_test_set_wake_n() {
        run_test("event_test_set_wake_n", async {
            let e1 = Rc::new(AsyncEvent::new());
            let x = Rc::new(Cell::new(0usize));
            let y = Rc::new(Cell::new(0usize));
            let ready = Rc::new(AsyncEvent::new());
            let tasks: Vec<_> = (0..10)
                .map(|_| {
                    let e1 = e1.clone();
                    let x = x.clone();
                    let y = y.clone();
                    let ready = ready.clone();
                    operations::spawn_task(async move {
                        y.set(y.get() + 1);
                        if y.get() == 10 {
                            ready.set();
                        }
                        e1.wait().await.unwrap();
                        x.set(x.get() + 1);
                    })
                })
                .collect();

            ready.wait().await.unwrap();
            assert_eq!(0, x.get());
            e1.set_wake_one();
            wait_for_n(&x, 1).await;
            e1.set();
            for task in tasks {
                task.await.unwrap();
            }
            wait_for_n(&x, 10).await;
        })
    }

    async fn wait_for_n(x: &Cell<usize>, n: usize) {
        for _ in 0..100 {
            if n == x.get() {
                return;
            }
            operations::yield_io().await;
        }
        panic!();
    }

    #[crate::test]
    async fn async_event_select() {
        let e1 = Rc::new(AsyncEvent::new());
        let e2 = Rc::new(AsyncEvent::new());

        let task = {
            let e1 = e1.clone();
            let e2 = e2.clone();
            operations::spawn_task(async move {
                let mut one = false;
                let mut two = false;
                let mut f1 = e1.wait();
                let mut f2 = e2.wait();
                while !one || !two {
                    futures::select! {
                        _ = f1 => {
                            assert!(!one);
                            one = true;
                        },
                        _ = f2 => {
                            assert!(!two);
                            two = true;
                        },
                    };
                }
            })
        };

        e1.set();
        e2.set();
        task.await.unwrap();
    }

    pin_project_lite::pin_project! {
        struct DoublePollFuture<F: Future> {
            #[pin]
            e: F,
            count: usize,
        }
    }

    impl<F: Future> Future for DoublePollFuture<F> {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.count < 2 {
                let this = self.project();
                match this.e.poll(cx) {
                    Poll::Ready(_) => {
                        // nasty.. poll it again
                        *this.count += 1;
                        if *this.count < 2 {
                            // returning pending but it is ready
                            cx.waker().wake_by_ref();
                            Poll::Pending
                        } else {
                            Poll::Ready(())
                        }
                    }
                    Poll::Pending => Poll::Pending,
                }
            } else {
                Poll::Ready(())
            }
        }
    }

    #[crate::test]
    async fn async_event_await_same_future_twice() {
        let e1 = AsyncEvent::default();
        let mut fut = e1.wait();
        futures::select! {
            _ = fut => panic!("should not complete"),
            _ = operations::nop() => (),
        }
        futures::select! {
            _ = fut => panic!("should not complete"),
            _ = operations::nop() => (),
        }
        e1.set();
        fut.await.unwrap();
    }

    #[crate::test]
    #[should_panic(expected = "It is not valid to poll a completed task.")]
    async fn async_event_await_awaited_event() {
        let e1 = AsyncEvent::default();
        e1.set();
        let double = DoublePollFuture {
            e: e1.wait(),
            count: 0,
        };
        double.await;
    }

    struct PollTwiceFuture<'a, F: Future + 'a> {
        e: F,
        _marker: PhantomData<&'a ()>,
    }

    impl<'a, F: Future + 'a> Future for PollTwiceFuture<'a, F> {
        type Output = F::Output;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut e = unsafe { self.map_unchecked_mut(|f| &mut f.e) };
            match e.as_mut().poll(cx) {
                Poll::Ready(x) => Poll::Ready(x),
                Poll::Pending => e.poll(cx),
            }
        }
    }

    #[crate::test]
    async fn async_event_await_poll_pending() {
        let e1 = Rc::new(AsyncEvent::default());
        let e2: Rc<AsyncEvent> = e1.clone();
        let t = operations::spawn_task(async move {
            let twice = PollTwiceFuture {
                e: e2.wait(),
                _marker: PhantomData,
            };
            twice.await.unwrap();
        });
        operations::yield_io().await;
        e1.set();
        t.await.unwrap();
    }

    #[crate::test]
    async fn async_event_await_rearm() {
        let e1 = Rc::new(AsyncEvent::default());
        let e2 = Rc::new(AsyncEvent::default());
        let e3 = Rc::new(AsyncEvent::default());
        let e4 = Rc::new(AsyncEvent::default());

        let e1_clone = e1.clone();
        let e2_clone = e2.clone();
        let e3_clone = e3.clone();
        let e4_clone = e4.clone();
        let f1 = &mut e1_clone.wait();
        let f2 = &mut e2_clone.wait();
        let f3 = &mut e3_clone.wait();
        let f4 = &mut e4_clone.wait();

        use futures::future::FusedFuture;
        async fn do_select<F: FusedFuture + Future + Unpin>(
            mut f1: &mut F,
            mut f2: &mut F,
            mut f3: &mut F,
            mut f4: &mut F,
        ) -> u32 {
            futures::select!(
                _ = f1 => 1,
                _ = f2 => 2,
                _ = f3 => 3,
                _ = f4 => 4,
            )
        }

        e4.set();
        assert_eq!(do_select(f1, f2, f3, f4).await, 4);
        e3.set();
        assert_eq!(do_select(f1, f2, f3, f4).await, 3);
        e2.set();
        assert_eq!(do_select(f1, f2, f3, f4).await, 2);
        e1.set();
        assert_eq!(do_select(f1, f2, f3, f4).await, 1);
    }

    #[crate::test]
    async fn async_event_await_drop() {
        let e = Rc::new(AsyncEvent::default());

        let e1 = e.clone();
        let e2 = e.clone();

        let mut f1 = e1.wait();
        let mut f2 = e2.wait();

        // force poll of f1
        futures::select!(
            _ = f1 => (),
            _ = f2 => (),
            _ = operations::nop() => ()
        );

        drop(f1);

        e.set();

        f2.await.unwrap();
    }
}
