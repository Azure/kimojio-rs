// Copyright (c) Microsoft Corporation. All rights reserved.
//! Task represents the per-task state. A Task is bound to a thread. Tasks can
//! suspend and be woken up by I/O. There can be many pending futures per task.
//!
use std::{
    cell::Cell,
    collections::VecDeque,
    mem::{MaybeUninit, swap},
    num::NonZeroUsize,
    panic::AssertUnwindSafe,
    pin::Pin,
    rc::Rc,
    thread::ThreadId,
};

#[cfg(feature = "io_uring_cmd")]
use rustix_uring::{cqueue::Entry32 as CQE, squeue::Entry128 as SQE};

#[cfg(not(feature = "io_uring_cmd"))]
use rustix_uring::{cqueue::Entry as CQE, squeue::Entry as SQE};

type IoUring = rustix_uring::IoUring<SQE, CQE>;

use crate::{
    Completion, EAGAIN, EBUSY, EINTR, HandleTable,
    async_event::{WaitData, WaitList},
    mut_in_place_cell::MutInPlaceCell,
    task_ref::wake_task,
    task_state_cell::{TaskStateCell, TaskStateCellRef},
    tracing::{EventEnvelope, Events},
    uring_stats::URingStats,
};

/// This is the maximum number of submission entries that can be
/// enqueued before we have to flush them to the kernel.
pub const IO_URING_SUBMISSION_ENTRIES: usize = 128;

/// The maximum number of completion pool entries that can be pooled for re-use.
pub const MAX_COMPLETION_POOL_ENTRIES: usize = 4096;

// `TaskReadyState` is used to represent the next task in the list for ready
// tasks, or the desired next action to do to the task for executing tasks.
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(u8)]
pub enum TaskReadyState {
    Unknown = 0,
    Ready = 1,
    Running = 2,
    Suspend = 3,
    Aborted = 4,
    Complete = 5,
}

/// `FutureOrResult` exists to have a single allocation and a single dyn for any kind
/// of future that can be wrapped in a task.
pub trait FutureOrResult {
    /// Poll the future. When the future is ready, this will return `Poll::Ready(())`,
    /// and the `result` call can be used to discover the result.
    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context) -> std::task::Poll<()>;

    /// Retrieve the result of the future. None is returned if the future has
    /// not been polled to completion, or if the result has already been retrieved.
    /// Some(()) indicates that the value has been written to the `value` pointer.
    /// Some(Err(e)) indicates the future panicked, and e is the panic value.
    ///
    ///  # Safety
    ///
    /// The caller must ensure that the pointer passed to out_value is of type T.
    unsafe fn result(
        self: Pin<&mut Self>,
        value: *mut (),
    ) -> Option<Result<(), Box<dyn std::any::Any + Send + 'static>>>;
}

pin_project_lite::pin_project! {
    /// `TaskFuture` is either a future that is being polled, or a result
    /// returned by that future, or a panic result from polling the future.
    #[project = TaskFutureProj]
    #[project_replace = TaskFutureProjReplace]
    pub enum TaskFuture<F> where F: Future {
        Future {
            #[pin]
            future: F,
        },
        Result {
            result: Option<Result<F::Output, Box<dyn std::any::Any + Send + 'static>>>,
        }
    }
}

impl<F: Future> FutureOrResult for TaskFuture<F> {
    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context) -> std::task::Poll<()> {
        let this = self.as_mut().project();
        match this {
            TaskFutureProj::Future { future } => {
                // SAFETY: if the future panics, we will drop it when we replace TaskFuture::Future
                // with TaskFuture::Result. That ensures the future will not get repolled. There is
                // still the possibility that panic unsafe code in the future has left state in
                // an invalid state that was shared with this future, for example with Rc.
                match std::panic::catch_unwind(AssertUnwindSafe(move || {
                    // the actual poll
                    future.poll(cx)
                })) {
                    Ok(std::task::Poll::Ready(result)) => {
                        self.project_replace(TaskFuture::Result {
                            result: Some(Ok(result)),
                        });
                        std::task::Poll::Ready(())
                    }
                    Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
                    Err(panic_value) => {
                        self.project_replace(TaskFuture::Result {
                            result: Some(Err(panic_value)),
                        });
                        std::task::Poll::Ready(())
                    }
                }
            }
            TaskFutureProj::Result { result: _ } => panic!("No future while in non-terminal state"),
        }
    }

    unsafe fn result(
        mut self: Pin<&mut Self>,
        out_value: *mut (),
    ) -> Option<Result<(), Box<dyn std::any::Any + Send + 'static>>> {
        let this = self.as_mut().project();
        match this {
            TaskFutureProj::Future { future: _ } => None,
            TaskFutureProj::Result { result } => match result.take() {
                Some(Ok(value)) => {
                    let out_value = out_value as *mut F::Output;
                    unsafe {
                        out_value.write(value);
                    }
                    Some(Ok(()))
                }
                Some(Err(e)) => Some(Err(e)),
                None => None,
            },
        }
    }
}

pub struct Task {
    // The activity_id associated with this task, for tracing.
    pub activity_id: Cell<uuid::Uuid>,

    // The tenant_id associated with this task, for tracing.
    pub tenant_id: Cell<uuid::Uuid>,

    pub(crate) active_state: MutInPlaceCell<Pin<Box<dyn FutureOrResult>>>,

    // The identifier of this task.
    pub task_index: u16,

    // If a task is high priority and wakes up another task, that task will
    // inherit the high priority and will be scheduled immediately, without
    // necessarily wait for I/O to be submitted.
    pub high_priority: Cell<bool>,

    // The current state of this task..
    // Ready    => in a ready list (needs to be polled)
    // Running  => poll method is currently executing
    // Suspend  => not in a ready list and not running (e.g. waiting for I/O)
    // Complete => poll method returned completion, should no longer be polled
    ready: Cell<TaskReadyState>,

    pub tag: Cell<u32>,

    // A list of tasks that are suspended and would like to
    // be made ready when this task goes to Complete ready state.
    pub joining_tasks: WaitList,

    io_scope_completions: MutInPlaceCell<Option<IoScopeCompletions>>,

    // The task state from which this task originates
    pub task_state: *const TaskState,
}

#[derive(Default)]
pub(crate) struct IoScopeCompletions {
    pub completions: Vec<Rc<Completion>>,
    pub waits: Vec<Rc<WaitData>>,
}

impl Task {
    pub(crate) fn replace_io_scope_completions(
        &self,
        mut new_completions: Option<IoScopeCompletions>,
    ) -> Option<IoScopeCompletions> {
        self.io_scope_completions.use_mut(|io_scope_completions| {
            std::mem::swap(io_scope_completions, &mut new_completions)
        });
        new_completions
    }

    pub(crate) fn register_io(&self, completion: &Rc<Completion>) {
        self.io_scope_completions.use_mut(|io_scope_completions| {
            if let Some(io_scope_completions) = io_scope_completions {
                io_scope_completions.completions.push(completion.clone());
            }
        })
    }

    pub(crate) fn cancel_io_scope_completions(&self, mut task_state: TaskStateCellRef<'_>) {
        self.io_scope_completions.use_mut(|io_scope_completions| {
            let io_scope_completions = io_scope_completions
                .as_mut()
                .expect("must be called from an io_scope callback");
            for completion in io_scope_completions.completions.drain(..) {
                completion.cancel(&mut task_state);
                task_state.return_completion(completion);
            }
            // flush the added SQE
            task_state = crate::runtime::submit_and_complete_io_all(task_state, true);

            for wait in io_scope_completions.waits.drain(..) {
                wait.canceled.set(true);
                wait.waker.use_mut(|waker| {
                    if let Some(waker) = waker {
                        wake_task(&mut task_state, waker)
                    }
                });
            }
        })
    }

    pub(crate) fn register_wait(&self, wait: &Rc<WaitData>) {
        self.io_scope_completions.use_mut(|io_scope_completions| {
            if let Some(io_scope_completions) = io_scope_completions {
                io_scope_completions.waits.push(wait.clone());
            }
        })
    }

    fn new<F>(
        future: F,
        task_index: u16,
        task_state: *const TaskState,
        activity_id: uuid::Uuid,
        tenant_id: uuid::Uuid,
    ) -> Rc<Self>
    where
        F: Future + 'static,
    {
        let future = Box::pin(TaskFuture::Future { future });
        Rc::new(Task {
            activity_id: Cell::new(activity_id),
            tenant_id: Cell::new(tenant_id),
            active_state: MutInPlaceCell::new(future),
            ready: Cell::new(TaskReadyState::Ready),
            joining_tasks: WaitList::new(),
            task_index,
            high_priority: Cell::new(false),
            // start at 1 as zero is reserved for RingEnter start/end
            tag: Cell::new(1),
            io_scope_completions: MutInPlaceCell::new(None),
            task_state,
        })
    }

    pub(crate) fn poll(&self, cx: &mut std::task::Context) -> std::task::Poll<()> {
        self.active_state.use_mut(|state| state.as_mut().poll(cx))
    }

    pub(crate) fn result<T>(&self) -> Option<Result<T, Box<dyn std::any::Any + Send + 'static>>> {
        self.active_state.use_mut(|state| {
            let mut result: MaybeUninit<T> = MaybeUninit::uninit();
            // SAFETY: by construction, result is a pointer to T, and active_state is a TaskFuture<T>
            match unsafe { state.as_mut().result(result.as_mut_ptr() as *mut ()) } {
                Some(Ok(())) => {
                    // SAFETY: when Ok(()) is returned, result initializes value_out
                    let result = unsafe { result.assume_init() };
                    Some(Ok(result))
                }
                Some(Err(e)) => Some(Err(e)),
                None => None,
            }
        })
    }

    #[inline(always)]
    pub(crate) fn get_state(&self) -> TaskReadyState {
        self.ready.get()
    }

    #[inline(always)]
    pub(crate) fn set_state(&self, state: TaskReadyState) {
        self.ready.set(state);
    }
}

#[derive(Default)]
pub(crate) struct TraceBuffer {
    // A trace buffer for this thread
    pub trace_configuration: Option<Box<dyn crate::TraceConfiguration>>,
    // A thread id that is a zero based index
    pub thread_idx: u8,
}

impl TraceBuffer {
    #[inline(always)]
    pub fn write_event(&self, task_index: u16, event: Events) {
        if let Some(buffer) = &self.trace_configuration {
            buffer.trace(EventEnvelope {
                thread_index: self.thread_idx,
                task_index,
                event,
            });
        }
    }
}

pub struct Ring(IoUring);

impl Ring {
    pub fn new(iopoll: bool) -> Self {
        let mut ring_builder = rustix_uring::IoUring::builder();
        if cfg!(feature = "setup_single_issuer") {
            // setup_single_issuer() accepts &mut self, no need to rebind
            // ring_builder to the return value from setup_single_issuer()
            ring_builder.setup_single_issuer();
        }
        if iopoll {
            ring_builder.setup_iopoll();
        }
        let entries = IO_URING_SUBMISSION_ENTRIES as u32;
        Self(ring_builder.build(entries).unwrap())
    }

    /// Blocks if want > 0. want is the number of CQEs to wait for.
    /// Returns the number of SQEs submitted
    pub fn submit_and_wait(&self, want: usize) -> usize {
        match self.0.submitter().submit_and_wait(want) {
            Ok(submitted) => submitted,
            Err(e) => match e.raw_os_error() {
                EAGAIN | EBUSY | EINTR => {
                    // EAGAIN indicates temporary inability to allocate resources.
                    // EBUSY means no CQE positions available
                    // EINTR means we got as signal
                    // In these three cases, the guidance is to process completions and continue
                    0
                }
                _ => {
                    // EBADF, EFAULT, EINVAL, ENXIO, EOPNOTSUPP
                    panic!("Error submitting or waiting for IO: {e:?}");
                }
            },
        }
    }

    pub fn register_probe(&self) -> rustix_uring::Probe {
        let mut probe = rustix_uring::Probe::new();
        self.0.submitter().register_probe(&mut probe).unwrap();
        probe
    }

    pub fn get_completion_count(&mut self) -> usize {
        self.0.completion().len()
    }

    #[inline(always)]
    pub fn get_next_cqe(&mut self) -> Option<CQE> {
        self.0.completion().next()
    }

    #[inline(always)]
    pub fn submit(&mut self, entries: &[SQE]) {
        // SAFETY: the pointers in entry have to live until the CQE is processed.
        // This will be the case as long as RingFuture is called from an "async"
        // method.
        let result = unsafe { self.0.submission().push_multiple(entries) };
        if result.is_err() {
            self.submit_and_wait(0);
            unsafe { self.0.submission().push_multiple(entries) }.expect("Can not push SQE");
        }
    }
}

use enter_stats::EnterStats;

#[cfg(all(not(test), not(feature = "enter_stats")))]
mod enter_stats {
    #[derive(Default)]
    pub struct EnterStats;

    impl EnterStats {
        pub fn new() -> Self {
            Self
        }

        pub fn start(&mut self) {}
        pub fn stop(&mut self) {}
        pub fn report(&self, _msg: &str) {}
    }
}

#[cfg(any(test, feature = "enter_stats"))]
mod enter_stats {
    use crate::timer::Timer;

    pub struct EnterStats {
        pub sum: u64,
        pub count: u64,
        pub min: u64,
        pub max: u64,
        pub start: u64,
    }

    impl EnterStats {
        pub fn new() -> Self {
            Self {
                sum: 0,
                count: 0,
                min: u64::MAX,
                max: 0,
                start: 0,
            }
        }
        pub fn start(&mut self) {
            self.start = Timer::ticks();
        }
        pub fn stop(&mut self) {
            let elapsed = Timer::ticks() - self.start;
            self.sum += elapsed;
            self.count += 1;
            self.min = std::cmp::min(self.min, elapsed);
            self.max = std::cmp::max(self.max, elapsed);
        }
        pub fn report(&self, msg: &str) {
            let t_per_us = Timer::ticks_per_us();
            if self.count > 0 {
                println!(
                    "EnterStats {msg}  count: {} , sum: {}ns, avg: {}ns, min: {}ns, max: {}ns",
                    self.count,
                    1000 * self.sum / t_per_us,
                    1000 * self.sum / t_per_us * self.count,
                    1000 * self.min / t_per_us,
                    1000 * self.max / t_per_us
                )
            } else {
                println!("EnterStats {msg}  count: 0",);
            }
        }
    }

    impl Default for EnterStats {
        fn default() -> Self {
            Self::new()
        }
    }
}

// All the information about tasks and task readiness
pub struct TaskState {
    // The current number of tasks that exist for this thread.
    // When it goes to zero, the thread can exit.
    pub task_count: usize,

    // true if the last task to be polled was a cpu task. Used
    // to determine whether or not to submit I/O early.
    pub last_ready_cpu: bool,

    // The thread id of the thread
    pub thread_id: ThreadId,

    // The I/O URing for this thread
    pub ring: Ring,

    // The I/O URing for this thread (with IO Poll enabled)
    pub ring_poll: Ring,

    pub(crate) trace_buffer: TraceBuffer,

    // Statistics for this thread (TODO replace with trace buffer analysis)
    pub stats: URingStats,

    // sum, count, min, max of duration of enter calls without wait
    pub enter_stats: EnterStats,

    // sum, count, min, max of duration of enter calls with wait
    pub enter_stats_wait: EnterStats,

    // The task that is in Running state
    pub current_task: Option<Rc<Task>>,

    // A list of Ready tasks that would like to run with I/O priority.
    // Tasks that become ready are added here, and then moved to
    // ready_task_list_io as a cohort.
    pub fill_ready_task_list_io: VecDeque<Rc<Task>>,

    // A list of Ready tasks to be polled together before the next
    // I/O uring submission.
    pub ready_task_list_io: VecDeque<Rc<Task>>,

    // A list of Ready tasks that would like to run with CPU priority
    pub ready_task_list_cpu: VecDeque<Rc<Task>>,

    pub(crate) completion_pool: VecDeque<Rc<Completion>>,

    // All tasks
    pub tasks: HandleTable<Rc<Task>>,

    pub completed_tasks: VecDeque<Rc<Task>>,

    // An id for tracking begin and end trace events. It does not guarantee
    // uniqueness but relies on wrap around taking enough time.
    pub next_tag: u32,

    // probe is used to check if the kernel supports certain I/O uring operations
    pub probe: rustix_uring::Probe,

    // Boolean indicating if the event loop should continue to process new
    // events or immediately terminate. Used internally to implement shutdown
    // at the application.rs framework level.
    pub keep_running: bool,

    #[cfg(feature = "fault_injection")]
    pub fault: Option<(usize, rustix_uring::Errno)>,
}

thread_local! {
    static TASK_STATE: TaskStateCell = TaskStateCell::new(TaskState::default());
}

impl TaskState {
    pub fn new() -> Self {
        let ring = Ring::new(false);
        let ring_poll = Ring::new(true);
        let probe = ring.register_probe();
        Self {
            task_count: 0,
            last_ready_cpu: false,
            thread_id: std::thread::current().id(),
            ring,
            ring_poll,
            trace_buffer: TraceBuffer::default(),
            stats: URingStats::new(),
            enter_stats: EnterStats::new(),
            enter_stats_wait: EnterStats::new(),
            current_task: None,
            fill_ready_task_list_io: VecDeque::new(),
            ready_task_list_io: VecDeque::new(),
            ready_task_list_cpu: VecDeque::new(),
            completion_pool: VecDeque::new(),
            tasks: HandleTable::new(),
            completed_tasks: VecDeque::new(),
            probe,
            keep_running: true,
            next_tag: 0,
            #[cfg(feature = "fault_injection")]
            fault: None,
        }
    }

    #[cfg(feature = "fault_injection")]
    pub fn inject_fault(&mut self, operation: usize, fault: rustix_uring::Errno) {
        self.fault = Some((operation, fault));
    }

    pub fn get() -> TaskStateCellRef<'static> {
        let task_state = TASK_STATE.with(|task_state| task_state as *const TaskStateCell);
        // SAFETY: although thread locals are not actually static, we can
        // treat this one as if it might as well be, since tasks can not
        // run after their thread is gone.
        let task_state = unsafe { &*task_state };
        task_state.borrow_mut()
    }

    #[inline(always)]
    pub fn get_task_count(&self) -> usize {
        self.task_count
    }

    #[inline(always)]
    pub fn get_completion_count(&mut self, iopoll: bool) -> usize {
        if iopoll {
            self.ring_poll.get_completion_count()
        } else {
            self.ring.get_completion_count()
        }
    }

    #[inline(always)]
    pub fn get_next_cqe(&mut self, iopoll: bool) -> Option<CQE> {
        if iopoll {
            self.ring_poll.get_next_cqe()
        } else {
            self.ring.get_next_cqe()
        }
    }

    #[inline(always)]
    pub fn get_current_task(&self) -> Rc<Task> {
        self.current_task.as_ref().unwrap().clone()
    }

    #[inline(always)]
    pub fn get_current_thread_id(&self) -> std::thread::ThreadId {
        self.thread_id
    }

    #[inline(always)]
    pub fn get_next_tag(&mut self) -> u32 {
        let next_tag = self.next_tag;
        self.next_tag = u32::wrapping_add(next_tag, 1);
        next_tag
    }

    #[inline(always)]
    pub fn get_current_activity_id(&self) -> uuid::Uuid {
        self.current_task
            .as_ref()
            .map(|task| task.activity_id.get())
            .unwrap_or_default()
    }

    #[inline(always)]
    pub fn get_current_tenant_id(&self) -> uuid::Uuid {
        self.current_task
            .as_ref()
            .map(|task| task.tenant_id.get())
            .unwrap_or_default()
    }

    pub fn set_current_activity_id_and_tenant_id(
        &mut self,
        activity_id: uuid::Uuid,
        tenant_id: uuid::Uuid,
    ) {
        if let Some(current_task) = self.current_task.as_ref() {
            current_task.activity_id.set(activity_id);
            current_task.tenant_id.set(tenant_id);
        }
    }

    pub fn any_ready(&self) -> bool {
        !self.fill_ready_task_list_io.is_empty()
            | !self.ready_task_list_io.is_empty()
            | !self.ready_task_list_cpu.is_empty()
    }

    /// Trigger a shutdown of the uringruntime instance actively running on
    /// this thread. Internally, the processing loop will exit after finishing
    /// the current iteration. Other pending completions and tasks may continue
    /// executing until the next iteration around the event loop. After this
    /// method is called, the event loop will terminate and kimojio::run()
    /// will return back to the calling thread.
    pub(crate) fn shutdown(&mut self) {
        self.keep_running = false
    }

    pub(crate) fn schedule_new<F>(
        &mut self,
        future: F,
        activity_id: uuid::Uuid,
        tenant_id: uuid::Uuid,
    ) -> Rc<Task>
    where
        F: Future + 'static,
    {
        let task_state_ptr = self as *const TaskState;
        let task_index = self.tasks.insert_fn(|task_index| {
            Task::new(
                future,
                task_index.get() as u16,
                task_state_ptr,
                activity_id,
                tenant_id,
            )
        });
        let task = self.tasks.get(task_index).unwrap().clone(); // just added
        self.task_count += 1;
        self.schedule_io_internal(task.clone());

        task
    }

    fn schedule_io_internal(&mut self, task: Rc<Task>) {
        assert_eq!(
            self as *const TaskState, task.task_state,
            "It is incorrect to wake a uringruntime task from another thread"
        );

        let tag = self.get_next_tag();
        let activity_id = task.activity_id.get();
        let tenant_id = task.tenant_id.get();
        task.tag.set(tag);
        self.write_event(
            task.task_index,
            Events::TaskScheduled {
                tag,
                activity_id,
                tenant_id,
                io: true,
            },
        );

        let high_priority = self
            .current_task
            .as_ref()
            .is_some_and(|task| task.high_priority.get());
        if high_priority {
            self.ready_task_list_io.push_back(task);
        } else {
            self.fill_ready_task_list_io.push_back(task);
        }
    }

    pub fn schedule_io(&mut self, task: Rc<Task>) {
        match task.get_state() {
            TaskReadyState::Complete => {
                // ignore I/O completions trying to schedule a completed task. This happens
                // if a task is completed with pending I/O outstanding
            }
            TaskReadyState::Running | TaskReadyState::Suspend => {
                // yield_io and yield_cpu call schedule_io on themselves when they are in
                // running state.  We allow this but it is important that they immediately
                // return Pending so that we don't have a running task in Ready state.
                task.ready.set(TaskReadyState::Ready);
                self.schedule_io_internal(task);
            }
            TaskReadyState::Ready | TaskReadyState::Aborted => {
                // Ignore scheduling a task that is already ready or canceled
            }
            TaskReadyState::Unknown => {
                panic!("Invalid task state");
            }
        }
    }

    pub fn schedule_cpu(&mut self, task: Rc<Task>) {
        match task.get_state() {
            TaskReadyState::Complete => {
                // ignore I/O completions trying to schedule a completed task. This happens
                // if a task is completed with pending I/O outstanding
            }
            TaskReadyState::Running | TaskReadyState::Suspend => {
                // yield_io and yield_cpu call schedule_io on themselves when they are in
                // running state.  We allow this but it is important that they immediately
                // return Pending so that we don't have a running task in Ready state.
                task.set_state(TaskReadyState::Ready);
                self.ready_task_list_cpu.push_back(task);
            }
            TaskReadyState::Ready | TaskReadyState::Aborted => {
                // Ignore scheduling a task that is already ready or canceled
            }
            TaskReadyState::Unknown => {
                panic!("Invalid task state");
            }
        }
    }

    #[inline(always)]
    pub fn submit(&mut self, entries: &[SQE]) {
        self.ring.submit(entries)
    }

    #[inline(always)]
    pub fn submit_poll(&mut self, entries: &[SQE]) {
        self.ring_poll.submit(entries)
    }

    pub fn complete_task<'a>(cell: &'a TaskStateCell, task: &Task) -> TaskStateCellRef<'a> {
        debug_assert!(task.get_state() != TaskReadyState::Complete);
        task.set_state(TaskReadyState::Complete);
        task.joining_tasks.wake_all();

        // borrow after updating task state and especially invoking wakers,
        // as this may recursively need to borrow TaskStateCellRef.
        let mut this = cell.borrow_mut();
        let index = NonZeroUsize::new(task.task_index as usize).unwrap();
        if let Some(task) = this.tasks.remove(index.into()) {
            this.completed_tasks.push_back(task);
        }
        this.task_count -= 1;
        this
    }

    pub fn prepare_cohort(&mut self) {
        if self.ready_task_list_io.is_empty() {
            swap(
                &mut self.ready_task_list_io,
                &mut self.fill_ready_task_list_io,
            )
        } else {
            // old cohort still leftover due to submission queue being full,
            // just continue working on the old one.
        }
    }

    pub fn get_ready(&mut self) -> Option<Rc<Task>> {
        if let Some(task) = self.ready_task_list_io.pop_front() {
            self.stats.increment_tasks_polled_io();
            let task_ready = task.get_state();
            if task_ready != TaskReadyState::Aborted && task_ready != TaskReadyState::Complete {
                task.set_state(TaskReadyState::Running);
            }
            self.last_ready_cpu = false;
            return Some(task);
        }

        if self.last_ready_cpu {
            self.last_ready_cpu = false;
            return None;
        }

        if let Some(task) = self.ready_task_list_cpu.pop_front() {
            if task.get_state() != TaskReadyState::Aborted {
                task.set_state(TaskReadyState::Running);
            }
            self.stats.increment_tasks_polled_cpu();
            self.last_ready_cpu = true;
            Some(task)
        } else {
            self.last_ready_cpu = false;
            None
        }
    }

    #[inline(always)]
    pub fn write_event(&self, task_index: u16, event: Events) {
        self.trace_buffer.write_event(task_index, event)
    }

    pub fn abort_task(&mut self, task: Rc<Task>) {
        match task.get_state() {
            TaskReadyState::Suspend => {
                // If tasks is suspended, mark it aborted and schedule it so the
                // cancelation can be turned into a panic
                task.set_state(TaskReadyState::Aborted);
                self.schedule_io_internal(task);
            }
            TaskReadyState::Running => {
                // if you abort yourself, then just panic
                panic!("Task aborted");
            }
            // Ready tasks are scheduled to run, mark it panic but don't schedule
            TaskReadyState::Ready => {
                task.set_state(TaskReadyState::Aborted);
            }
            // Already aborted tasks do not get re-aborted
            TaskReadyState::Aborted => (),
            // Complete tasks can not be aborted, ignore
            TaskReadyState::Complete => (),
            // Tasks should never be in Unknown state
            TaskReadyState::Unknown => unreachable!(),
        }
    }

    pub(crate) fn return_completion(&mut self, completion: Rc<Completion>) {
        debug_assert!(
            Rc::weak_count(&completion) == 0,
            "Completion should not have weak refs"
        );
        if self.completion_pool.len() < MAX_COMPLETION_POOL_ENTRIES
            // Since we do not use weak refs for completions, a strong count of 1
            // on our owned Rc<> indicates that we are the only owner of the completion.
            && Rc::strong_count(&completion) == 1
        {
            self.completion_pool.push_back(completion)
        }
    }

    pub(crate) fn new_completion(&mut self, completion: Completion) -> Rc<Completion> {
        if let Some(mut existing) = self.completion_pool.pop_front() {
            let ptr =
                Rc::get_mut(&mut existing).expect("Pooled completion must have singular refcount");
            *ptr = completion;
            existing
        } else {
            Rc::new(completion)
        }
    }
}

impl Default for TaskState {
    fn default() -> Self {
        Self::new()
    }
}
