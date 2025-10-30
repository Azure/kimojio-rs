// Copyright (c) Microsoft Corporation. All rights reserved.
use std::{cell::Cell, collections::VecDeque, rc::Rc, time::Instant};

use crate::{AsyncEvent, ChannelError, MutInPlaceCell};

/// Creates a new unbounded channel. The channel can be used to send messages
/// between tasks. Sending never blocks. The channel is closed when the sender
/// is dropped or can be closed explicitly by the sender. Closing a channel
/// drops any messages that are in the channel.
pub fn async_channel_unbounded<T>() -> (SenderUnbounded<T>, ReceiverUnbounded<T>) {
    let inner = Rc::new(AsyncChannelUnbounded::new());
    (
        SenderUnbounded {
            inner: inner.clone(),
        },
        ReceiverUnbounded { inner },
    )
}

/// Creates a new unbounded channel with a specified capacity. The channel can
/// be used to send messages between tasks. Sending never blocks. The channel is
/// closed when the sender is dropped or can be closed explicitly by the sender.
/// Closing a channel drops any messages that are in the channel.
pub fn async_channel_unbounded_with_capacity<T>(
    capacity: usize,
) -> (SenderUnbounded<T>, ReceiverUnbounded<T>) {
    let inner = Rc::new(AsyncChannelUnbounded::with_capacity(capacity));
    (
        SenderUnbounded {
            inner: inner.clone(),
        },
        ReceiverUnbounded { inner },
    )
}

/// A sender that can be used to send messages to an unbounded channel. If the
/// sender is dropped, the channel will be closed and any messages that are in
/// the channel will be dropped.
pub struct SenderUnbounded<T> {
    inner: Rc<AsyncChannelUnbounded<T>>,
}

impl<T> SenderUnbounded<T> {
    /// Sends a message to the channel. Sending never blocks, but if the channel
    /// is closed, the message will be returned.
    pub fn send(&self, message: T) -> Result<(), T> {
        self.inner.send(message)
    }

    /// Closes the channel. Any further attempts to send messages will return an
    /// error. Any messages that are already in the channel will be dropped.
    pub fn close(&self) {
        self.inner.close()
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the number of messages in the channel.
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<T> Clone for SenderUnbounded<T> {
    fn clone(&self) -> Self {
        let inner = self.inner.clone();
        inner.increment_sender_count();
        Self { inner }
    }
}

impl<T> Drop for SenderUnbounded<T> {
    fn drop(&mut self) {
        self.inner.decrement_sender_count()
    }
}

/// A receiver that can be used to receive messages from an unbounded channel.
pub struct ReceiverUnbounded<T> {
    inner: Rc<AsyncChannelUnbounded<T>>,
}

impl<T> ReceiverUnbounded<T> {
    /// Attempts to receive a message from the channel without blocking.
    /// If the channel is closed, returns `ChannelClosedError`.
    pub fn try_recv(&self) -> Result<Option<T>, ChannelError> {
        self.inner.try_recv()
    }

    /// Receives a message from the channel, blocking until a message is available
    /// or the channel is closed.
    pub async fn recv(&self) -> Result<T, ChannelError> {
        self.inner.recv(None).await
    }

    /// Receives a message from the channel, blocking until a message is available,
    /// the channel is closed, or the deadline is reached.
    pub async fn recv_with_deadline(&self, deadline: Option<Instant>) -> Result<T, ChannelError> {
        self.inner.recv(deadline).await
    }
}

struct ChannelQueue<T> {
    items: VecDeque<T>,
    closed: bool,
}

struct AsyncChannelUnbounded<T> {
    queue: MutInPlaceCell<ChannelQueue<T>>,
    has_items: AsyncEvent,
    sender_count: Cell<usize>,
}

// Ensure that AsyncChannelUnbounded is always !Send and !Sync
static_assertions::const_assert!(impls::impls!(AsyncChannelUnbounded<()>: !Send & !Sync));

impl<T> AsyncChannelUnbounded<T> {
    fn new() -> Self {
        Self::with_capacity(0)
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            queue: MutInPlaceCell::new(ChannelQueue {
                items: VecDeque::with_capacity(capacity),
                closed: false,
            }),
            has_items: AsyncEvent::new(),
            sender_count: Cell::new(1),
        }
    }

    fn send(&self, message: T) -> std::result::Result<(), T> {
        self.push_back(message)?;
        self.has_items.set();
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.queue.use_mut(|queue| queue.items.is_empty())
    }

    fn len(&self) -> usize {
        self.queue.use_mut(|queue| queue.items.len())
    }

    fn try_recv(&self) -> std::result::Result<Option<T>, ChannelError> {
        self.pop_front()
    }

    async fn recv(&self, deadline: Option<Instant>) -> std::result::Result<T, ChannelError> {
        loop {
            if let Some(value) = self.pop_front()? {
                return Ok(value);
            }

            self.has_items.wait_with_deadline(deadline).await?;
        }
    }

    fn close(&self) {
        self.queue.use_mut(|queue| queue.closed = true);
        self.has_items.set();
    }

    fn push_back(&self, message: T) -> std::result::Result<(), T> {
        self.queue.use_mut(|queue| {
            queue.items.push_back(message);
            Ok(())
        })
    }

    fn pop_front(&self) -> std::result::Result<Option<T>, ChannelError> {
        self.queue
            .use_mut(|queue| match (queue.closed, queue.items.pop_front()) {
                (_, Some(item)) => Ok(Some(item)),
                (true, _) => Err(ChannelError::Closed),
                _ => {
                    self.has_items.reset();
                    Ok(None)
                }
            })
    }

    fn increment_sender_count(&self) {
        let sender_count = self.sender_count.get() + 1;
        self.sender_count.set(sender_count);
    }

    fn decrement_sender_count(&self) {
        let sender_count = self.sender_count.get() - 1;
        self.sender_count.set(sender_count);

        if sender_count == 0 {
            self.close();
        }
    }
}

impl<T> Default for AsyncChannelUnbounded<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Creates a new channel. The channel can be used to send messages between
/// tasks. Sending blocks until the message is received. The channel is closed
/// when the sender is dropped or can be closed explicitly by the sender.
///
/// There is no buffering so attempting to send a message when a previous
/// message has not been received blocks.
pub fn async_channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Rc::new(AsyncChannel::new());
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner },
    )
}

/// A sender that can be used to send messages to a channel. If the sender is
/// dropped, the channel will be closed.
pub struct Sender<T> {
    inner: Rc<AsyncChannel<T>>,
}

impl<T> Sender<T> {
    /// Sends a message to the channel. Sending blocks until the previous
    /// message has been received. In other words, only one message can be
    /// in flight in the channel at a time.  If the channel is closed, the
    /// message will be returned.
    pub async fn send(&self, message: T) -> Result<(), T> {
        self.inner.send(message).await
    }

    pub fn try_send(&self, message: T) -> Result<(), SendError<T>> {
        self.inner.try_send(message)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        let inner = self.inner.clone();
        inner.increment_sender_count();
        Self { inner }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.inner.decrement_sender_count();
    }
}

/// A receiver that can be used to receive messages from a channel.
pub struct Receiver<T> {
    inner: Rc<AsyncChannel<T>>,
}

impl<T> Receiver<T> {
    /// Attempts to receive a message from the channel without blocking.
    pub fn try_recv(&self) -> Result<Option<T>, ChannelError> {
        self.inner.try_recv()
    }

    /// Receives a message from the channel, blocking until a message is available
    /// or the channel is closed.
    pub async fn recv(&self) -> Result<T, ChannelError> {
        self.inner.recv().await
    }
}
// Receiver should not be Clone unless the Drop implementation is updated
// to support that.
static_assertions::const_assert!(impls::impls!(Receiver<()>: !Clone));

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner.close();
    }
}

struct AsyncChannelValue<T> {
    value: Option<T>,
    closed: bool,
}

struct AsyncChannel<T> {
    item: MutInPlaceCell<AsyncChannelValue<T>>,
    has_item: AsyncEvent,
    sender_count: Cell<usize>,
}

// Ensure that AsyncChannel is always !Send and !Sync
static_assertions::const_assert!(impls::impls!(AsyncChannel<()>: !Send & !Sync));

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum SendError<T> {
    ChannelClosed(T),
    ChannelFull(T),
}

impl<T> AsyncChannel<T> {
    fn new() -> Self {
        Self {
            item: MutInPlaceCell::new(AsyncChannelValue {
                value: None,
                closed: false,
            }),
            has_item: AsyncEvent::new(),
            sender_count: Cell::new(1),
        }
    }

    async fn send(&self, mut message: T) -> Result<(), T> {
        loop {
            match self.try_send(message) {
                Ok(()) => return Ok(()),
                Err(SendError::ChannelFull(m)) => {
                    message = m;
                    if let Err(_canceled_error) = self.has_item.wait_reset().await {
                        return Err(message);
                    }
                }
                Err(SendError::ChannelClosed(m)) => return Err(m),
            }
        }
    }

    fn try_send(&self, message: T) -> Result<(), SendError<T>> {
        self.item.use_mut(|item| {
            if !item.closed {
                if item.value.is_none() {
                    item.value = Some(message);
                    Ok(())
                } else {
                    Err(SendError::ChannelFull(message))
                }
            } else {
                Err(SendError::ChannelClosed(message))
            }
        })?;
        self.has_item.set();
        Ok(())
    }

    fn try_recv(&self) -> Result<Option<T>, ChannelError> {
        self.item.use_mut(|item| {
            if let Some(value) = item.value.take() {
                self.has_item.reset();
                Ok(Some(value))
            } else if !item.closed {
                Ok(None)
            } else {
                Err(ChannelError::Closed)
            }
        })
    }

    async fn recv(&self) -> Result<T, ChannelError> {
        loop {
            if let Some(value) = self.try_recv()? {
                return Ok(value);
            }

            self.has_item.wait().await?;
        }
    }

    fn close(&self) {
        self.item.use_mut(|item| item.closed = true);
        self.has_item.set();
    }

    fn increment_sender_count(&self) {
        let sender_count = self.sender_count.get() + 1;
        self.sender_count.set(sender_count);
    }

    fn decrement_sender_count(&self) {
        let sender_count = self.sender_count.get() - 1;
        self.sender_count.set(sender_count);

        if sender_count == 0 {
            self.close();
        }
    }
}

impl<T> Default for AsyncChannel<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod test {
    use std::{future::Future, rc::Rc};

    use super::{
        AsyncChannel, ChannelError, SendError, async_channel, async_channel_unbounded,
        async_channel_unbounded_with_capacity,
    };
    use crate::operations;

    #[derive(Default)]
    struct AsyncBiChannel<T> {
        channel1: Rc<AsyncChannel<T>>,
        channel2: Rc<AsyncChannel<T>>,
    }

    impl<T> AsyncBiChannel<T> {
        pub fn new() -> (Self, Self) {
            let channel1 = Rc::new(AsyncChannel::default());
            let channel2 = Rc::new(AsyncChannel::default());
            (
                Self {
                    channel1: channel1.clone(),
                    channel2: channel2.clone(),
                },
                Self {
                    channel1: channel2,
                    channel2: channel1,
                },
            )
        }

        pub fn send(&self, message: T) -> impl Future<Output = Result<(), T>> + '_ {
            self.channel1.send(message)
        }
        pub fn recv(&self) -> impl Future<Output = Result<T, ChannelError>> + '_ {
            self.channel2.recv()
        }
        pub fn try_recv(&self) -> Result<Option<T>, ChannelError> {
            self.channel2.try_recv()
        }
    }

    #[test]
    pub fn channel_test() {
        crate::run_test("channel_test", async {
            let (client, server) = AsyncBiChannel::new();
            let t1 = {
                operations::spawn_task(async move {
                    loop {
                        let message = server.recv().await.unwrap();
                        if message > 0 {
                            server.send(message + 1).await.expect("channel closed");
                        } else {
                            break;
                        }
                    }
                })
            };

            for message in 1..10 {
                client.send(message).await.expect("channel closed");
                let response = client.recv().await.unwrap();
                assert_eq!(response, message + 1);
            }

            client.send(-1).await.expect("channel closed");
            t1.await.unwrap();
        });
    }

    #[test]
    pub fn channel_test_try() {
        crate::run_test("channel_test", async {
            let (client, server) = AsyncBiChannel::new();
            let t1 = {
                operations::spawn_task(async move {
                    loop {
                        let message =
                            if let Some(message) = server.try_recv().expect("channel closed") {
                                message
                            } else {
                                server.recv().await.unwrap()
                            };
                        if message > 0 {
                            server.send(message + 1).await.expect("channel closed");
                        } else {
                            break;
                        }
                    }
                })
            };

            for message in 1..10 {
                for _ in 0..2 {
                    client.send(message).await.expect("channel closed");
                }
                for _ in 0..2 {
                    let response =
                        if let Some(response) = client.try_recv().expect("channel closed") {
                            response
                        } else {
                            client.recv().await.unwrap()
                        };
                    assert_eq!(response, message + 1);
                }
            }

            client.send(-1).await.expect("channel closed");
            t1.await.unwrap();
        });
    }

    #[test]
    pub fn unbounded_channel_test() {
        crate::run_test("unbounded_channel_test", async {
            let (channel1_tx, channel1_rx) = async_channel_unbounded();
            let (channel2_tx, channel2_rx) = async_channel_unbounded();
            let t1 = {
                operations::spawn_task(async move {
                    loop {
                        let message = channel1_rx.recv().await.unwrap();
                        if message > 0 {
                            channel2_tx.send(message + 1).unwrap();
                        } else {
                            break;
                        }
                    }
                })
            };

            for message in 1..10 {
                channel1_tx.send(message).unwrap();
                let response = channel2_rx.recv().await.unwrap();
                assert_eq!(response, message + 1);
            }

            for message in 1..10 {
                channel1_tx.send(message).unwrap();
                channel1_tx.send(10 * message).unwrap();
                let response = channel2_rx.recv().await.unwrap();
                assert_eq!(response, message + 1);
                let response = channel2_rx.recv().await.unwrap();
                assert_eq!(response, 10 * message + 1);
            }

            channel1_tx.send(-1).unwrap();
            t1.await.unwrap();
        });
    }

    #[test]
    pub fn test_recv_when_sender_dropped() {
        crate::run_test("test_recv_when_sender_dropped", async {
            let (channel1_tx, channel1_rx) = async_channel_unbounded();
            channel1_tx.send(1).unwrap();
            channel1_tx.send(2).unwrap();
            drop(channel1_tx);

            assert_eq!(1, channel1_rx.recv().await.unwrap());
            assert_eq!(2, channel1_rx.recv().await.unwrap());
            assert_eq!(Err(ChannelError::Closed), channel1_rx.recv().await);
        })
    }
    #[test]
    pub fn test_send_when_receiver_dropped() {
        crate::run_test("test_send_when_receiver_dropped", async {
            let (channel1_tx, channel1_rx) = async_channel();
            drop(channel1_rx);
            assert_eq!(Err(SendError::ChannelClosed(3)), channel1_tx.try_send(3));
        })
    }

    #[test]
    pub fn test_async_channel_recv_when_sender_dropped() {
        crate::run_test("test_async_channel_recv_when_sender_dropped", async {
            let (channel1_tx, channel1_rx) = async_channel();
            channel1_tx.send(1).await.unwrap();
            drop(channel1_tx);

            assert_eq!(1, channel1_rx.recv().await.unwrap());
            assert_eq!(Err(ChannelError::Closed), channel1_rx.recv().await);

            let (channel1_tx, channel1_rx) = async_channel::<i32>();
            drop(channel1_tx);

            assert_eq!(Err(ChannelError::Closed), channel1_rx.recv().await);
        })
    }

    #[test]
    pub fn test_async_channel_unbounded_with_capacity() {
        crate::run_test("test_async_channel_unbounded_with_capacity", async {
            let (tx, rx) = async_channel_unbounded_with_capacity(5);

            // Test that it works like a normal unbounded channel
            for i in 1..=10 {
                tx.send(i).unwrap();
            }

            for i in 1..=10 {
                let received = rx.recv().await.unwrap();
                assert_eq!(received, i);
            }
        })
    }

    #[test]
    pub fn test_sender_unbounded_close_and_status() {
        crate::run_test("test_sender_unbounded_close_and_status", async {
            let (tx, rx) = async_channel_unbounded();

            // Test is_empty and len when empty
            assert!(tx.is_empty());
            assert_eq!(tx.len(), 0);

            // Send some messages
            tx.send(1).unwrap();
            tx.send(2).unwrap();
            tx.send(3).unwrap();

            // Test is_empty and len with items
            assert!(!tx.is_empty());
            assert_eq!(tx.len(), 3);

            // Receive one message
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg, 1);
            assert_eq!(tx.len(), 2);

            // Test close functionality
            tx.close();

            // Should still be able to receive existing messages
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg, 2);
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg, 3);

            // Now should get channel closed error
            assert_eq!(Err(ChannelError::Closed), rx.recv().await);

            // Note: Unbounded channels may still allow sending after close()
            // but will be closed when all senders are dropped
        })
    }

    #[test]
    pub fn test_receiver_unbounded_try_recv() {
        crate::run_test("test_receiver_unbounded_try_recv", async {
            let (tx, rx) = async_channel_unbounded();

            // Should return None when empty
            assert_eq!(Ok(None), rx.try_recv());

            // Send a message
            tx.send(42).unwrap();

            // Should receive the message
            assert_eq!(Ok(Some(42)), rx.try_recv());

            // Should return None again when empty
            assert_eq!(Ok(None), rx.try_recv());

            // Close the channel
            tx.close();

            // Should return ChannelError::Closed
            assert_eq!(Err(ChannelError::Closed), rx.try_recv());
        })
    }

    #[test]
    pub fn test_receiver_unbounded_recv_with_deadline() {
        crate::run_test("test_receiver_unbounded_recv_with_deadline", async {
            let (tx, rx) = async_channel_unbounded();

            // Test timeout
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(10);
            let result = rx.recv_with_deadline(Some(deadline)).await;
            assert!(result.is_err());

            // Test successful receive with deadline
            tx.send(123).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
            let result = rx.recv_with_deadline(Some(deadline)).await.unwrap();
            assert_eq!(result, 123);

            // Test no deadline (should behave like normal recv)
            let send_task = operations::spawn_task(async move {
                tx.send(456).unwrap();
            });

            let recv_task = operations::spawn_task(async move {
                let result = rx.recv_with_deadline(None).await.unwrap();
                assert_eq!(result, 456);
            });

            send_task.await.unwrap();
            recv_task.await.unwrap();
        })
    }

    #[test]
    pub fn test_sender_unbounded_clone() {
        crate::run_test("test_sender_unbounded_clone", async {
            let (tx, rx) = async_channel_unbounded();
            let tx_clone = tx.clone();

            // Both senders should work
            tx.send(1).unwrap();
            tx_clone.send(2).unwrap();

            // Receive both messages
            assert_eq!(1, rx.recv().await.unwrap());
            assert_eq!(2, rx.recv().await.unwrap());

            // Drop original sender
            drop(tx);

            // Clone should still work
            tx_clone.send(3).unwrap();
            assert_eq!(3, rx.recv().await.unwrap());

            // Drop clone - now channel should close
            drop(tx_clone);
            assert_eq!(Err(ChannelError::Closed), rx.recv().await);
        })
    }

    #[test]
    pub fn test_sender_clone() {
        crate::run_test("test_sender_clone", async {
            let (tx, rx) = async_channel();
            let tx_clone = tx.clone();

            // Test that both senders work
            let send_task1 = operations::spawn_task(async move {
                tx.send(1).await.unwrap();
            });

            let send_task2 = operations::spawn_task(async move {
                tx_clone.send(2).await.unwrap();
            });

            let recv_task = operations::spawn_task(async move {
                let msg1 = rx.recv().await.unwrap();
                let msg2 = rx.recv().await.unwrap();

                // Messages could arrive in any order
                let mut msgs = vec![msg1, msg2];
                msgs.sort();
                assert_eq!(msgs, vec![1, 2]);
            });

            send_task1.await.unwrap();
            send_task2.await.unwrap();
            recv_task.await.unwrap();
        })
    }

    #[test]
    pub fn test_sender_clone_drop_behavior() {
        crate::run_test("test_sender_clone_drop_behavior", async {
            let (tx, rx) = async_channel();
            let tx_clone = tx.clone();

            // Send message with first sender and receive it
            tx.send(1).await.unwrap();
            assert_eq!(1, rx.recv().await.unwrap());

            // Drop original sender
            drop(tx);

            // Clone should still work
            tx_clone.send(2).await.unwrap();
            assert_eq!(2, rx.recv().await.unwrap());

            // Now drop clone - channel should close
            drop(tx_clone);

            // Channel should now be closed
            assert_eq!(Err(ChannelError::Closed), rx.recv().await);
        })
    }

    #[test]
    pub fn test_multiple_sender_unbounded_clones() {
        crate::run_test("test_multiple_sender_unbounded_clones", async {
            let (tx, rx) = async_channel_unbounded();

            // Create multiple clones
            let tx1 = tx.clone();
            let tx2 = tx.clone();
            let tx3 = tx.clone();

            // All should be able to send
            tx.send(1).unwrap();
            tx1.send(2).unwrap();
            tx2.send(3).unwrap();
            tx3.send(4).unwrap();

            // Receive all messages
            let mut received = Vec::new();
            for _ in 0..4 {
                received.push(rx.recv().await.unwrap());
            }
            received.sort();
            assert_eq!(received, vec![1, 2, 3, 4]);

            // Drop all but one sender
            drop(tx);
            drop(tx1);
            drop(tx2);

            // Last sender should still work
            tx3.send(5).unwrap();
            assert_eq!(5, rx.recv().await.unwrap());

            // Drop last sender
            drop(tx3);
            assert_eq!(Err(ChannelError::Closed), rx.recv().await);
        })
    }

    #[test]
    pub fn test_channel_try_send_error_variants() {
        crate::run_test("test_channel_try_send_error_variants", async {
            let (tx, rx) = async_channel();

            // First send should succeed
            assert_eq!(Ok(()), tx.try_send(1));

            // Second send should fail with ChannelFull
            match tx.try_send(2) {
                Err(SendError::ChannelFull(2)) => {}
                other => panic!("Expected ChannelFull(2), got {other:?}"),
            }

            // Receive the first message
            assert_eq!(1, rx.recv().await.unwrap());

            // Now second send should succeed
            assert_eq!(Ok(()), tx.try_send(2));

            // Drop receiver
            drop(rx);

            // Now send should fail with ChannelClosed
            match tx.try_send(3) {
                Err(SendError::ChannelClosed(3)) => {}
                other => panic!("Expected ChannelClosed(3), got {other:?}"),
            }
        })
    }
}
