// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
use std::rc::Rc;

use futures::Future;
use rustix::thread::sched_getcpu;
use rustix_uring::Errno;

use crate::{
    Completion, CompletionState, OwnedFd, RuntimeHandle,
    configuration::{BusyPoll, Configuration},
    task::{FutureOrResult, Task, TaskReadyState, TaskState},
    task_ref::create_waker,
    task_state_cell::TaskStateCellRef,
    timer::Timer,
    tracing::Events,
};

fn poll_task(task: Rc<Task>, mut task_state: TaskStateCellRef<'_>) -> TaskStateCellRef<'_> {
    if task.get_state() == TaskReadyState::Complete {
        // A running task could get "woken" while running, and then complete without
        // suspending.  In that case we should not repoll the task as it has nothing
        // to do.
        return task_state;
    }

    let activity_id = task.activity_id.get();
    let tag = task.tag.get();
    let start_time = Timer::ticks();

    task_state.current_task = Some(task.clone());

    // We drop task_state here so that there is no mutable reference to it during
    // the Future::poll call. This will allow the poll implementation to call back
    // into functions in operations.rs that may need to borrow task_state. We will
    // reborrow immediately after the poll call.
    let task_state_ref = task_state.into_inner();

    let waker = create_waker(task.clone());
    let mut context = std::task::Context::from_waker(&waker);
    let complete = match task.poll(&mut context) {
        std::task::Poll::Pending => {
            // tasks that return Pending lose their high_priority bit
            task.high_priority.set(false);

            if task.get_state() == TaskReadyState::Running {
                task.set_state(TaskReadyState::Suspend);
            }

            false
        }
        std::task::Poll::Ready(()) => true,
    };

    let mut task_state = if complete {
        TaskState::complete_task(task_state_ref, &task)
    } else {
        task_state_ref.borrow_mut()
    };

    task_state.current_task = None;

    task_state.write_event(
        task.task_index,
        Events::TaskRunEnd {
            action: task.get_state() as u8,
            complete,
            tag,
            start_time,
            activity_id,
            cpu: sched_getcpu() as u16,
        },
    );

    task_state
}

struct RingEventTraceInfo {
    start_time: u64,
    ring_tag: u32,
    submissions: u64,
    in_flight_io: u64,
    want: bool,
    iopoll: bool,
}

fn process_completions(
    mut task_state: TaskStateCellRef<'_>,
    trace_info: RingEventTraceInfo,
) -> TaskStateCellRef<'_> {
    let RingEventTraceInfo {
        start_time,
        ring_tag,
        submissions,
        in_flight_io,
        want,
        iopoll,
    } = trace_info;
    let completions_hint = task_state.get_completion_count(iopoll) as u64;

    if want || submissions > 0 || completions_hint > 0 {
        // Only trace ring events if they will wait for I/O
        task_state.write_event(
            0,
            Events::RingEnterEnd {
                task_count: task_state.task_count as u32,
                tag: ring_tag,
                submissions,
                completions: completions_hint,
                in_flight_io,
                iopoll,
                want,
                start_time,
            },
        );
    }

    let mut completions = 0;
    while let Some(cqe) = task_state.get_next_cqe(iopoll) {
        completions += 1;
        // SAFETY: there was one Rc::into_raw when the SQE was successfully
        // submitted and there will be one resulting CQE where we reconstitute
        // the Rc. When a timeout is used, there is a compensating
        // Rc::increment_strong_count() so the second CQE used for the timeout
        // will hold a pointer to an Rc<> with a remaining ref count of 1,
        // which is then still safe here.
        let user_data_ptr = cqe.user_data().ptr();
        // if user_data is None, then this is a timeout completion, we need to skip it
        if user_data_ptr.is_null() {
            // This is a timeout completion, we need to skip it
            continue;
        }
        let pool_handle = unsafe { Rc::from_raw(user_data_ptr as *mut Completion) };

        let mut result: Result<u32, Errno> = cqe.result();

        let task_state_mut = &mut *task_state;
        let trace = &mut task_state_mut.trace_buffer;

        let waker = pool_handle.state.use_mut(|state| match state {
            CompletionState::Idle { .. } => {
                panic!("You can't get a completion for an entry that is not submitted")
            }
            CompletionState::Submitted {
                waker,
                canceled,
                activity_id,
                tag,
            } => {
                let waker = waker.clone();
                if result == Err(Errno::CANCELED) && !*canceled {
                    result = Err(Errno::TIME);
                }

                trace.write_event(
                    pool_handle.task_index,
                    Events::IoEnd {
                        result: match result {
                            Ok(_) => 0,
                            Err(e) => e.raw_os_error(),
                        },
                        tag: *tag,
                        activity_id: *activity_id,
                    },
                );

                *state = CompletionState::Completed {
                    result,
                    #[cfg(feature = "io_uring_cmd")]
                    big_cqe: *cqe.big_cqe(),
                };

                waker
            }
            CompletionState::Terminated | CompletionState::Completed { .. } => {
                panic!("You can't get a completion twice")
            }
        });

        task_state.return_completion(pool_handle);

        // Drop and re-borrow task_state so that wake_by_ref has the option
        // to access it via thread local storage.
        let task_state_ref = task_state.into_inner();
        waker.wake();
        task_state = task_state_ref.borrow_mut();
    }

    if iopoll {
        task_state.stats.decrement_in_flight_io_poll(completions);
    } else {
        task_state.stats.decrement_in_flight_io(completions);
    }
    task_state
}

pub(crate) fn submit_and_complete_io_all(
    mut task_state: TaskStateCellRef<'_>,
    busy_poll: bool,
) -> TaskStateCellRef<'_> {
    task_state = submit_and_complete_io(task_state, busy_poll, true);
    task_state = submit_and_complete_io(task_state, busy_poll, false);

    // If there are any completed tasks, we drop them with task_state_ref
    // released in case their drop implementations try to call TaskState::get().
    // At this point, no tasks are referenced from this call stack so any
    // references to the task that remain would be from a TaskHandle if any,
    // which can also safely be referenced since we do not use them except from
    // user code which is polled outside the TaskState lock.
    if !task_state.completed_tasks.is_empty() {
        // temporarily replace completed_tasks with an empty vec
        let mut completed_tasks = std::mem::take(&mut task_state.completed_tasks);
        // drop our reference to task_state
        let task_state_cell = task_state.into_inner();
        // drop the tasks
        completed_tasks.clear();
        // reborrow task state
        task_state = task_state_cell.borrow_mut();
        // put back the (now empty) completed_tasks vec so we can re-use the allocated capacity
        task_state.completed_tasks = completed_tasks;
    }

    task_state
}

/// Submit new IOs to the kernel and wait for completion events to occur if
/// there are no ready tasks to run in the meantime. This is called from the
/// heart of the event loop to pass incoming IOs to the kernel and extract
/// completed IOs from the kernel (after this call the completions are in the
/// completion queue ring buffer)
///
/// Returns the tag for tracing purposes.
pub(crate) fn submit_and_complete_io(
    mut task_state: TaskStateCellRef<'_>,
    busy_poll: bool,
    iopoll: bool,
) -> TaskStateCellRef<'_> {
    let tracing_info = {
        let any_ready = task_state.any_ready();
        let ring_tag = task_state.get_next_tag();
        let task_state = &mut *task_state;
        let stats = &task_state.stats;
        let in_flight_io_poll = stats.in_flight_io_poll.get();
        let in_flight_io = stats.in_flight_io.get();
        let enter_stats_wait = &mut task_state.enter_stats_wait;
        let enter_stats = &mut task_state.enter_stats;
        let ring = &mut task_state.ring;
        let ring_poll = &mut task_state.ring_poll;
        let ring = if iopoll { ring_poll } else { ring };

        if iopoll && in_flight_io_poll == 0 {
            // nothing to add, nothing in flight, just return
            None
        } else {
            let start_time = crate::timer::Timer::ticks();

            // If there are pending tasks to be executed, we do not want to block
            // in the kernel waiting IO completions. Instead, we want to pull new
            // completions out of the kernel and then process the ready tasks along
            // with the new completions and tasks woken up by those completions
            let (want, enter_stats) = if busy_poll || iopoll || any_ready || in_flight_io_poll > 0 {
                (0, enter_stats)
            } else {
                (1, enter_stats_wait)
            };

            enter_stats.start();
            let submissions = ring.submit_and_wait(want);
            enter_stats.stop();

            // do not double count the submissions
            let submissions = submissions as u64;
            let in_flight_io = if iopoll {
                in_flight_io_poll
            } else {
                in_flight_io
            } - submissions;

            Some(RingEventTraceInfo {
                start_time,
                submissions,
                want: want > 0,
                in_flight_io,
                ring_tag,
                iopoll,
            })
        }
    };

    if let Some(tracing_info) = tracing_info {
        task_state = process_completions(task_state, tracing_info);
    }

    task_state
}

pub struct Runtime {
    busy_poll: BusyPoll,
    server_pipe: OwnedFd,
    client_pipe: OwnedFd,
    // Runtime is bound to a single thread and must not be sent across threads.
    // io_uring submission/completion queues, TaskState, and virtual clocks are
    // all thread-local resources.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Runtime {
    pub fn new(thread_index: u8, configuration: Configuration) -> Self {
        let Configuration {
            trace_buffer_manager,
            busy_poll,
        } = configuration;
        let mut task_state = TaskState::get();
        task_state.trace_buffer.thread_idx = thread_index;
        task_state.trace_buffer.trace_configuration = trace_buffer_manager;
        let (server_pipe, client_pipe) = crate::pipe::bipipe();
        Self {
            busy_poll,
            server_pipe,
            client_pipe,
            _not_send: std::marker::PhantomData,
        }
    }

    pub fn get_handle(&self) -> RuntimeHandle {
        RuntimeHandle::new(self.client_pipe.try_clone().unwrap())
    }

    /// Runs the given future until completion.
    /// Returns None if shutdown_loop() is called.
    /// Returns Some(Err) if the future panics.
    /// Returns Some(Ok(Fut::Output)) if the future completes successfully.
    pub fn block_on<Fut>(
        &mut self,
        main: Fut,
    ) -> Option<Result<Fut::Output, Box<dyn std::any::Any + Send + 'static>>>
    where
        Fut: Future + 'static,
    {
        let mut task_state = TaskState::get();

        // Set a new activity_id for each uringruntime thread
        let activity_id = uuid::Uuid::new_v4();
        let tenant_id = uuid::Uuid::nil();

        let task = {
            let client_pipe = self.client_pipe.try_clone().unwrap();
            task_state.schedule_new(
                async move {
                    let runtime_handle = RuntimeHandle::new(client_pipe);
                    let _scope = scopeguard::guard((), |_| runtime_handle.close_sync());
                    main.await
                },
                activity_id,
                tenant_id,
            )
        };

        crate::runtime_handle::schedule_runtime_server(
            self.server_pipe.try_clone().unwrap(),
            &mut task_state,
            activity_id,
            tenant_id,
        );

        let (mut cool_down_time, poll_duration_ticks) = match self.busy_poll {
            BusyPoll::Never => (0, None),
            BusyPoll::Always => (0, None),
            BusyPoll::Until(duration) => {
                let poll_duration_ticks = duration.as_micros() as u64 * Timer::ticks_per_us();
                (
                    Timer::ticks() + poll_duration_ticks,
                    Some(poll_duration_ticks),
                )
            }
        };

        // Bail out once all tasks complete or if shutdown() is called which sets keep_running to false
        #[cfg(feature = "virtual-clock")]
        let mut idle_advance_active = true; // optimistic: try callback on first iteration
        while task_state.get_task_count() > 0 && task_state.keep_running {
            // do not busy poll if the cool down time has elapsed
            let busy_poll = match self.busy_poll {
                BusyPoll::Never => false,
                BusyPoll::Always => true,
                BusyPoll::Until(_) => Timer::ticks() <= cool_down_time,
            };

            // When virtual clock is active, don't block in submit_and_wait
            // if there are idle advance callbacks to try OR self-woken tasks in
            // the fill list (e.g., from yield-once cooperative scheduling).
            // Without this, submit_and_wait(1) blocks on io_uring forever
            // since self-wakeups don't produce io_uring CQEs.
            // idle_advance_active prevents spinning: cleared when the callback
            // returns None/ZERO, reset when tasks are polled (which may register
            // new timers, giving the callback something to advance to).
            #[cfg(feature = "virtual-clock")]
            let busy_poll = busy_poll
                || (task_state.clock.is_some()
                    && (task_state.any_ready()
                        || (idle_advance_active
                            && task_state
                                .clock
                                .as_ref()
                                .is_some_and(|c| c.has_idle_advance_fn()))));

            task_state = crate::runtime::submit_and_complete_io_all(task_state, busy_poll);
            task_state.prepare_cohort();

            #[cfg(feature = "virtual-clock")]
            let mut polled_any = false;
            while let Some(task) = task_state.get_ready() {
                task_state = poll_task(task, task_state);
                #[cfg(feature = "virtual-clock")]
                {
                    polled_any = true;
                }

                if let Some(poll_duration_ticks) = poll_duration_ticks {
                    // if we polled a task, then reset the cool down time.  Any I/O completion
                    // will wake a task, so this is an adequate proxy for I/O completion activity.
                    cool_down_time = Timer::ticks() + poll_duration_ticks;
                }
            }

            // If tasks were polled, they may have registered new timers —
            // the callback might have something to advance to now.
            #[cfg(feature = "virtual-clock")]
            if polled_any {
                idle_advance_active = true;
            }

            // When the runtime is idle (no tasks ready) and an idle advance
            // callback is installed, invoke it to determine how far to advance.
            // Re-entrancy: extract callback, release TaskState borrow, invoke,
            // re-borrow, restore callback, then advance and wake if needed.
            #[cfg(feature = "virtual-clock")]
            if !task_state.any_ready()
                && idle_advance_active
                && task_state
                    .clock
                    .as_ref()
                    .is_some_and(|c| c.has_idle_advance_fn())
            {
                let (mut cb, now, next_deadline) = {
                    let clock = task_state.clock.as_mut().unwrap();
                    let cb = clock.take_idle_advance_fn().unwrap();
                    let now = clock.now();
                    let next = clock.next_deadline();
                    (cb, now, next)
                };
                let cell = task_state.into_inner();
                let advance_duration = cb(now, next_deadline);
                task_state = cell.borrow_mut();

                // Restore callback unless user code replaced/cleared it
                // during invocation (dirty flag set by set_idle_advance_fn).
                // If replaced, re-arm idle_advance_active so the new callback
                // gets a chance to run on the next idle point.
                // Guard: the callback may have called virtual_clock_enable(false),
                // dropping the clock entirely. In that case, drop the old callback.
                let was_replaced = if let Some(clock) = task_state.clock.as_mut() {
                    clock.restore_idle_advance_fn(cb)
                } else {
                    drop(cb);
                    false
                };

                if let Some(dur) = advance_duration.filter(|d| !d.is_zero()) {
                    if let Some(clock) = task_state.clock.as_mut() {
                        let (_, wakers) = clock.advance(dur);
                        let cell = task_state.into_inner();
                        for w in wakers {
                            w.wake();
                        }
                        task_state = cell.borrow_mut();
                    }
                    idle_advance_active = true;
                    continue;
                }
                // Callback returned None or Duration::ZERO — don't spin.
                // But if the callback was replaced during invocation, the new
                // callback should get a chance: re-arm so it runs next iteration.
                idle_advance_active = was_replaced;
            }
        }

        task_state.enter_stats.report("enter_stats     ");
        task_state.enter_stats_wait.report("enter_stats_wait");

        let result = task.result();

        // Runtime shutdown teardown.
        //
        // iterate every Task in `task_state.tasks` (which transitively reaches
        // all Rc<Task>s — the ready queues and any waker-held Rc<Task>s all
        // reference tasks registered here) and replace each task's future with
        // a shutdown sentinel. Dropping the original future (via the
        // std::mem::replace) happens while the io_uring rings are still alive,
        // so any inner RingFuture::Drop can successfully cancel and drain its
        // pending I/O. After the swap every task holds a `ShutdownFuture` that
        // can never resume any I/O and whose `result()` reports a shutdown
        // panic if queried.

        let tasks_snapshot: Vec<Rc<Task>> =
            task_state.tasks.iter().map(|(_, t)| t.clone()).collect();
        drop(task_state);

        for other_task in &tasks_snapshot {
            // Set `current_task` to the task whose future is about to be
            // dropped. Diagnostics emitted from its RingFuture::Drop (e.g.
            // the `FutureCanceled` trace event) then attribute correctly
            // to this task's task_index / activity_id, and any future code
            // that assumes `current_task` is Some in a drop path continues
            // to work. A scopeguard ensures we always clear it afterwards,
            // even if dropping the old future panics.
            {
                let mut ts = TaskState::get();
                ts.current_task = Some(other_task.clone());
            }
            let _restore_current_task = scopeguard::guard((), |_| {
                TaskState::get().current_task = None;
            });

            // `std::mem::replace` returns the old future as the closure's
            // output; we drop it *after* `use_mut` releases its recursion
            // guard. RingFuture::Drop inside the old future only touches
            // TaskState (not Task::active_state) so there is no re-entrant
            // borrow of this cell.
            let old_future = other_task
                .active_state
                .use_mut(|state| std::mem::replace(state, ShutdownFuture::new_pinned()));
            drop(old_future);
        }
        drop(tasks_snapshot);

        // At this point no task in `task_state.tasks` has a future that can
        // still be holding pending borrowed-buffer I/O, so it is safe to
        // tear down the rest of TaskState (including the io_uring rings).
        // We must still be careful to release our task_state borrow before
        // dropping `old_state`, because arbitrary Drop code may re-enter
        // TaskState::get().
        let mut task_state = TaskState::get();
        let old_state = std::mem::take(&mut *task_state);
        drop(task_state);
        drop(old_state);

        result
    }
}

/// A no-op [`FutureOrResult`] used to replace a task's future during runtime
/// shutdown. Swapping in a `ShutdownFuture` causes the task's real future
/// (and any in-flight I/O inside it) to be dropped while the io_uring rings
/// are still alive, and guarantees that any surviving `TaskHandle::result()`
/// call produces a well-formed `Some(Err(_))` "panic" result rather than
/// reading through freed memory.
struct ShutdownFuture {
    // Pre-packaged panic value returned once from `result()` and `None`
    // thereafter, matching `TaskFuture::<F>::Result`'s one-shot contract.
    result: Option<Box<dyn std::any::Any + Send + 'static>>,
}

impl ShutdownFuture {
    fn new_pinned() -> std::pin::Pin<Box<dyn FutureOrResult>> {
        Box::pin(ShutdownFuture {
            result: Some(Box::new(
                "task aborted: runtime shutdown while I/O or wait was pending",
            )),
        })
    }
}

// ShutdownFuture has no self-referential state, so pinning is a no-op.
impl Unpin for ShutdownFuture {}

impl FutureOrResult for ShutdownFuture {
    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        // The runtime never polls tasks after the event loop has exited, and
        // this sentinel is only installed during that teardown. A poll here
        // would indicate a logic bug elsewhere.
        panic!("ShutdownFuture must not be polled");
    }

    unsafe fn result(
        self: std::pin::Pin<&mut Self>,
        _out_value: *mut (),
    ) -> Option<Result<(), Box<dyn std::any::Any + Send + 'static>>> {
        // We never write to `out_value`, so the caller's `F::Output` type
        // does not need to match anything in particular — the soundness of
        // `Task::result::<T>()` is preserved for every `T`.
        std::pin::Pin::into_inner(self).result.take().map(Err)
    }
}

#[cfg(test)]
mod test {
    use std::rc::Rc;

    use crate::{AsyncEvent, Errno, TaskHandleError, configuration::BusyPoll, operations};

    #[crate::test]
    async fn task_spawn_panic_test() {
        let task1 = operations::spawn_task(async {
            panic!("abort!");
        });
        let task2 = operations::spawn_task(async { Err(Errno::INVAL) as Result<i32, Errno> });
        let task3 = operations::spawn_task(async { Ok(1i32) as Result<i32, Errno> });

        let result1 = task1.await;
        let result2 = task2.await;
        let result3 = task3.await;

        match result1 {
            Err(TaskHandleError::Panic(payload)) => {
                assert_eq!(*payload.downcast::<&str>().unwrap(), "abort!")
            }
            _ => panic!("task1 should have panicked"),
        }

        assert_eq!(result2.unwrap().unwrap_err(), Errno::INVAL);
        assert_eq!(result3.unwrap().unwrap(), 1i32);
    }

    #[test]
    fn test_thread_spawn_panic() {
        fn test_spawn(panic: bool) -> std::thread::JoinHandle<Result<(), Errno>> {
            std::thread::spawn(move || {
                if panic {
                    panic!("abort!");
                } else {
                    Err(Errno::INVAL)
                }
            })
        }

        let handle1 = test_spawn(true);
        let handle2 = test_spawn(false);

        let result1 = handle1.join();
        let result2 = handle2.join();

        assert_eq!(*result1.unwrap_err().downcast::<&str>().unwrap(), "abort!");
        assert_eq!(result2.unwrap().unwrap_err(), Errno::INVAL);
    }

    #[crate::test]
    async fn test_shutdown_with_pending_borrowed_io() {
        // Regression test for the panic at `current_task.as_ref().unwrap()`
        // in RingFuture::Drop.
        //
        // The runtime exit path (block_on after the loop) takes ownership of
        // the TaskState, drops it, and only then drops the per-thread tasks.
        // During those drops `current_task` is None. If a Task being dropped
        // owns a still-Submitted RingFuture borrowing user resources (e.g. a
        // stack buffer), RingFuture::Drop reaches its
        // pending_io_with_borrowed_resources branch and previously unwrapped
        // a None current_task, panicking.
        //
        // Reproducing this requires two things at shutdown time:
        //   1. The task actually drops (not held alive by a Waker→Rc<Task>
        //      cycle from a Submitted Completion).
        //   2. The task owns a Submitted RingFuture with borrowed resources.
        //
        // We satisfy (1) by suspending with `yield_io()` (no Rc<Task> in its
        // wake path) and (2) by manually polling a `read` RingFuture with a
        // no-op waker so the Completion never captures the task's real waker.

        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Waker};

        let ready = Rc::new(AsyncEvent::new());
        let _task = {
            let ready = ready.clone();
            operations::spawn_task(async move {
                // Pipe with no writer — the read will never complete.
                let (read_fd, _write_fd) = crate::pipe::pipe().unwrap();
                let mut buf = [0u8; 16];

                // Drive the read RingFuture to Submitted state using a
                // throwaway no-op waker so the Completion does NOT hold an
                // Rc<Task>. This avoids the cycle that would otherwise leak
                // the task across shutdown.
                let mut read_fut = Box::pin(operations::read(&read_fd, &mut buf));
                let noop = Waker::noop();
                let mut cx = Context::from_waker(noop);
                let _ = Pin::as_mut(&mut read_fut).poll(&mut cx);

                ready.set();
                // Suspend cooperatively using yield_io; its wake path is
                // index-based and does not hold an Rc<Task>, so the task is
                // free to be dropped cleanly during runtime shutdown.
                loop {
                    operations::yield_io().await;
                }
            })
        };

        // Wait for the spawned task to submit its read I/O.
        ready.wait().await.unwrap();

        // Trigger runtime exit. The spawned task's pending RingFuture (with
        // borrowed buffer) will be dropped after current_task has been
        // cleared, exercising the Drop path that previously unwrapped.
        operations::shutdown_loop();
    }

    #[crate::test]
    async fn test_dangerous_task_drop() {
        let ready = Rc::new(AsyncEvent::new());
        let _task = {
            let ready = ready.clone();
            operations::spawn_task(async move {
                let _guard = scopeguard::guard((), |_| {
                    // when the task is dropped while the guard is active, this guard will
                    // be invoked which will recursively borrow_mut on task_state.
                    println!("Activity id: {}", operations::get_activity_id());
                });
                ready.set();
                loop {
                    operations::yield_io().await;
                }
            })
        };

        // wait for the task to get past the point where guard is active
        ready.wait().await.unwrap();

        // shut down and let loop clean up tasks
        operations::shutdown_loop();
    }

    #[test]
    fn test_busy_poll_always() {
        crate::run_test_with_handle(
            "test_busy_poll_always",
            async |_| {
                operations::sleep(std::time::Duration::from_millis(100))
                    .await
                    .unwrap();
                let wait_count = crate::task::TaskState::get().enter_stats_wait.count;
                assert_eq!(wait_count, 0);
            },
            false,
            BusyPoll::Always,
        );
    }

    #[test]
    fn test_busy_poll_never() {
        crate::run_test_with_handle(
            "test_busy_poll_never",
            async |_| {
                operations::sleep(std::time::Duration::from_millis(100))
                    .await
                    .unwrap();
                let wait_count = crate::task::TaskState::get().enter_stats_wait.count;
                assert_ne!(wait_count, 0);
            },
            false,
            BusyPoll::Never,
        );
    }

    /// Regression test for in_flight_io accounting drift.
    ///
    /// Each `read_with_timeout(_, _, Some(_))` submits a linked pair of SQEs
    /// (read + link_timeout -> 2 SQEs, 2 CQEs) and increments `in_flight_io`
    /// by 2. When the CQEs are drained in `process_completions`, the counter
    /// must be decremented by the actual number of CQEs consumed.
    #[crate::test]
    async fn test_in_flight_io_accounting_with_linked_timeouts() {
        use futures::future::join_all;
        use std::time::Duration;

        // Yield once so the runtime-server task starts its recv_message()
        // read, establishing the steady-state baseline for in_flight_io.
        operations::sleep(Duration::from_millis(1)).await.unwrap();
        let baseline = crate::task::TaskState::get().stats.in_flight_io.get();

        for _ in 0..200 {
            let mut writers = Vec::new();
            let futs: Vec<_> = (0..50)
                .map(|_| {
                    let (reader, writer) = crate::pipe::bipipe();
                    writers.push(writer);
                    async move {
                        let mut buf = [0u8; 1];
                        let _ = operations::read_with_timeout(
                            &reader,
                            &mut buf,
                            Some(Duration::from_millis(1)),
                        )
                        .await;
                    }
                })
                .collect();
            join_all(futs).await;
            drop(writers);
        }

        // One more event-loop iteration to drain any trailing CQEs.
        operations::sleep(Duration::from_millis(1)).await.unwrap();

        let after = crate::task::TaskState::get().stats.in_flight_io.get();
        assert_eq!(
            after, baseline,
            "in_flight_io drifted from {baseline} to {after} after linked-timeout operations; \
             CQE completion counting is broken"
        );
    }

    #[test]
    fn test_busy_poll_until() {
        crate::run_test_with_handle(
            "test_busy_poll_until",
            async |_| {
                operations::sleep(std::time::Duration::from_millis(1))
                    .await
                    .unwrap();

                // should not have waited during above sleep
                let wait_count = crate::task::TaskState::get().enter_stats_wait.count;
                assert_eq!(wait_count, 0);

                operations::sleep(std::time::Duration::from_millis(100))
                    .await
                    .unwrap();

                // should have waiting during this sleep
                let wait_count = crate::task::TaskState::get().enter_stats_wait.count;
                assert_ne!(wait_count, 0);
            },
            false,
            BusyPoll::Until(std::time::Duration::from_millis(10)),
        );
    }
}
