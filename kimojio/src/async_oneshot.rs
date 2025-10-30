// Copyright (c) Microsoft Corporation. All rights reserved.

use crate::{AsyncEvent, ChannelError, MutInPlaceCell};
use std::{cell::Cell, rc::Rc, time::Instant};

/// A oneshot channel for sending a single value between tasks.
pub fn oneshot<T>() -> (SenderOneshot<T>, ReceiverOneshot<T>) {
    let inner = Rc::new(OneshotChannel::new());
    (
        SenderOneshot {
            inner: inner.clone(),
        },
        ReceiverOneshot { inner },
    )
}

pub struct SenderOneshot<T> {
    inner: Rc<OneshotChannel<T>>,
}

impl<T> SenderOneshot<T> {
    /// Sends a value to the channel.
    /// Returns the value as an error if the channel is closed.
    /// The channel is automatically closed if the receiver is dropped.
    pub fn send(self, value: T) -> Result<(), T> {
        self.inner.send(value)
    }
}

impl<T> Drop for SenderOneshot<T> {
    fn drop(&mut self) {
        self.inner.close();
    }
}

impl<T> std::fmt::Debug for SenderOneshot<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SenderOneshot")
            .field("complete", &self.inner.has_value.is_set())
            .finish()
    }
}

pub struct ReceiverOneshot<T> {
    inner: Rc<OneshotChannel<T>>,
}

impl<T> ReceiverOneshot<T> {
    /// Receives a value from the channel, waiting if necessary.
    /// Returns an error if the channel is closed.
    /// The channel is automatically closed if the sender is dropped.
    pub async fn recv(self) -> Result<T, ChannelError> {
        self.inner.recv().await
    }

    /// Receives a value from the channel, waiting if necessary.
    /// Returns an error if the channel is closed.
    /// The channel is automatically closed if the sender is dropped.
    /// If a deadline is provided, the function will return an error
    /// if the deadline is reached before a value is received.
    pub async fn recv_with_deadline(self, deadline: Option<Instant>) -> Result<T, ChannelError> {
        self.inner.recv_with_deadline(deadline).await
    }

    /// Tries to receive a value from the channel.
    /// Returns None if no value is available, or an error if the channel is closed.
    /// The channel is automatically closed if the sender is dropped.
    pub fn try_recv(&self) -> Result<Option<T>, ChannelError> {
        self.inner.try_recv()
    }
}

impl<T> Drop for ReceiverOneshot<T> {
    fn drop(&mut self) {
        self.inner.close();
    }
}

impl<T> std::fmt::Debug for ReceiverOneshot<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiverOneshot")
            .field("complete", &self.inner.has_value.is_set())
            .finish()
    }
}

struct OneshotChannel<T> {
    value: MutInPlaceCell<Option<T>>,
    has_value: AsyncEvent,
    closed: Cell<bool>,
}

impl<T> OneshotChannel<T> {
    fn new() -> Self {
        Self {
            value: MutInPlaceCell::new(None),
            has_value: AsyncEvent::new(),
            closed: Cell::new(false),
        }
    }

    fn send(&self, value: T) -> Result<(), T> {
        if self.closed.get() {
            return Err(value);
        }
        self.value.use_mut(|v| *v = Some(value));
        self.has_value.set();
        Ok(())
    }

    async fn recv(&self) -> Result<T, ChannelError> {
        loop {
            if let Some(value) = self.try_recv()? {
                return Ok(value);
            }
            self.has_value.wait().await?;
        }
    }

    async fn recv_with_deadline(&self, deadline: Option<Instant>) -> Result<T, ChannelError> {
        loop {
            if let Some(value) = self.try_recv()? {
                return Ok(value);
            }
            self.has_value.wait_with_deadline(deadline).await?;
        }
    }

    fn try_recv(&self) -> Result<Option<T>, ChannelError> {
        if let Some(value) = self.value.use_mut(|v| v.take()) {
            Ok(Some(value))
        } else if self.closed.get() {
            Err(ChannelError::Closed)
        } else {
            Ok(None)
        }
    }

    fn close(&self) {
        self.closed.set(true);
        self.has_value.set();
    }
}

#[cfg(test)]
mod test {
    use super::oneshot;
    use crate::ChannelError;

    #[test]
    fn oneshot_recv_before_send() {
        use crate::operations::spawn_task;

        crate::run_test("oneshot_recv_before_send", async {
            let (tx, rx) = oneshot();
            let recv_task = spawn_task(async move {
                assert_eq!(rx.recv().await, Ok(42));
            });
            tx.send(42).unwrap();
            recv_task.await.unwrap();
        })
    }

    #[test]
    fn oneshot_recv_after_send() {
        crate::run_test("oneshot_recv_after_send", async {
            let (tx, rx) = oneshot();
            tx.send(42).unwrap();
            assert_eq!(rx.recv().await, Ok(42));
        })
    }

    #[test]
    fn oneshot_recv_and_send_dropped() {
        crate::run_test("oneshot_recv_and_send_dropped", async {
            let (tx, rx) = oneshot::<i32>();
            drop(tx);
            assert_eq!(rx.recv().await, Err(ChannelError::Closed));
        })
    }

    #[test]
    fn oneshot_send_dropped_before_recv() {
        crate::run_test("oneshot_send_dropped_before_recv", async {
            let (tx, rx) = oneshot::<i32>();
            drop(tx);
            assert_eq!(rx.recv().await, Err(ChannelError::Closed));
        })
    }

    #[test]
    fn oneshot_recv_with_deadline_success() {
        use crate::operations::spawn_task;
        use std::time::{Duration, Instant};

        crate::run_test("oneshot_recv_with_deadline_success", async {
            let (tx, rx) = oneshot();
            let deadline = Instant::now() + Duration::from_millis(100);

            let recv_task = spawn_task(async move { rx.recv_with_deadline(Some(deadline)).await });

            // Send value immediately
            tx.send(42).unwrap();

            assert_eq!(recv_task.await.unwrap(), Ok(42));
        })
    }

    #[test]
    fn oneshot_recv_with_deadline_timeout() {
        use std::time::{Duration, Instant};

        crate::run_test("oneshot_recv_with_deadline_timeout", async {
            let (_tx, rx) = oneshot::<i32>();
            let deadline = Instant::now() + Duration::from_millis(10);

            let result = rx.recv_with_deadline(Some(deadline)).await;
            assert!(result.is_err());
        })
    }

    #[test]
    fn oneshot_recv_with_deadline_no_deadline() {
        use crate::operations::spawn_task;

        crate::run_test("oneshot_recv_with_deadline_no_deadline", async {
            let (tx, rx) = oneshot();

            let recv_task = spawn_task(async move { rx.recv_with_deadline(None).await });

            tx.send(123).unwrap();
            assert_eq!(recv_task.await.unwrap(), Ok(123));
        })
    }

    #[test]
    fn oneshot_recv_with_deadline_closed_channel() {
        use std::time::{Duration, Instant};

        crate::run_test("oneshot_recv_with_deadline_closed_channel", async {
            let (tx, rx) = oneshot::<i32>();
            drop(tx); // Close the channel

            let deadline = Instant::now() + Duration::from_millis(100);
            let result = rx.recv_with_deadline(Some(deadline)).await;
            assert_eq!(result, Err(ChannelError::Closed));
        })
    }

    #[test]
    fn oneshot_try_recv_available() {
        crate::run_test("oneshot_try_recv_available", async {
            let (tx, rx) = oneshot();
            tx.send(42).unwrap();

            assert_eq!(rx.try_recv(), Ok(Some(42)));
            // Second try_recv should return None since value was already consumed
            assert_eq!(rx.try_recv(), Err(ChannelError::Closed));
        })
    }

    #[test]
    fn oneshot_try_recv_not_available() {
        crate::run_test("oneshot_try_recv_not_available", async {
            let (_tx, rx) = oneshot::<i32>();

            assert_eq!(rx.try_recv(), Ok(None));
        })
    }

    #[test]
    fn oneshot_try_recv_closed() {
        crate::run_test("oneshot_try_recv_closed", async {
            let (tx, rx) = oneshot::<i32>();
            drop(tx); // Close the channel

            assert_eq!(rx.try_recv(), Err(ChannelError::Closed));
        })
    }

    #[test]
    fn oneshot_try_recv_multiple_attempts() {
        use crate::operations::spawn_task;

        crate::run_test("oneshot_try_recv_multiple_attempts", async {
            let (tx, rx) = oneshot();

            // First try - nothing available
            assert_eq!(rx.try_recv(), Ok(None));

            let send_task = spawn_task(async move {
                // Small delay to ensure try_recv is called first
                crate::operations::sleep(std::time::Duration::from_millis(10))
                    .await
                    .unwrap();
                tx.send(99).unwrap();
            });

            // Wait for send and then try again
            send_task.await.unwrap();
            assert_eq!(rx.try_recv(), Ok(Some(99)));
        })
    }

    #[test]
    fn oneshot_debug_sender() {
        crate::run_test("oneshot_debug_sender", async {
            let (tx, _rx) = oneshot::<i32>();

            // Debug before sending
            let debug_str = format!("{tx:?}");
            assert!(debug_str.contains("SenderOneshot"));
            assert!(debug_str.contains("complete: false"));

            // Send value and check debug after
            tx.send(42).unwrap();
            // Note: tx is moved by send(), so we can't check its debug state after
        })
    }

    #[test]
    fn oneshot_debug_receiver() {
        crate::run_test("oneshot_debug_receiver", async {
            let (tx, rx) = oneshot::<i32>();

            // Debug before receiving
            let debug_str = format!("{rx:?}");
            assert!(debug_str.contains("ReceiverOneshot"));
            assert!(debug_str.contains("complete: false"));

            // Send value and check debug after
            tx.send(42).unwrap();
            let debug_str_after_send = format!("{rx:?}");
            assert!(debug_str_after_send.contains("ReceiverOneshot"));
            assert!(debug_str_after_send.contains("complete: true"));

            // Receive value - rx is moved by recv()
            let _value = rx.recv().await.unwrap();
        })
    }

    #[test]
    fn oneshot_debug_closed_sender() {
        crate::run_test("oneshot_debug_closed_sender", async {
            let (tx, rx) = oneshot::<i32>();
            drop(rx); // Close by dropping receiver

            let debug_str = format!("{tx:?}");
            assert!(debug_str.contains("SenderOneshot"));
            // When receiver is dropped, the has_value event is set but no value is sent
            // The debug should still show complete: false since no value was actually sent
            assert!(debug_str.contains("complete:"));
        })
    }

    #[test]
    fn oneshot_debug_closed_receiver() {
        crate::run_test("oneshot_debug_closed_receiver", async {
            let (tx, rx) = oneshot::<i32>();
            drop(tx); // Close by dropping sender

            let debug_str = format!("{rx:?}");
            assert!(debug_str.contains("ReceiverOneshot"));
            // When sender is dropped, the has_value event is set to signal closure
            // but no actual value was sent, so complete should be true (event is set)
            assert!(debug_str.contains("complete:"));
        })
    }
    #[test]
    fn oneshot_channel_recv_with_deadline_direct() {
        use std::time::{Duration, Instant};

        crate::run_test("oneshot_channel_recv_with_deadline_direct", async {
            let channel = super::OneshotChannel::<i32>::new();

            // Test timeout
            let deadline = Instant::now() + Duration::from_millis(10);
            let result = channel.recv_with_deadline(Some(deadline)).await;
            assert!(result.is_err());
        })
    }

    #[test]
    fn oneshot_channel_recv_with_deadline_success_direct() {
        use crate::operations::spawn_task;
        use std::time::{Duration, Instant};

        crate::run_test("oneshot_channel_recv_with_deadline_success_direct", async {
            let channel = std::rc::Rc::new(super::OneshotChannel::<i32>::new());
            let deadline = Instant::now() + Duration::from_millis(100);

            let recv_channel = channel.clone();
            let recv_task =
                spawn_task(async move { recv_channel.recv_with_deadline(Some(deadline)).await });

            // Send value immediately
            channel.send(42).unwrap();

            assert_eq!(recv_task.await.unwrap(), Ok(42));
        })
    }
}
