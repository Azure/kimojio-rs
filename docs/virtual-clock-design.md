# Virtual Clock Design Document

## Overview

The virtual clock feature provides deterministic timing for kimojio's async
runtime, enabling tests that involve timeouts, sleeps, and deadlines to run
in microseconds instead of waiting for real wall-clock time.

## Architecture

### Component Diagram

```
┌─────────────────────────────────────────────────────────────┐
│                      User Test Code                          │
│  operations::virtual_clock_advance(60s)                      │
│                    ←→  operations::sleep(60s)                │
└───────┬───────────────────────────────────┬─────────────────┘
        │                                   │
        ▼                                   ▼
┌─────────────────────────┐      ┌────────────────────────────┐
│  VirtualClockState      │      │   VirtualSleepFuture        │
│  ┌────────────┐         │      │  deadline: Instant          │
│  │ BinaryHeap │◄────────┼──────│  timer_id: Option<TimerId>  │
│  │  (timers)  │  reg/   │      │  completed: bool            │
│  └────────────┘  cancel │      │  (accesses clock via        │
│  current: Instant       │      │   TaskState::get())         │
└──────────┬──────────────┘      └────────────────────────────┘
           │ Option<VirtualClockState>
           ▼
┌───────────────────────────────────────────────────────────┐
│                     TaskState                              │
│  clock: Option<VirtualClockState>   (feature-gated)       │
└──────────────────────┬────────────────────────────────────┘
                       │
        ┌──────────────┼─────────────────────┐
        ▼              ▼                     ▼
  operations.rs   async_event.rs       async_lock.rs
  clock_now()     clock_now()          clock_now()
  virtual_clock_*()
```

### Operations-Based API

All virtual clock interaction happens through `operations::` module functions.
The `VirtualClockState` type is internal to the crate — users never construct
or access it directly. Instead:

- `operations::virtual_clock_enable(true)` creates a `VirtualClockState` and
  installs it into the thread-local `TaskState`.
- `operations::virtual_clock_advance(duration)` borrows `TaskState`, mutates
  the clock, collects expired wakers, drops the borrow, then wakes the wakers.
- Other `virtual_clock_*` functions similarly access the clock through
  the `TaskState` thread-local.

This follows kimojio's existing pattern where control functions in `operations`
module operate through thread-local `TaskState`.

### VirtualClockState (Internal Type)

`VirtualClockState` is a `pub(crate)` struct stored directly inside `TaskState`
as `Option<VirtualClockState>`:

- **`None`**: Production behavior. `clock_now()` returns `Instant::now()`.
  Timer operations use real io_uring kernel timeouts.
- **`Some(VirtualClockState)`**: Test behavior. `clock_now()` returns the
  virtual clock's current time. Sleeps dispatch to `VirtualSleepFuture`.

Unlike the previous design, there is no `Rc<RefCell<>>` wrapper. The state
lives directly in `TaskState`, which already provides interior mutability via
`TaskStateCellRef` (`RefMut<TaskState>`). Methods take `&mut self` directly.

Internal methods on `VirtualClockState`:

| Method | Description |
|--------|-------------|
| `new(start)` | Create clock state starting at the given instant |
| `now()` | Current virtual time |
| `epoch()` | Start time (never changes after construction) |
| `advance(duration)` | Move time forward, return `(count, Vec<Waker>)` |
| `advance_to(instant)` | Move to a specific instant, return `(count, Vec<Waker>)` |
| `register_timer(deadline, waker)` | Register a timer, returns `TimerId` |
| `cancel_timer(id)` | Cancel a previously registered timer |
| `next_deadline()` | Peek at the earliest pending timer deadline |
| `pending_timers()` | Count of pending timers |
| `set_idle_advance_fn(Option<Box<IdleAdvanceFn>>)` | Install or clear the idle advance callback |
| `has_idle_advance_fn()` | Whether an idle advance callback is installed |
| `take_idle_advance_fn()` | Extract callback for invocation (resets dirty flag) |
| `restore_idle_advance_fn(Box<IdleAdvanceFn>)` | Restore callback after invocation (checks dirty flag) |

### Re-entrancy Safety

`advance()` and `advance_to()` return the expired wakers as a `Vec<Waker>`
rather than calling `waker.wake()` internally. The caller (in `operations.rs`
or `runtime.rs`) must:

1. Hold the `TaskState` borrow while calling `advance()`.
2. Drop the `TaskState` borrow.
3. Call `waker.wake()` for each returned waker.

This prevents re-entrancy panics: when a woken future's poll re-enters
`TaskState` (e.g., via `schedule_io`), the borrow is no longer held.

### VirtualClockState Timer Wheel

`VirtualClockState` uses a `BinaryHeap<VirtualTimer>` configured as a
min-heap ordered by `(deadline, TimerId)`:

- **Deadline ordering**: Earliest-deadline timers fire first.
- **Tie-breaking**: Equal-deadline timers fire in registration order (lowest
  `TimerId` first), providing fully deterministic behavior.
- **Complexity**: O(log n) for register/cancel, O(k log n) for advance
  (where k = timers fired).
- **Waker caching**: `VirtualSleepFuture` caches the waker from the task
  context and only re-registers the timer when the waker changes. This
  preserves the original `TimerId` across spurious wakeups, maintaining
  deterministic ordering for equal-deadline timers.

### TaskState Integration

The clock is stored as `Option<VirtualClockState>` in `TaskState`, the
thread-local runtime state struct. This is accessed by:

1. **`clock_now()`** — Called by deadline I/O functions to get current time.
   Returns the virtual clock's `now()` if present, `Instant::now()` otherwise.
2. **`sleep()`** — Checks `clock.is_some()` to dispatch to `VirtualSleepFuture`.
3. **`virtual_clock_*()` operations** — Read/modify the clock via `TaskState::get()`.
4. **`VirtualSleepFuture::poll()`** — Accesses the clock via `TaskState::get()`
   to read current time and register/cancel timers.
5. **Runtime loop** — After all ready tasks are polled, if no tasks remain
   ready and an idle advance callback is installed, the runtime extracts
   the callback (via `Option::take()`), releases the `TaskStateCellRef`
   borrow (via `into_inner()`), invokes the callback with `(now, next_deadline)`,
   re-borrows, conditionally restores the callback (checking a dirty flag for
   self-replacement), and if the callback returned a non-zero duration, advances
   the clock and wakes expired timers.

The `Option` ensures zero overhead when no virtual clock is installed
(the common case in production).

## Design Decisions

### Why operations:: functions instead of a VirtualClockState handle?

Kimojio's established pattern is for control functions to live in the
`operations` module and operate through thread-local `TaskState`. Examples:
`sleep()`, `yield_io()`, `spawn_task()`, `set_activity_id_and_tenant_id()`.

Exposing virtual clock control through the same pattern:

- **Consistency**: Users don't need to learn a different API pattern.
- **Simplicity**: No clock handle to pass between test setup and async blocks.
  Tests just call `operations::virtual_clock_advance()`.
- **Encapsulation**: The `VirtualClockState` type is internal, reducing public
  API surface to a small set of clearly-named functions.
- **Isolation**: Virtual clock is only active for code that explicitly calls
  `virtual_clock_enable(true)` — there is no global configuration change.

### Why a feature flag?

The `virtual-clock` feature flag ensures:

1. **Zero compile-time overhead**: Without the feature, `clock_now()` compiles
   to a direct `Instant::now()` call. No `Option` check, no trait dispatch.
2. **No public API surface change**: Existing users see no new types or
   methods unless they opt in.
3. **Clean dependency boundary**: Test-only code stays behind a gate.
4. **Behavioral isolation**: Even with the feature enabled, behavior doesn't
   change until `virtual_clock_enable(true)` is called at runtime.

### Why store VirtualClockState directly in TaskState (no Rc wrapper)?

The original design used `Rc<RefCell<VirtualClockState>>` so that
`VirtualSleepFuture` could hold a reference to the clock independently of
`TaskState`. The current design eliminates the wrapper because:

- `VirtualSleepFuture` can access the clock via `TaskState::get()` (the
  same thread-local already used by all operations functions), eliminating
  the need for a separate clock handle.
- Direct storage avoids the `Rc` reference count, `RefCell` borrow overhead,
  and the risk of `BorrowMutError` panics from incorrect usage.
- `TaskState` already provides the correct lifetime and interior mutability
  semantics for the single-threaded runtime model.

### Why no Clock trait?

Since the `virtual-clock` feature compiles only one timing behavior at a time,
a trait with dynamic dispatch provides no value — it adds indirection and API
surface without enabling extensibility. The concrete `Option<VirtualClockState>`
approach:

- Eliminates virtual dispatch overhead entirely.
- Reduces type complexity (no `dyn Clock`, no `SystemClock`, no sealed module).
- Makes the feature's presence/absence a simple `Option` check.

### Why dual `#[cfg]` for `SleepFuture`?

`pin_project_lite` does not support `#[cfg]` attributes on enum variants.
Rather than adding a dependency on the full `pin-project` crate, we use two
separate type definitions:

- **Without feature**: Original `pin_project_lite` struct wrapping
  `UnitFuture<'a>` — identical compiled output to before.
- **With feature**: A struct wrapping an internal `SleepFutureInner` enum
  with `Real(UnitFuture)` and `Virtual(VirtualSleepFuture)` variants.
  Pin projection uses `unsafe` but is straightforward since
  `VirtualSleepFuture` is `Unpin` (enforced by a
  `static_assertions::assert_impl_all!` check). The enum variant is set at
  construction and never changes, which is the key safety invariant for the
  pin projection.

### Why `clock_now()` lives in `clock.rs`?

Placing the helper in `clock.rs` rather than `operations.rs` avoids coupling
`async_event.rs` and `async_lock.rs` to the operations module. All modules
that need current time import from the same source.

### Why `#[kimojio::test]` instead of `#[kimojio::test(virtual)]`?

Calling `virtual_clock_enable(true)` as the first line of the test body is
equivalent to the old `(virtual)` macro argument, but more explicit and
flexible. Benefits:

- **No macro complexity**: The `test` macro is a simple wrapper with no
  argument parsing.
- **Composable**: The user can add an RAII guard for cleanup, set defaults,
  or enable the clock conditionally — without any macro magic.
- **Transparent**: What the test does is visible in the test body, not hidden
  behind an attribute.

## Virtual vs Real Timer Paths

### Real path (default)

```
sleep(duration)
  → Timespec::from(duration)
  → opcode::Timeout → SQE
  → io_uring_enter → kernel timer
  → CQE with ETIME → Ready(Ok(()))
```

### Virtual path (with virtual clock enabled)

```
sleep(duration)
  → check TaskState.clock → is_some()
  → VirtualSleepFuture::new(clock.now() + duration)
  → poll: TaskState::get().clock.now() < deadline
      → register_timer → Pending
  → test: operations::virtual_clock_advance(duration)
      → advance_to: collect expired wakers
      → drop TaskState borrow
      → for w in wakers { w.wake() }
  → poll: TaskState::get().clock.now() >= deadline → Ready(Ok(()))
```

## Performance Characteristics

| Scenario | Overhead |
|----------|----------|
| Feature disabled | Zero — `clock_now()` = `Instant::now()` |
| Feature enabled, no virtual clock | One `Option::is_none()` check per `clock_now()` |
| Feature enabled, with virtual clock | `RefMut` borrow per `clock_now()` |
| Virtual sleep registration | O(log n) heap push |
| Virtual time advance | O(k log n) for k expired timers |

## Known Limitations

1. **Real I/O timeouts are not virtualized**: The `timeout` parameter on I/O
   operations becomes a kernel io_uring timeout. Virtual clock only affects
   the `Instant::now()` used to compute the remaining duration. This is
   documented on `writev_with_deadline`, `write_with_deadline`,
   `read_with_deadline`, and `wait_with_deadline`.

2. **No automatic time advancement**: The virtual clock never advances on its
   own. Test code must explicitly call `virtual_clock_advance()`.

3. **Single-threaded only**: `VirtualClockState` is accessed through
   `TaskState`, which is `!Send`/`!Sync`. `Runtime` is also `!Send` — it is
   bound to a single thread.

4. **`timeout_at` left-bias**: When both the inner future and the timeout are
   immediately ready, the inner future's result takes priority due to
   `futures::future::select` left-bias. This is documented on `timeout_at()`.

## Recently Added Features

### `poll_once()` test helper

`operations::poll_once(fut)` polls a pinned future exactly once and returns
whether it was ready. Simplifies the common poll-advance-await pattern in
virtual clock tests.

### `virtual_clock_epoch()`

Returns the start time of the clock, useful for computing elapsed virtual
time: `virtual_clock_now().duration_since(virtual_clock_epoch())`.

### `virtual_clock_set_idle_advance(callback)`

Installs a `FnMut(Instant, Option<Instant>) -> Option<Duration>` callback
that the runtime invokes when idle. The callback receives the current virtual
time and the next pending timer deadline (if any), and returns how far to
advance. If it returns `None` or `Some(Duration::ZERO)`, no advancement
occurs and the runtime blocks in io_uring. Internally the
`VirtualClockState` stores the callback as `Option<Box<IdleAdvanceFn>>`.
When the runtime's inner poll loop drains all ready tasks, it extracts the
callback (via `Option::take()` to release the borrow), invokes it outside
the `TaskState` borrow, restores the callback (unless it was replaced during
invocation via the dirty flag mechanism), then advances the clock and wakes
expired timers if the callback returned a non-zero duration.

### `virtual_clock_clear_idle_advance()`

Removes the idle advance callback. Equivalent to disabling automatic
time advancement — the runtime will block in io_uring when idle.

### `virtual_clock_has_idle_advance()`

Returns `true` if an idle advance callback is currently installed.

## Future Work

- Auto-advance mode for testing retry loops with exponential backoff (see
  `docs/virtual-clock-auto-advance-future.md` for the full design).
- Virtual clock statistics (timers registered, fired, cancelled).
- I/O-aware advancement for mixed I/O + timer scenarios.
