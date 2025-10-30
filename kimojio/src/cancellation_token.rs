// Copyright (c) Microsoft Corporation. All rights reserved.

use crate::{AsyncEvent, async_event::WaitAsyncEventFuture};

/// CancellationToken can be used to track when an async
/// operation should be cancelled.
///
/// The async task can poll the cancellation token using
/// the is_cancelled method to check if it should cancel,
/// or alternatively can await the cancelled method to
/// discover cancellation.
///
/// # Usage
///
/// ```
/// use futures::FutureExt;
/// use std::time::Duration;
/// use kimojio:: operations::sleep;
///
/// async fn cancellation_examples() {
///     let cancellation_token = kimojio::CancellationToken::new();
///
///     // Cancel the token
///     cancellation_token.cancel();
///
///     // wait for cancellation
///     let result = futures::select! {
///         // this will complete first if the cancellation token is has been cancelled
///         _ = cancellation_token.cancelled() => "cancelled",
///
///         // this simulates the work you want to do
///         _ = sleep(Duration::from_secs(1)).fuse() => "finished the work",
///     };
/// }
///
/// ```
#[derive(Debug, Default)]
pub struct CancellationToken {
    event: AsyncEvent,
}

impl CancellationToken {
    /// Creates a new uncancelled cancellation token.
    pub fn new() -> Self {
        let event = AsyncEvent::new();
        Self { event }
    }

    /// Returns true if `cancel()` has ever been called, and false if it has
    /// never been called on this instance.
    pub fn is_cancelled(&self) -> bool {
        self.event.is_set()
    }

    /// Marks the cancellation token as cancelled. Any subsequent calls to
    /// `is_cancelled()` will return true, pending `cancelled()` calls will
    /// complete, and any future calls to `cancelled()` will return immediately.
    pub fn cancel(&self) {
        self.event.set();
    }

    /// `cancelled()`` will return only when the cancellation token has ben
    /// canceled by the `cancel()` call.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe.
    pub fn cancelled(&self) -> WaitAsyncEventFuture<'_> {
        self.event.wait()
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use futures::select;

    use super::*;

    #[test]
    fn test_cancellation_token() {
        crate::run_test("test_cancellation_token", async {
            // Test simple
            let token = CancellationToken::new();
            assert!(!token.is_cancelled());
            token.cancel();
            assert!(token.is_cancelled());

            // Test cancel from another task
            let token = Rc::new(CancellationToken::new());
            let task = {
                let token = token.clone();
                crate::operations::spawn_task(async move { token.cancel() })
            };

            assert!(!token.is_cancelled());
            token.cancelled().await.unwrap();
            assert!(token.is_cancelled());
            task.await.unwrap();
        })
    }

    #[test]
    fn test_cancel_select() {
        crate::run_test("test_cancel_select", async {
            let token = CancellationToken::new();

            let result = select! {
                _ = token.cancelled() => "cancelled",
                default => "not cancelled",
            };
            assert_eq!("not cancelled", result);

            token.cancel();

            let result = select! {
                _ = token.cancelled() => "cancelled",
                default => "not cancelled",
            };
            assert_eq!("cancelled", result);
        })
    }
}
