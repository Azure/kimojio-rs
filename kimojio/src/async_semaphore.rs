// Copyright (c) Microsoft Corporation. All rights reserved.
//! AsyncSemaphore is a counter representing "tokens" you can acquire. If the
//! count is zero you have to wait.  AsyncSemaphore is not scoped based as it is
//! intended for more complex scenarios. You have to be careful to acquire and
//! releas when appropriate. Unlike for locks, acquire and release do not
//! necessarily need to be balanced depending on your use case.
//!
//! If you need balanced access to resources protected by scopes, use AsyncLock
//! or AsyncReaderWriterLock.
//!
//! # Usage
//!
//! ```
//! async fn semaphore_example() {
//!     let tokens = std::rc::Rc::new(kimojio::AsyncSemaphore::new(3));
//!     tokens.acquire().await;
//!     tokens.release();
//! }
//! ```
//!
use crate::{CanceledError, async_event::AsyncEvent};
use std::cell::Cell;

pub struct AsyncSemaphore {
    tokens: Cell<usize>,
    available: AsyncEvent,
}

// Ensure that AsyncSemaphore is always !Send and !Sync
static_assertions::const_assert!(impls::impls!(AsyncSemaphore: !Send & !Sync));

impl AsyncSemaphore {
    pub fn new(initial_count: usize) -> Self {
        let available = AsyncEvent::new();
        if initial_count > 0 {
            available.set();
        }
        Self {
            tokens: Cell::new(initial_count),
            available,
        }
    }

    /// Wait until at least one token is available and then grab it by
    /// decreasing token count by one.
    pub async fn acquire(&self) -> Result<(), CanceledError> {
        while self.tokens.get() == 0 {
            self.available.wait().await?;
        }
        let count = self.tokens.get() - 1;
        self.tokens.set(count);
        if count == 0 {
            self.available.reset();
        }
        Ok(())
    }

    // Increase token count by one, and wake up an waiting acquirer if any.
    pub fn release(&self) {
        self.tokens.set(self.tokens.get() + 1);
        self.available.set_wake_one();
    }
}

#[cfg(test)]
mod test {
    use std::{cell::Cell, rc::Rc};

    use crate::{AsyncEvent, AsyncSemaphore, operations, run_test};

    #[test]
    fn semaphore_test() {
        run_test("semaphore_test", async {
            let semaphore = Rc::new(AsyncSemaphore::new(3));
            let counter = Rc::new(Cell::new(0i32));

            let mut tasks = Vec::new();
            for _ in 0..4 {
                let semaphore = semaphore.clone();
                let counter = counter.clone();
                let started = Rc::new(AsyncEvent::new());
                let started_copy = started.clone();
                tasks.push(operations::spawn_task(async move {
                    started_copy.set();
                    semaphore.acquire().await.unwrap();
                    counter.set(counter.get() + 1);
                }));

                started.wait().await.unwrap();
            }

            assert_eq!(counter.get(), 3);
            semaphore.release();

            for task in tasks {
                task.await.unwrap();
            }

            assert_eq!(counter.get(), 4);
        })
    }
}
