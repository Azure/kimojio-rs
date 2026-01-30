// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! AsyncReaderWriterLock is an interior mutability struct appropriate for
//! shared access to data from a single thread but multiple tasks.
//!
//! It guarantess that only a single WriteRef will be live in any task at any
//! given time or that many ReadRef may be live across many tasks at any given
//! time. There will never be a live WriteRef if there is any live ReadRef, but
//! there may be many ReadRef if there is no WriteRef.
//!
//! Only WriteRef gives mutable access, so the saftey guarantees are similar to
//! AsyncLock.
//!
//! For ReadRef, you can only get const reference access.  You can work around
//! that by creating an AsyncReaderWriterLock around a RefCell, but then it
//! would be your responsbility to be careful to make sure that you only borrow
//! in a way that does not conflict. This is the same restriction that always
//! exists for RefCell.
//!

use crate::{CanceledError, async_event::AsyncEvent};
use std::cell::{Cell, UnsafeCell};
use std::ops::{Deref, DerefMut};

/// A reader-writer lock for single-threaded, multi-task environments.
///
/// Allows either multiple readers or a single writer to access the data.
/// Readers get shared immutable access, while writers get exclusive mutable access.
pub struct AsyncReaderWriterLock<T> {
    data: UnsafeCell<T>,
    readers: Cell<usize>,
    writer: Cell<bool>,
    read_event: AsyncEvent,
    write_event: AsyncEvent,
}

// Ensure that AsyncReaderWriterLock is always !Send and !Sync
static_assertions::const_assert!(impls::impls!(AsyncReaderWriterLock<()>: !Send & !Sync));

impl<T> AsyncReaderWriterLock<T> {
    /// Creates a new `AsyncReaderWriterLock` containing the given value.
    pub fn new(value: T) -> Self {
        let read_event = AsyncEvent::new();
        read_event.set();
        let write_event = AsyncEvent::new();
        write_event.set();
        Self {
            data: UnsafeCell::new(value),
            readers: Cell::new(0),
            writer: Cell::new(false),
            read_event,
            write_event,
        }
    }

    /// Wait until there are no readers or writers and then return a WriteRef
    /// that will allow mutable or immutable access as long as it is live.
    pub async fn lock_write(&self) -> Result<WriteRef<'_, T>, CanceledError> {
        self.assert_valid();
        while self.readers.get() > 0 || self.writer.get() {
            self.write_event.wait().await?;
        }
        self.assert_valid();
        self.writer.set(true);
        self.write_event.reset();
        self.read_event.reset();
        self.assert_valid();
        Ok(WriteRef { parent: self })
    }

    /// Waits until there are no writers, then returns a `ReadRef` for shared access.
    ///
    /// Multiple readers can hold `ReadRef` simultaneously, but no writer can
    /// acquire a `WriteRef` while any `ReadRef` exists.
    pub async fn lock_read(&self) -> Result<ReadRef<'_, T>, CanceledError> {
        self.assert_valid();
        while self.writer.get() {
            self.read_event.wait().await?;
        }
        self.assert_valid();
        self.readers.set(self.readers.get() + 1);
        self.write_event.reset();
        self.assert_valid();
        Ok(ReadRef { parent: self })
    }

    fn assert_valid(&self) {
        // either there is no writer (and any readers), or a writer and no
        // readers
        debug_assert!(!self.writer.get() || self.readers.get() == 0);
    }
}

impl<T: Default> Default for AsyncReaderWriterLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// A guard providing exclusive write access to data protected by `AsyncReaderWriterLock`.
pub struct WriteRef<'a, T> {
    parent: &'a AsyncReaderWriterLock<T>,
}

impl<T> Deref for WriteRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: Only one WriteRef can exist at a time
        unsafe { &*self.parent.data.get() }
    }
}

impl<T> DerefMut for WriteRef<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: Only one WriteRef can exist at a time
        unsafe { &mut *self.parent.data.get() }
    }
}

impl<T> Drop for WriteRef<'_, T> {
    fn drop(&mut self) {
        let parent = self.parent;
        parent.assert_valid();
        parent.writer.set(false);
        if parent.write_event.any_waiting() {
            parent.write_event.set_wake_one();
            parent.read_event.set_wake_n(0);
        } else {
            parent.write_event.set();
            parent.read_event.set();
        }
        parent.assert_valid();
    }
}

/// A guard providing shared read access to data protected by `AsyncReaderWriterLock`.
pub struct ReadRef<'a, T> {
    parent: &'a AsyncReaderWriterLock<T>,
}

impl<'a, T> ReadRef<'a, T> {
    /// Upgrades this read lock to a write lock, waiting if necessary.
    ///
    /// The read lock is released before acquiring the write lock.
    pub async fn upgrade(self) -> Result<WriteRef<'a, T>, CanceledError> {
        self.parent.readers.set(self.parent.readers.get() - 1);
        let result = self.parent.lock_write().await?;
        std::mem::forget(self);
        Ok(result)
    }
}

impl<T> Deref for ReadRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: If there is any ReadRef then there is no WriteRef.
        unsafe { &*self.parent.data.get() }
    }
}

impl<T> Drop for ReadRef<'_, T> {
    fn drop(&mut self) {
        let parent = self.parent;
        parent.assert_valid();
        let readers = parent.readers.get() - 1;
        parent.readers.set(readers);
        if readers == 0 {
            parent.write_event.set();
        }
        parent.assert_valid();
    }
}

#[cfg(test)]
mod test {
    use std::{cell::Cell, collections::HashSet, ops::AsyncFnOnce, rc::Rc};

    use crate::{
        AsyncEvent, AsyncReaderWriterLock,
        operations::{self, TaskHandle},
    };

    async fn spawn(
        lock: &Rc<AsyncReaderWriterLock<i32>>,
        f: impl AsyncFnOnce(&AsyncReaderWriterLock<i32>) + 'static,
    ) -> TaskHandle<()> {
        let lock = lock.clone();
        let e1 = Rc::new(AsyncEvent::new());
        let e2 = e1.clone();
        let handle = operations::spawn_task(async move {
            e2.set();
            f(&lock).await;
        });
        e1.wait().await.unwrap();
        handle
    }

    #[crate::test]
    async fn readers_block_writer_test() {
        let lock = Rc::new(AsyncReaderWriterLock::new(0i32));
        let reader = lock.lock_read().await.unwrap();
        let t1 = spawn(&lock, async move |lock| {
            let mut ptr = lock.lock_write().await.unwrap();
            *ptr = 5;
        })
        .await;

        assert_eq!(*reader, 0);

        {
            //nested reader
            assert_eq!(*lock.lock_read().await.unwrap(), 0);
        }
        drop(reader);
        t1.await.unwrap();

        assert_eq!(*lock.lock_read().await.unwrap(), 5);
    }

    #[crate::test]
    async fn writer_blocks_readers_test() {
        let lock = Rc::new(AsyncReaderWriterLock::new(0i32));
        let counter = Rc::new(Cell::new(0i32));
        let writer = lock.lock_write().await.unwrap();
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let counter = counter.clone();
            tasks.push(
                spawn(&lock, async move |lock| {
                    lock.lock_read().await.unwrap();
                    counter.set(counter.get() + 1);
                })
                .await,
            );
        }

        assert_eq!(counter.get(), 0);
        drop(writer);

        let expected_len = tasks.len() as i32;

        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(counter.get(), expected_len);
    }

    #[crate::test]
    async fn writer_blocks_writer_test() {
        let lock: Rc<AsyncReaderWriterLock<i32>> = Default::default();
        let mut writer = lock.lock_write().await.unwrap();
        *writer = 4;

        let t1 = spawn(&lock, async move |lock| {
            let mut ptr = lock.lock_write().await.unwrap();
            *ptr = 5;
        })
        .await;

        assert_eq!(*writer, 4);

        drop(writer);
        t1.await.unwrap();

        assert_eq!(*lock.lock_read().await.unwrap(), 5);
    }

    #[crate::test]
    async fn simple_upgrade() {
        let lock = Rc::new(AsyncReaderWriterLock::new(0i32));
        let read = lock.lock_read().await.unwrap();
        let mut write = read.upgrade().await.unwrap();
        *write = 5;
        drop(write);

        assert_eq!(*lock.lock_read().await.unwrap(), 5);
    }

    #[crate::test]
    async fn many_readers_can_upgrade() {
        let lock = Rc::new(AsyncReaderWriterLock::new(HashSet::new()));
        let mut readies = Vec::new();
        let proceed = Rc::new(AsyncEvent::new());
        let mut indices = HashSet::new();

        let mut tasks = Vec::new();
        for index in 0..10 {
            indices.insert(index);
            let lock = lock.clone();
            let ready = Rc::new(AsyncEvent::new());
            readies.push(ready.clone());
            let proceed = proceed.clone();
            tasks.push(operations::spawn_task(async move {
                let reader = lock.lock_read().await.unwrap();
                ready.set();
                proceed.wait().await.unwrap();
                let mut writer = reader.upgrade().await.unwrap();
                writer.insert(index);
            }));
        }

        for r in readies {
            r.wait().await.unwrap();
        }
        proceed.set();
        for task in tasks {
            task.await.unwrap();
        }
        let results = lock.lock_read().await.unwrap();
        assert_eq!(&*results, &indices);
    }
}
