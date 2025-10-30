// Copyright (c) Microsoft Corporation. All rights reserved.
//! AsyncLock is an interior mutability struct appropriate for shared access to
//! data from a single thread but multiple tasks.
//!
//! It guarantees that only a single AsyncLockRef will be live in any task at
//! any given time.
//!
//! As a consequence it is safe to borrow from the AsyncLockRef both mutably and
//! immutably since the borrow lifetime will be bounded by the single
//! AsyncLockRef lifetime.
//!
//! Other tasks that attempt to access the interior data using the lock() method
//! while the lock is locked will be suspended until the lock is released.
//!
use std::{
    borrow::{Borrow, BorrowMut},
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    time::{Duration, Instant},
};

use crate::{AsyncEvent, CanceledError, TimeoutError};

pub struct AsyncLock<T> {
    value: UnsafeCell<T>,
    unlocked: AsyncEvent,
}

// Ensure that AsyncLock is always !Send and !Sync
static_assertions::const_assert!(impls::impls!(AsyncLock<()>: !Send & !Sync));

impl<T> AsyncLock<T> {
    pub fn new(value: T) -> Self {
        let unlocked = AsyncEvent::new();
        unlocked.set();

        Self {
            value: UnsafeCell::new(value),
            unlocked,
        }
    }

    /// Attempts to acquire the lock without waiting.
    /// Returns None if the lock is already held by another task.
    pub fn try_lock(&self) -> Option<AsyncLockRef<'_, T>> {
        if self.unlocked.is_set() {
            self.unlocked.reset();
            Some(AsyncLockRef { parent: self })
        } else {
            None
        }
    }

    /// Attempts to acquire the lock, waiting until the deadline.
    /// Returns None if the lock is already held by another task or the
    /// deadline is reached before the lock is acquired.
    pub async fn lock_with_deadline(
        &self,
        deadline: Option<Instant>,
    ) -> Result<AsyncLockRef<'_, T>, TimeoutError> {
        self.unlocked.wait_with_deadline(deadline).await?;

        // Due to single threaded nature of uringruntime, if we get
        // here then we know either unlocked is set, or deadline
        // has elapsed.  In either case, try_lock will return the
        // correct result.
        self.try_lock().ok_or(TimeoutError::Timeout)
    }

    /// Attempts to acquire the lock, waiting for the timeout.
    /// Returns None if the lock is already held by another task or the
    /// timeout is reached before the lock is acquired.
    pub async fn lock_with_timeout(
        &self,
        timeout: Option<Duration>,
    ) -> Result<AsyncLockRef<'_, T>, TimeoutError> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        self.lock_with_deadline(deadline).await
    }

    /// Waits for the lock to be unlocked, then marks it locked and returns an
    /// AsyncLockRef that can be used to get mutable and immutable access to the
    /// underlying data.
    pub async fn lock(&self) -> Result<AsyncLockRef<'_, T>, CanceledError> {
        while !self.unlocked.is_set() {
            self.unlocked.wait().await?;
        }
        self.unlocked.reset();
        Ok(AsyncLockRef { parent: self })
    }
}

impl<T: Default> Default for AsyncLock<T> {
    fn default() -> Self {
        AsyncLock::new(T::default())
    }
}

pub struct AsyncLockRef<'a, T> {
    parent: &'a AsyncLock<T>,
}

impl<T> Deref for AsyncLockRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: there is only one AsyncLockRef at a time
        unsafe { &*self.parent.value.get() }
    }
}

impl<T> DerefMut for AsyncLockRef<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: there is only one AsyncLockRef at a time
        unsafe { &mut *self.parent.value.get() }
    }
}

impl<T> Drop for AsyncLockRef<'_, T> {
    fn drop(&mut self) {
        self.parent.unlocked.set_wake_one();
    }
}

impl<T> Borrow<T> for AsyncLockRef<'_, T> {
    fn borrow(&self) -> &T {
        self.deref()
    }
}

impl<T> BorrowMut<T> for AsyncLockRef<'_, T> {
    fn borrow_mut(&mut self) -> &mut T {
        self.deref_mut()
    }
}

#[cfg(test)]
mod test {
    use std::{rc::Rc, time::Duration};

    use crate::{
        AsyncEvent, AsyncLock,
        operations::{self, spawn_task},
    };

    #[test]
    fn async_lock_test() {
        crate::run_test("async_lock_test", async {
            let l: Rc<AsyncLock<usize>> = Default::default();
            let mut l_ref = l.lock().await.unwrap();
            let task = {
                let l = l.clone();
                operations::spawn_task(async move {
                    let mut l_ref = l.lock().await.unwrap();
                    *l_ref += 1;
                })
            };

            for _ in 0..100 {
                operations::yield_io().await;
            }

            *l_ref = 100;
            drop(l_ref);
            task.await.unwrap();

            assert_eq!(*l.lock().await.unwrap(), 101);
        })
    }

    #[test]
    fn async_timeout_test() {
        crate::run_test("async_timeout_test", async {
            let l = Rc::new(AsyncLock::new(0));
            let l2 = l.clone();
            let ready = Rc::new(AsyncEvent::new());
            let ready2 = ready.clone();
            let done = Rc::new(AsyncEvent::new());
            let done2 = done.clone();
            let other = spawn_task(async move {
                let guard = l2.lock().await.unwrap();
                ready2.set();
                done2.wait().await.unwrap();
                drop(guard);
            });
            ready.wait().await.unwrap();
            let wait = l.lock_with_timeout(Some(Duration::from_millis(1))).await;
            assert!(wait.is_err());
            done.set();
            other.await.unwrap();
            let wait = l
                .lock_with_timeout(Some(Duration::from_millis(1)))
                .await
                .unwrap();
            assert_eq!(*wait, 0);
        })
    }

    // Parallelism is at the future level, not task level, so checking
    // current task to detect recursive lock does not work in this case.
    // This test exists to verify that any future recursive lock check
    // implementation does not regress future parallelism.
    #[test]
    fn lock_from_parallel_futures() {
        crate::run_test("lock_from_parallel_futures", async {
            let l1 = Rc::new(AsyncLock::new(0));
            let l2 = l1.clone();
            let l3 = l2.clone();
            let fut1 = async move {
                let mut guard = l1.lock().await.unwrap();
                operations::sleep(std::time::Duration::from_millis(100))
                    .await
                    .unwrap();
                *guard += 2;
                drop(guard);
            };
            let fut2 = async move {
                let mut guard = l2.lock().await.unwrap();
                *guard += 1;
                drop(guard);
            };
            futures::join!(fut1, fut2);
            assert_eq!(*l3.lock().await.unwrap(), 3);
        })
    }
}
