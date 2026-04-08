// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Clock abstraction for deterministic timing in tests.
//!
//! Users interact with the virtual clock through the
//! [`operations`](crate::operations) module functions:
//! [`virtual_clock_enable`](crate::operations::virtual_clock_enable),
//! [`virtual_clock_advance`](crate::operations::virtual_clock_advance), etc.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::task::Waker;
use std::time::{Duration, Instant};

/// Type alias for the idle advance callback.
type IdleAdvanceFn = dyn FnMut(Instant, Option<Instant>) -> Option<Duration>;

/// Opaque identifier for a registered virtual timer.
///
/// Returned by [`VirtualClockState::register_timer()`] and used with
/// [`VirtualClockState::cancel_timer()`] to cancel pending timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(pub(crate) u64);

/// Virtual clock state for deterministic test timing.
///
/// `VirtualClockState` maintains a user-controlled notion of "now" that does not
/// advance automatically. Instead, test code explicitly advances time via
/// [`advance()`](VirtualClockState::advance) or
/// [`advance_to()`](VirtualClockState::advance_to), causing any timers whose
/// deadlines have been reached to be returned for the caller to wake.
///
/// This enables tests that involve timeouts, sleeps, and deadlines to run in
/// microseconds instead of waiting real wall-clock time, while maintaining
/// deterministic ordering guarantees.
///
/// # Timer Ordering
///
/// When advancing past multiple deadlines simultaneously, timers are returned
/// in deadline order (earliest first). Timers with equal deadlines are returned
/// in registration order (earliest registered first), providing fully
/// deterministic behavior.
///
/// # Re-entrancy
///
/// `advance()` and `advance_to()` collect expired wakers into a `Vec` and
/// return them to the caller rather than waking them internally. The caller
/// must release the `TaskState` borrow before waking, so that woken futures
/// can re-borrow `TaskState` via `schedule_io`.
pub(crate) struct VirtualClockState {
    epoch: Instant,
    current: Instant,
    timers: BinaryHeap<VirtualTimer>,
    next_timer_id: u64,
    idle_advance_fn: Option<Box<IdleAdvanceFn>>,
    idle_advance_dirty: bool,
}

struct VirtualTimer {
    id: TimerId,
    deadline: Instant,
    waker: Waker,
}

// Ordering: min-heap by (deadline, id) for deterministic tie-breaking
impl PartialEq for VirtualTimer {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.id == other.id
    }
}

impl Eq for VirtualTimer {}

impl PartialOrd for VirtualTimer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VirtualTimer {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering for min-heap: smallest (deadline, id) has highest priority
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.id.0.cmp(&self.id.0))
    }
}

impl VirtualClockState {
    /// Creates a new virtual clock state starting at the given instant.
    pub(crate) fn new(start: Instant) -> Self {
        Self {
            epoch: start,
            current: start,
            timers: BinaryHeap::new(),
            next_timer_id: 1,
            idle_advance_fn: None,
            idle_advance_dirty: false,
        }
    }

    /// Returns the epoch (start time) of this virtual clock.
    pub(crate) fn epoch(&self) -> Instant {
        self.epoch
    }

    /// Returns the current virtual time.
    ///
    /// This does not advance; it returns whatever time was last set via
    /// [`advance()`](Self::advance) or [`advance_to()`](Self::advance_to).
    pub(crate) fn now(&self) -> Instant {
        self.current
    }

    /// Advances virtual time by the given duration.
    ///
    /// All timers with deadlines at or before the new current time are collected
    /// and returned in deadline order. Returns the count of expired timers and the
    /// wakers to call. The caller must release the `TaskState` borrow before
    /// calling `waker.wake()` to avoid re-entrancy issues.
    pub(crate) fn advance(&mut self, duration: Duration) -> (usize, Vec<Waker>) {
        let new_time = self.current.checked_add(duration).unwrap_or_else(|| {
            // Saturate rather than panic on extreme durations.
            self.current + Duration::from_secs(365 * 24 * 3600 * 100)
        });
        self.advance_to(new_time)
    }

    /// Advances virtual time to a specific instant.
    ///
    /// All timers with deadlines at or before `target` are collected and returned
    /// in deadline order. If `target` is before the current time, no timers are
    /// returned and the clock is not moved backward.
    ///
    /// Returns the count of expired timers and the wakers to call. The caller must
    /// release the `TaskState` borrow before calling `waker.wake()`.
    pub(crate) fn advance_to(&mut self, target: Instant) -> (usize, Vec<Waker>) {
        if target > self.current {
            self.current = target;
        }

        let mut expired = Vec::new();
        while let Some(timer) = self.timers.peek() {
            if timer.deadline <= self.current {
                let timer = self.timers.pop().unwrap();
                expired.push(timer.waker);
            } else {
                break;
            }
        }

        let fired = expired.len();
        (fired, expired)
    }

    /// Returns the next pending timer deadline, or `None` if no timers
    /// are registered.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.timers.peek().map(|t| t.deadline)
    }

    /// Returns the number of pending (unfired) timers.
    pub(crate) fn pending_timers(&self) -> usize {
        self.timers.len()
    }

    /// Registers a timer to fire at `deadline`, returning a unique [`TimerId`].
    ///
    /// When the deadline is reached (via time advancement), the provided
    /// [`Waker`] will be included in the `Vec` returned by `advance_to`.
    pub(crate) fn register_timer(&mut self, deadline: Instant, waker: Waker) -> TimerId {
        let id = TimerId(self.next_timer_id);
        self.next_timer_id += 1;
        self.timers.push(VirtualTimer {
            id,
            deadline,
            waker,
        });
        id
    }

    /// Cancels a previously registered timer.
    ///
    /// If the timer has already fired or does not exist, this is a no-op.
    pub(crate) fn cancel_timer(&mut self, id: TimerId) {
        self.timers.retain(|t| t.id != id);
    }

    /// Installs or replaces the idle advance callback.
    ///
    /// When installed, the runtime calls this callback at each idle point with
    /// `(current_virtual_time, next_pending_timer_deadline)`. If it returns
    /// `Some(duration)`, virtual time advances by that amount. If `None`,
    /// the runtime blocks in io_uring normally.
    ///
    /// Pass `None` to clear any installed callback.
    pub(crate) fn set_idle_advance_fn(&mut self, f: Option<Box<IdleAdvanceFn>>) {
        self.idle_advance_fn = f;
        self.idle_advance_dirty = true;
    }

    /// Returns `true` if an idle advance callback is installed.
    pub(crate) fn has_idle_advance_fn(&self) -> bool {
        self.idle_advance_fn.is_some()
    }

    /// Extracts the idle advance callback for external invocation.
    ///
    /// Resets the dirty flag so `restore_idle_advance_fn` can detect
    /// whether user code replaced or cleared the callback during invocation.
    pub(crate) fn take_idle_advance_fn(&mut self) -> Option<Box<IdleAdvanceFn>> {
        self.idle_advance_dirty = false;
        self.idle_advance_fn.take()
    }

    /// Restores the callback after invocation, unless user code touched
    /// the field during invocation (dirty flag set by `set_idle_advance_fn`).
    /// Returns `true` if the callback was replaced or cleared during invocation
    /// (dirty), so the caller can re-arm idle advancement for the new callback.
    pub(crate) fn restore_idle_advance_fn(&mut self, f: Box<IdleAdvanceFn>) -> bool {
        if !self.idle_advance_dirty {
            self.idle_advance_fn = Some(f);
            false
        } else {
            // User code called set/clear during invocation — drop the old callback.
            true
        }
    }
}

/// A future that completes when the virtual clock reaches the specified deadline.
///
/// Created internally by [`sleep()`](crate::operations::sleep) and
/// [`sleep_until()`](crate::operations::sleep_until) when a virtual clock
/// is active. Registers a timer with the clock on first poll and resolves
/// when virtual time is advanced past the deadline.
///
/// Accesses the virtual clock through `TaskState::get()` rather than storing
/// a direct reference, since `VirtualClockState` is now stored directly in
/// `TaskState` (no `Rc` wrapper).
///
/// Implements [`Drop`] to cancel the registered timer if the future is
/// dropped before completion (timer cancellation on drop).
pub(crate) struct VirtualSleepFuture {
    deadline: Instant,
    timer_id: Option<TimerId>,
    cached_waker: Option<Waker>,
    completed: bool,
    /// Ensures at least one yield before completing, matching real-time
    /// sleep's cooperative scheduling behavior. Without this, tight loops
    /// on `sleep(Duration::ZERO)` would starve other tasks.
    yielded_once: bool,
}

impl VirtualSleepFuture {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            timer_id: None,
            cached_waker: None,
            completed: false,
            yielded_once: false,
        }
    }

    pub(crate) fn cancel(&mut self) {
        if let Some(id) = self.timer_id.take()
            && let Some(clock) = crate::task::TaskState::get().clock.as_mut()
        {
            clock.cancel_timer(id);
        }
        self.completed = true;
    }

    pub(crate) fn is_terminated(&self) -> bool {
        self.completed
    }
}

impl std::future::Future for VirtualSleepFuture {
    type Output = Result<(), rustix_uring::Errno>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if self.completed {
            panic!("polled after completion");
        }

        let now = crate::task::TaskState::get()
            .clock
            .as_ref()
            .expect("virtual clock not active")
            .now();

        if now >= self.deadline {
            // Yield once before completing to maintain cooperative
            // scheduling. Real-time sleep always returns Pending on first
            // poll (timer hasn't fired yet). Without this, tight
            // `sleep(Duration::ZERO)` loops would starve other tasks.
            if !self.yielded_once {
                self.yielded_once = true;
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            self.completed = true;
            // Just clear the timer_id — if advance() fired this timer,
            // it's already removed from the heap. Drop handles cleanup
            // for non-completed futures.
            self.timer_id.take();
            return std::task::Poll::Ready(Ok(()));
        }

        // Only cancel and re-register if we have no active timer or the
        // waker has changed. Preserves the original TimerId on spurious
        // wakeups, which maintains deterministic ordering for equal
        // deadlines.
        let needs_register = match (&self.timer_id, &self.cached_waker) {
            (Some(_), Some(w)) => !w.will_wake(cx.waker()),
            _ => true,
        };

        if needs_register {
            let old_id = self.timer_id.take();
            let new_waker = cx.waker().clone();
            let deadline = self.deadline;

            let mut task_state = crate::task::TaskState::get();
            let clock = task_state.clock.as_mut().expect("virtual clock not active");
            if let Some(id) = old_id {
                clock.cancel_timer(id);
            }
            let id = clock.register_timer(deadline, new_waker.clone());
            drop(task_state);

            self.timer_id = Some(id);
            self.cached_waker = Some(new_waker);
        }

        std::task::Poll::Pending
    }
}

impl Drop for VirtualSleepFuture {
    fn drop(&mut self) {
        if let Some(id) = self.timer_id.take()
            && let Some(clock) = crate::task::TaskState::get().clock.as_mut()
        {
            clock.cancel_timer(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct CountingWake(AtomicUsize);

    impl std::task::Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    fn counting_waker() -> (Waker, Arc<CountingWake>) {
        let counter = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        (waker, counter)
    }

    struct OrderRecorder {
        index: usize,
        order: Arc<std::sync::Mutex<Vec<usize>>>,
    }

    impl std::task::Wake for OrderRecorder {
        fn wake(self: Arc<Self>) {
            self.order.lock().unwrap().push(self.index);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.order.lock().unwrap().push(self.index);
        }
    }

    fn ordering_waker(index: usize, order: &Arc<std::sync::Mutex<Vec<usize>>>) -> Waker {
        Waker::from(Arc::new(OrderRecorder {
            index,
            order: order.clone(),
        }))
    }

    #[test]
    fn virtual_clock_initial_time() {
        let start = Instant::now();
        let state = VirtualClockState::new(start);
        assert_eq!(state.now(), start);
    }

    #[test]
    fn advance_with_no_timers_returns_zero() {
        let mut state = VirtualClockState::new(Instant::now());
        let (fired, wakers) = state.advance(Duration::from_secs(10));
        assert_eq!(fired, 0);
        assert!(wakers.is_empty());
    }

    #[test]
    fn advance_by_zero_fires_no_timers() {
        let mut state = VirtualClockState::new(Instant::now());
        let (waker, counter) = counting_waker();
        state.register_timer(state.now() + Duration::from_secs(1), waker);
        let (fired, wakers) = state.advance(Duration::ZERO);
        assert_eq!(fired, 0);
        assert!(wakers.is_empty());
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn advance_fires_expired_timers() {
        let start = Instant::now();
        let mut state = VirtualClockState::new(start);
        let (waker, counter) = counting_waker();
        state.register_timer(start + Duration::from_secs(5), waker);
        let (fired, wakers) = state.advance(Duration::from_secs(5));
        assert_eq!(fired, 1);
        for w in wakers {
            w.wake();
        }
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn advance_does_not_fire_future_timers() {
        let start = Instant::now();
        let mut state = VirtualClockState::new(start);
        let (waker, counter) = counting_waker();
        state.register_timer(start + Duration::from_secs(10), waker);
        let (fired, wakers) = state.advance(Duration::from_secs(3));
        assert_eq!(fired, 0);
        assert!(wakers.is_empty());
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn timers_fire_in_deadline_order() {
        let start = Instant::now();
        let mut state = VirtualClockState::new(start);
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        for (secs, i) in [(5u64, 0usize), (10, 1), (7, 2)] {
            let waker = ordering_waker(i, &order);
            state.register_timer(start + Duration::from_secs(secs), waker);
        }
        let (fired, wakers) = state.advance(Duration::from_secs(15));
        assert_eq!(fired, 3);
        for w in wakers {
            w.wake();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 2, 1]);
    }

    #[test]
    fn equal_deadline_fires_in_registration_order() {
        let start = Instant::now();
        let mut state = VirtualClockState::new(start);
        let deadline = start + Duration::from_secs(5);
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        for i in 0..3 {
            let waker = ordering_waker(i, &order);
            state.register_timer(deadline, waker);
        }
        let (fired, wakers) = state.advance(Duration::from_secs(5));
        assert_eq!(fired, 3);
        for w in wakers {
            w.wake();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn cancel_timer_prevents_firing() {
        let start = Instant::now();
        let mut state = VirtualClockState::new(start);
        let (waker, counter) = counting_waker();
        let id = state.register_timer(start + Duration::from_secs(5), waker);
        state.cancel_timer(id);
        let (fired, wakers) = state.advance(Duration::from_secs(10));
        assert_eq!(fired, 0);
        assert!(wakers.is_empty());
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn cancel_nonexistent_timer_is_noop() {
        let mut state = VirtualClockState::new(Instant::now());
        state.cancel_timer(TimerId(999));
    }

    #[test]
    fn next_deadline_returns_earliest() {
        let start = Instant::now();
        let mut state = VirtualClockState::new(start);
        let (w1, _) = counting_waker();
        let (w2, _) = counting_waker();
        let (w3, _) = counting_waker();
        state.register_timer(start + Duration::from_secs(10), w1);
        state.register_timer(start + Duration::from_secs(3), w2);
        state.register_timer(start + Duration::from_secs(7), w3);
        assert_eq!(state.next_deadline(), Some(start + Duration::from_secs(3)));
    }

    #[test]
    fn next_deadline_none_when_empty() {
        let state = VirtualClockState::new(Instant::now());
        assert_eq!(state.next_deadline(), None);
    }

    #[test]
    fn advance_to_specific_instant() {
        let start = Instant::now();
        let mut state = VirtualClockState::new(start);
        let (w1, c1) = counting_waker();
        let (w2, _c2) = counting_waker();
        state.register_timer(start + Duration::from_secs(5), w1);
        state.register_timer(start + Duration::from_secs(10), w2);
        let target = start + Duration::from_secs(7);
        let (fired, wakers) = state.advance_to(target);
        assert_eq!(fired, 1);
        assert_eq!(state.now(), target);
        for w in wakers {
            w.wake();
        }
        assert_eq!(c1.0.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn advance_to_before_current_time_is_noop() {
        let start = Instant::now();
        let mut state = VirtualClockState::new(start);
        state.advance(Duration::from_secs(10));
        let before = start + Duration::from_secs(5);
        let (fired, wakers) = state.advance_to(before);
        assert_eq!(fired, 0);
        assert!(wakers.is_empty());
        assert_eq!(state.now(), start + Duration::from_secs(10));
    }

    #[test]
    fn pending_timers_count() {
        let start = Instant::now();
        let mut state = VirtualClockState::new(start);
        assert_eq!(state.pending_timers(), 0);
        let (w1, _) = counting_waker();
        let (w2, _) = counting_waker();
        state.register_timer(start + Duration::from_secs(5), w1);
        state.register_timer(start + Duration::from_secs(10), w2);
        assert_eq!(state.pending_timers(), 2);
        state.advance(Duration::from_secs(7));
        assert_eq!(state.pending_timers(), 1);
    }

    #[test]
    fn past_deadline_timer_fires_on_next_advance() {
        let start = Instant::now();
        let mut state = VirtualClockState::new(start + Duration::from_secs(10));
        let (waker, counter) = counting_waker();
        state.register_timer(start + Duration::from_secs(5), waker);
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 0);
        let (fired, wakers) = state.advance(Duration::ZERO);
        assert_eq!(fired, 1);
        for w in wakers {
            w.wake();
        }
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 1);
    }

    // ── Idle advance callback tests ─────────────────────────────

    #[test]
    fn idle_advance_fn_none_by_default() {
        let state = VirtualClockState::new(Instant::now());
        assert!(!state.has_idle_advance_fn());
    }

    #[test]
    fn set_idle_advance_fn_stores() {
        let mut state = VirtualClockState::new(Instant::now());
        state.set_idle_advance_fn(Some(Box::new(|_, _| Some(Duration::from_secs(1)))));
        assert!(state.has_idle_advance_fn());
    }

    #[test]
    fn set_and_clear_idle_advance_fn() {
        let mut state = VirtualClockState::new(Instant::now());
        state.set_idle_advance_fn(Some(Box::new(|_, _| Some(Duration::from_secs(1)))));
        assert!(state.has_idle_advance_fn());

        state.set_idle_advance_fn(None);
        assert!(!state.has_idle_advance_fn());
    }

    #[test]
    fn take_idle_advance_fn_returns_none_when_empty() {
        let mut state = VirtualClockState::new(Instant::now());
        assert!(state.take_idle_advance_fn().is_none());
    }

    #[test]
    fn take_idle_advance_fn_extracts_callback() {
        let mut state = VirtualClockState::new(Instant::now());
        state.set_idle_advance_fn(Some(Box::new(|_, _| Some(Duration::from_secs(1)))));
        let cb = state.take_idle_advance_fn();
        assert!(cb.is_some());
        assert!(!state.has_idle_advance_fn());
    }

    #[test]
    fn restore_idle_advance_fn_puts_back_when_clean() {
        let mut state = VirtualClockState::new(Instant::now());
        state.set_idle_advance_fn(Some(Box::new(|_, _| Some(Duration::from_secs(1)))));
        let cb = state.take_idle_advance_fn().unwrap();
        // dirty flag was reset by take
        assert!(!state.has_idle_advance_fn());
        let was_replaced = state.restore_idle_advance_fn(cb);
        assert!(!was_replaced);
        assert!(state.has_idle_advance_fn());
    }

    #[test]
    fn restore_idle_advance_fn_drops_when_dirty() {
        let mut state = VirtualClockState::new(Instant::now());
        state.set_idle_advance_fn(Some(Box::new(|_, _| Some(Duration::from_secs(1)))));
        let cb = state.take_idle_advance_fn().unwrap();
        // Simulate user code calling set during invocation
        state.set_idle_advance_fn(None);
        // dirty flag is now set — restore should NOT put back the old callback
        let was_replaced = state.restore_idle_advance_fn(cb);
        assert!(was_replaced);
        assert!(!state.has_idle_advance_fn());
    }

    #[test]
    fn callback_invoked_with_correct_params() {
        let start = Instant::now();
        let mut state = VirtualClockState::new(start);
        let (waker, _) = counting_waker();
        let deadline = start + Duration::from_secs(10);
        state.register_timer(deadline, waker);

        state.set_idle_advance_fn(Some(Box::new(|now, next| {
            next.map(|d| d.saturating_duration_since(now))
        })));

        let mut cb = state.take_idle_advance_fn().unwrap();
        let now = state.now();
        let next = state.next_deadline();
        let result = cb(now, next);

        assert_eq!(now, start);
        assert_eq!(next, Some(deadline));
        assert_eq!(result, Some(Duration::from_secs(10)));
    }
}
