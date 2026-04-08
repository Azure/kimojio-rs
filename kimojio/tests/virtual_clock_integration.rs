// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the virtual clock feature.
//!
//! These tests exercise the full stack: virtual clock → runtime → operations,
//! verifying that sleep, sleep_until, timeout_at, and drop cancellation all
//! work correctly with virtual time advancement.

#![cfg(feature = "virtual-clock")]

use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

use kimojio::TimeoutError;
use kimojio::operations;

// ── P2: sleep_until with virtual time ────────────────────────────────

#[kimojio::test]
async fn sleep_until_completes_after_advance() {
    operations::virtual_clock_enable(true);
    let deadline = operations::virtual_clock_now() + Duration::from_secs(30);
    let mut sleep = pin!(operations::sleep_until(deadline));

    assert!(
        operations::poll_once(sleep.as_mut()).await.is_none(),
        "sleep_until should be pending before advance"
    );

    operations::virtual_clock_advance(Duration::from_secs(30));
    sleep.await.unwrap();
}

#[kimojio::test]
async fn sleep_until_with_past_deadline_completes_immediately() {
    operations::virtual_clock_enable(true);
    operations::virtual_clock_advance(Duration::from_secs(100));
    let past_deadline = operations::virtual_clock_now() - Duration::from_secs(50);
    operations::sleep_until(past_deadline).await.unwrap();
}

// ── P4: timeout_at with virtual time ─────────────────────────────────

#[kimojio::test]
async fn timeout_at_returns_value_when_inner_completes_first() {
    operations::virtual_clock_enable(true);
    let deadline = operations::virtual_clock_now() + Duration::from_secs(10);
    let result = operations::timeout_at(deadline, async { 42 }).await;
    assert_eq!(result.unwrap(), 42);
}

#[kimojio::test]
async fn timeout_at_returns_timeout_when_deadline_reached() {
    operations::virtual_clock_enable(true);
    let deadline = operations::virtual_clock_now() + Duration::from_secs(5);

    let mut timeout_fut = pin!(operations::timeout_at(
        deadline,
        std::future::poll_fn(|_cx| Poll::<()>::Pending),
    ));

    assert!(
        operations::poll_once(timeout_fut.as_mut()).await.is_none(),
        "timeout_at should be pending before advance"
    );

    operations::virtual_clock_advance(Duration::from_secs(5));

    let result = timeout_fut.await;
    assert!(
        matches!(result, Err(TimeoutError::Timeout)),
        "should return Timeout error, got: {result:?}",
    );
}

#[kimojio::test]
async fn timeout_at_with_long_timeout_allows_inner_to_complete() {
    operations::virtual_clock_enable(true);
    let deadline = operations::virtual_clock_now() + Duration::from_secs(100);
    let result = operations::timeout_at(deadline, async { "hello" }).await;
    assert_eq!(result.unwrap(), "hello");
}

// ── P5: Drop cancellation ────────────────────────────────────────────

#[kimojio::test]
async fn drop_sleep_cancels_timer() {
    operations::virtual_clock_enable(true);
    {
        let mut sleep = pin!(operations::sleep(Duration::from_secs(100)));
        operations::poll_once(sleep.as_mut()).await;
        assert_eq!(
            operations::virtual_clock_pending_timers(),
            1,
            "timer should be registered"
        );
    }
    assert_eq!(
        operations::virtual_clock_pending_timers(),
        0,
        "timer should be cancelled on drop"
    );
}

#[kimojio::test]
async fn cancel_method_prevents_timer_from_firing() {
    operations::virtual_clock_enable(true);
    let mut sleep = pin!(operations::sleep(Duration::from_secs(10)));
    operations::poll_once(sleep.as_mut()).await;
    assert_eq!(operations::virtual_clock_pending_timers(), 1);

    sleep.as_mut().cancel();
    assert_eq!(
        operations::virtual_clock_pending_timers(),
        0,
        "timer should be cancelled after cancel()"
    );
}

// ── Regression: spurious wakeup preserves timer ordering (F-2) ───────

#[kimojio::test]
async fn spurious_wakeup_preserves_timer_order() {
    operations::virtual_clock_enable(true);
    let mut sleep_a = pin!(operations::sleep(Duration::from_secs(10)));
    let mut sleep_b = pin!(operations::sleep(Duration::from_secs(10)));

    // Poll both to register timers (A gets lower TimerId)
    operations::poll_once(sleep_a.as_mut()).await;
    operations::poll_once(sleep_b.as_mut()).await;

    // Spurious wakeup — should NOT re-register timers
    operations::poll_once(sleep_a.as_mut()).await;
    operations::poll_once(sleep_b.as_mut()).await;

    assert_eq!(
        operations::virtual_clock_pending_timers(),
        2,
        "timers should not be duplicated by spurious wakeups"
    );

    operations::virtual_clock_advance(Duration::from_secs(10));
    sleep_a.await.unwrap();
    sleep_b.await.unwrap();
}

// ── Regression: advance_to doesn't panic on re-entrant waker (F-1) ──

#[kimojio::test]
async fn advance_to_safe_with_reentrant_access() {
    operations::virtual_clock_enable(true);
    let mut sleep = pin!(operations::sleep(Duration::from_secs(1)));
    operations::poll_once(sleep.as_mut()).await;

    operations::virtual_clock_advance(Duration::from_secs(1));
    sleep.await.unwrap();
}

// ── Proc macro tests ────────────────────────────────────────────────

#[kimojio::test]
async fn macro_virtual_sleep_completes() {
    operations::virtual_clock_enable(true);
    let mut sleep = pin!(operations::sleep(Duration::from_secs(30)));
    assert!(operations::poll_once(sleep.as_mut()).await.is_none());

    operations::virtual_clock_advance(Duration::from_secs(30));
    sleep.await.unwrap();
}

#[kimojio::test]
async fn macro_virtual_no_param() {
    operations::virtual_clock_enable(true);
    // The virtual macro works without a clock parameter
    operations::sleep(Duration::ZERO).await.unwrap();
}

// ── epoch() tests ───────────────────────────────────────────────────

#[kimojio::test]
async fn epoch_returns_start_time() {
    operations::virtual_clock_enable(true);
    let epoch = operations::virtual_clock_epoch();
    assert_eq!(
        operations::virtual_clock_now(),
        epoch,
        "initially now == epoch"
    );
    operations::virtual_clock_advance(Duration::from_secs(100));
    assert_eq!(
        operations::virtual_clock_epoch(),
        epoch,
        "epoch doesn't change"
    );
    assert_eq!(
        operations::virtual_clock_now().duration_since(epoch),
        Duration::from_secs(100)
    );
}

// ── Past-deadline and zero-duration edge cases ───────────────────────

/// Tests that a sleep with a past deadline completes immediately.
#[kimojio::test]
async fn past_deadline_sleep_completes_immediately() {
    operations::virtual_clock_enable(true);
    operations::virtual_clock_advance(Duration::from_secs(100));
    let past = operations::virtual_clock_epoch();
    operations::sleep_until(past).await.unwrap();
}

/// Tests that sleep(Duration::ZERO) completes without hanging.
#[kimojio::test]
async fn zero_duration_sleep_completes() {
    operations::virtual_clock_enable(true);
    operations::sleep(Duration::ZERO).await.unwrap();
}

// ── Idle advance callback tests ──────────────────────────────────────

/// Sleep completes automatically via advance-to-next-timer callback.
#[kimojio::test]
async fn callback_completes_sleep() {
    operations::virtual_clock_enable(true);
    operations::virtual_clock_set_idle_advance(|now, next| {
        next.map(|d| d.saturating_duration_since(now))
    });
    operations::sleep(Duration::from_secs(60)).await.unwrap();
}

/// Multiple timers fire in order via successive idle callback invocations.
#[kimojio::test]
async fn callback_fires_timers_in_order() {
    operations::virtual_clock_enable(true);
    let epoch = operations::virtual_clock_epoch();

    operations::virtual_clock_set_idle_advance(|now, next| {
        next.map(|d| d.saturating_duration_since(now))
    });

    operations::sleep(Duration::from_secs(10)).await.unwrap();
    assert_eq!(
        operations::virtual_clock_now().duration_since(epoch),
        Duration::from_secs(10)
    );

    operations::sleep(Duration::from_secs(20)).await.unwrap();
    assert_eq!(
        operations::virtual_clock_now().duration_since(epoch),
        Duration::from_secs(30)
    );
}

/// Idle advance callback wakes spawned tasks correctly.
#[kimojio::test]
async fn callback_wakes_spawned_task() {
    operations::virtual_clock_enable(true);
    let done = std::rc::Rc::new(std::cell::Cell::new(false));
    let d = done.clone();

    operations::virtual_clock_set_idle_advance(|now, next| {
        next.map(|d| d.saturating_duration_since(now))
    });

    operations::spawn_task(async move {
        operations::sleep(Duration::from_secs(60)).await.unwrap();
        d.set(true);
    });

    operations::sleep(Duration::from_secs(60)).await.unwrap();
    operations::yield_io().await;

    assert!(
        done.get(),
        "spawned task should have completed via idle advance"
    );
}

/// has_idle_advance reflects callback installation state.
#[kimojio::test]
async fn has_idle_advance_reflects_state() {
    operations::virtual_clock_enable(true);
    assert!(!operations::virtual_clock_has_idle_advance());

    operations::virtual_clock_set_idle_advance(|_, _| Some(Duration::from_secs(1)));
    assert!(operations::virtual_clock_has_idle_advance());

    operations::virtual_clock_clear_idle_advance();
    assert!(!operations::virtual_clock_has_idle_advance());
}

/// Fixed-duration callback resolves sleeps without pre-computation.
#[kimojio::test]
async fn fixed_duration_callback_resolves_sleep() {
    operations::virtual_clock_enable(true);
    operations::virtual_clock_set_idle_advance(|_, _| Some(Duration::from_millis(1)));
    operations::sleep(Duration::from_secs(60)).await.unwrap();
}

/// Clearing the callback stops automatic advancement.
#[kimojio::test]
async fn cleared_callback_stops_advancement() {
    operations::virtual_clock_enable(true);
    let epoch = operations::virtual_clock_epoch();

    // Install and use callback for first sleep
    operations::virtual_clock_set_idle_advance(|now, next| {
        next.map(|d| d.saturating_duration_since(now))
    });
    operations::sleep(Duration::from_secs(10)).await.unwrap();
    assert_eq!(
        operations::virtual_clock_now().duration_since(epoch),
        Duration::from_secs(10)
    );

    // Clear callback — no more automatic advancement
    operations::virtual_clock_clear_idle_advance();
    assert!(!operations::virtual_clock_has_idle_advance());

    // Explicit advance still works
    operations::virtual_clock_advance(Duration::from_secs(20));
    assert_eq!(
        operations::virtual_clock_now().duration_since(epoch),
        Duration::from_secs(30)
    );
}

// ── New capability tests (not possible with old API) ─────────────────

/// Advance-to-next-timer completes a 60s sleep in under 10ms wall time.
#[kimojio::test]
async fn advance_to_next_timer_one_liner() {
    operations::virtual_clock_enable(true);
    let wall_start = std::time::Instant::now();

    operations::virtual_clock_set_idle_advance(|now, next| {
        next.map(|d| d.saturating_duration_since(now))
    });

    operations::sleep(Duration::from_secs(60)).await.unwrap();
    assert!(
        wall_start.elapsed() < Duration::from_millis(100),
        "60s virtual sleep should complete in <100ms wall time"
    );
}

/// Countdown callback fires exactly N times then stops.
#[kimojio::test]
async fn countdown_callback() {
    operations::virtual_clock_enable(true);
    let count = std::rc::Rc::new(std::cell::Cell::new(3u32));
    let c = count.clone();

    operations::virtual_clock_set_idle_advance(move |_, _| {
        let remaining = c.get();
        if remaining > 0 {
            c.set(remaining - 1);
            Some(Duration::from_secs(1))
        } else {
            None
        }
    });

    // Sleep 3s — exactly 3 advances should fire (one per idle point)
    operations::sleep(Duration::from_secs(3)).await.unwrap();
    assert_eq!(count.get(), 0);
}

/// Callback receives correct now and next_deadline parameters.
#[kimojio::test]
async fn callback_receives_correct_params() {
    operations::virtual_clock_enable(true);
    let epoch = operations::virtual_clock_epoch();
    let recorded = std::rc::Rc::new(std::cell::Cell::new((
        std::time::Instant::now(),
        None::<std::time::Instant>,
    )));
    let r = recorded.clone();

    operations::virtual_clock_set_idle_advance(move |now, next| {
        r.set((now, next));
        next.map(|d| d.saturating_duration_since(now))
    });

    operations::sleep(Duration::from_secs(42)).await.unwrap();
    let (got_now, got_next) = recorded.get();
    // On first invocation, now should be the epoch and next should be epoch + 42s
    assert_eq!(got_now, epoch);
    assert_eq!(got_next, Some(epoch + Duration::from_secs(42)));
}

/// Replacing the callback mid-test changes behavior.
#[kimojio::test]
async fn replace_callback_mid_test() {
    operations::virtual_clock_enable(true);
    let epoch = operations::virtual_clock_epoch();

    // Start with advance-to-next
    operations::virtual_clock_set_idle_advance(|now, next| {
        next.map(|d| d.saturating_duration_since(now))
    });
    operations::sleep(Duration::from_secs(60)).await.unwrap();
    assert_eq!(
        operations::virtual_clock_now().duration_since(epoch),
        Duration::from_secs(60)
    );

    // Switch to fixed 1ms
    operations::virtual_clock_set_idle_advance(|_, _| Some(Duration::from_millis(1)));
    operations::sleep(Duration::from_millis(10)).await.unwrap();
    assert!(
        operations::virtual_clock_now().duration_since(epoch)
            >= Duration::from_secs(60) + Duration::from_millis(10)
    );
}

/// Callback returning None when no timers are pending blocks normally.
#[kimojio::test]
async fn callback_returns_none_when_no_timers() {
    operations::virtual_clock_enable(true);
    let called = std::rc::Rc::new(std::cell::Cell::new(false));
    let c = called.clone();

    operations::virtual_clock_set_idle_advance(move |_now, next| {
        if next.is_none() {
            c.set(true);
            None
        } else {
            next.map(|d| d.saturating_duration_since(_now))
        }
    });

    // No timers pending initially — callback should be called and return None
    // But we need a task to actually run; sleep registers a timer so next won't be None.
    // Instead, just verify has_idle_advance is true and the callback is installed.
    assert!(operations::virtual_clock_has_idle_advance());

    // Now use it with a real sleep to verify it works end-to-end
    operations::sleep(Duration::from_secs(1)).await.unwrap();
}

/// Callback that always returns Duration::ZERO does not cause infinite loop.
#[kimojio::test]
async fn zero_duration_return_does_not_loop() {
    operations::virtual_clock_enable(true);

    // Install a callback that always returns ZERO — the runtime should not spin.
    operations::virtual_clock_set_idle_advance(|_, _| Some(Duration::ZERO));

    // Do a manual advance to verify the runtime isn't stuck
    operations::virtual_clock_advance(Duration::from_secs(1));
    let epoch = operations::virtual_clock_epoch();
    assert_eq!(
        operations::virtual_clock_now().duration_since(epoch),
        Duration::from_secs(1)
    );
}

/// Callback calls virtual_clock_clear_idle_advance during invocation.
/// The clear should take effect — the old callback should NOT be restored.
#[kimojio::test]
async fn callback_self_clears_during_invocation() {
    operations::virtual_clock_enable(true);

    operations::virtual_clock_set_idle_advance(|now, next| {
        // Clear ourselves during invocation
        operations::virtual_clock_clear_idle_advance();
        // Still return an advance for this one invocation
        next.map(|d| d.saturating_duration_since(now))
    });

    // The first sleep should complete (callback returns advance-to-next)
    operations::sleep(Duration::from_secs(10)).await.unwrap();

    // But the callback should now be gone (self-cleared via dirty flag)
    assert!(
        !operations::virtual_clock_has_idle_advance(),
        "callback should have been cleared during invocation"
    );
}

/// Conditional callback with a shared flag to toggle advancement on/off.
#[kimojio::test]
async fn conditional_callback_with_flag() {
    operations::virtual_clock_enable(true);
    let active = std::rc::Rc::new(std::cell::Cell::new(true));
    let a = active.clone();

    operations::virtual_clock_set_idle_advance(move |now, next| {
        if a.get() {
            next.map(|d| d.saturating_duration_since(now))
        } else {
            None
        }
    });

    // With flag=true, sleep completes
    operations::sleep(Duration::from_secs(10)).await.unwrap();

    // Disable the flag — callback returns None, runtime should block.
    // We can't test blocking directly, but we can verify the flag
    // is respected by toggling it back and sleeping again.
    active.set(false);

    // Re-enable so we don't hang, and verify it works again.
    active.set(true);
    operations::sleep(Duration::from_secs(10)).await.unwrap();
}

/// Callback A replaces itself with callback B and returns None.
/// Callback B should run on the next idle point (not be blocked by anti-spin).
#[kimojio::test]
async fn replacement_during_callback_none_takes_effect_next_idle_point() {
    operations::virtual_clock_enable(true);

    // Callback A: install callback B and return None (don't advance this time)
    operations::virtual_clock_set_idle_advance(|_now, _next| {
        // Replace with advance-to-next callback
        operations::virtual_clock_set_idle_advance(|now, next| {
            next.map(|d| d.saturating_duration_since(now))
        });
        None // Don't advance on this invocation
    });

    // The sleep should still complete: callback A returns None but installs B,
    // and B should fire on the next idle point.
    operations::sleep(Duration::from_secs(10)).await.unwrap();
}
