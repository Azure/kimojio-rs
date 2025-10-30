// Copyright (c) Microsoft Corporation. All rights reserved.

use std::{future::Future, rc::Rc};

use crate::{
    AsyncSemaphore, CanceledError,
    operations::{TaskHandle, spawn_task},
};

/// `TaskPool` puts an upper limit on spawned tasks.  It can be
/// used to implement backpressure in a system and prevent tasks
/// from growing without limit.
#[derive(Clone)]
pub struct TaskPool {
    semaphore: Rc<AsyncSemaphore>,
}

impl TaskPool {
    /// `max_tasks` is the maximum number of tasks that can be spawned
    /// at one time.
    pub fn new(max_tasks: usize) -> Self {
        Self {
            semaphore: Rc::new(AsyncSemaphore::new(max_tasks)),
        }
    }

    /// Spawns a task.  If the maximum number of tasks has been reached,
    /// this will block until an existing task completes.
    pub async fn spawn_task<Fut>(&self, main: Fut) -> Result<TaskHandle<Fut::Output>, CanceledError>
    where
        Fut: Future + 'static,
    {
        let semaphore = self.semaphore.clone();
        semaphore.acquire().await?;
        Ok(spawn_task(async move {
            let _guard = scopeguard::guard((), |_| {
                semaphore.release();
            });

            main.await
        }))
    }
}

#[cfg(test)]
mod test {
    use std::cell::Cell;

    use super::*;
    use crate::{AsyncEvent, operations, run_test};

    #[test]
    pub fn task_pool_test_sequential() {
        run_test("task_ptask_pool_test_sequential", async {
            let started1 = Rc::new(AsyncEvent::new());
            let started2 = Rc::new(AsyncEvent::new());
            let count = Rc::new(Cell::new(0));
            let background = {
                let count = count.clone();
                let started1 = started1.clone();
                let started2 = started2.clone();
                let task_pool = TaskPool::new(1);
                spawn_task(async move {
                    let task1 = {
                        let count = count.clone();
                        let started1 = started1.clone();
                        task_pool
                            .spawn_task(async move {
                                count.set(count.get() + 1);
                                started1.wait().await.unwrap();
                            })
                            .await
                            .unwrap()
                    };
                    let task2 = {
                        let count = count.clone();
                        task_pool
                            .spawn_task(async move {
                                count.set(count.get() + 1);
                                started2.wait().await.unwrap();
                            })
                            .await
                            .unwrap()
                    };
                    let task_panic = task_pool
                        .spawn_task(async move {
                            panic!("Make sure a panicing task does not stall the poll");
                        })
                        .await
                        .unwrap();
                    let task3 = task_pool
                        .spawn_task(async move {
                            count.set(count.get() + 1);
                        })
                        .await
                        .unwrap();
                    task1.await.unwrap();
                    task2.await.unwrap();
                    assert!(task_panic.await.is_err());
                    task3.await.unwrap();
                })
            };

            async fn spin() {
                for _ in 1..=10 {
                    // spin a few times
                    operations::yield_io().await;
                }
            }

            spin().await;
            assert_eq!(count.get(), 1);
            started1.set();
            spin().await;
            assert_eq!(count.get(), 2);
            started2.set();
            spin().await;
            assert_eq!(count.get(), 3);

            background.await.unwrap();
        });
    }

    #[test]
    pub fn task_pool_test_parallel() {
        run_test("task_pool_test_parallel", async {
            let task_pool = TaskPool::new(2);
            let count = Rc::new(Cell::new(0));
            let events = (0..10)
                .map(|_| Rc::new(AsyncEvent::new()))
                .collect::<Vec<_>>();
            let mut handles = Vec::new();
            for event in &events {
                let event = event.clone();
                let count = count.clone();
                let task_pool = task_pool.clone();
                let join_handle = spawn_task(async move {
                    let count = count.clone();
                    let handle = task_pool
                        .spawn_task(async move {
                            count.set(count.get() + 1);
                            event.wait().await.unwrap();
                        })
                        .await
                        .unwrap();
                    handle.await.unwrap()
                });
                handles.push(join_handle);
            }

            async fn spin() {
                for _ in 1..=10 {
                    // spin a few times
                    operations::yield_io().await;
                }
            }

            let mut expected_count = 2;
            for event in &events {
                event.set();
                spin().await;
                expected_count = std::cmp::min(expected_count + 1, events.len());
                assert_eq!(expected_count, count.get());
            }

            for handle in handles {
                handle.await.unwrap();
            }
        });
    }
}
