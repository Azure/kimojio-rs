// Copyright (c) Microsoft Corporation. All rights reserved.

use std::cell::Cell;
use std::hint::black_box;
use std::rc::Rc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};

use kimojio::{AsyncEvent, AsyncLock, async_channel, operations};

pub fn benchmark(c: &mut Criterion) {
    c.bench_function("async_event_set", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let event = AsyncEvent::new();
                let start = Instant::now();
                for _ in 0..iters {
                    event.set();
                }
                dur_copy.set(start.elapsed());
            });

            duration.get()
        });
    });

    c.bench_function("async_event_ping_pong", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let ping = Rc::new(AsyncEvent::new());
                let pong = Rc::new(AsyncEvent::new());

                let task = {
                    let ping = ping.clone();
                    let pong = pong.clone();
                    operations::spawn_task(async move {
                        for _ in 0..iters {
                            ping.wait().await.unwrap();
                            ping.reset();
                            pong.set();
                        }
                    })
                };

                let start = Instant::now();
                for _ in 0..iters {
                    ping.set();
                    pong.wait().await.unwrap();
                    pong.reset();
                }
                dur_copy.set(start.elapsed());

                task.await.unwrap();
            });
            duration.get()
        });
    });

    c.bench_function("async_lock", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let lock = AsyncLock::new(0usize);
                let start = Instant::now();
                for _ in 0..iters {
                    let guard = black_box(lock.lock().await.unwrap());
                    drop(guard)
                }
                dur_copy.set(start.elapsed());
            });
            duration.get()
        });
    });

    c.bench_function("async_channel", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let (tx, rx) = async_channel();
                let start = Instant::now();
                for i in 0..iters {
                    tx.send(i).await.expect("channel closed");
                    let _result = black_box(rx.recv().await);
                }
                dur_copy.set(start.elapsed());
            });
            duration.get()
        });
    });

    c.bench_function("async_channel_wait", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let (ping_tx, ping_rx) = async_channel();
                let (pong_tx, pong_rx) = async_channel();

                let task = {
                    operations::spawn_task(async move {
                        for _ in 0..iters {
                            let value = ping_rx.recv().await.unwrap();
                            pong_tx.send(value).await.expect("channel closed");
                        }
                    })
                };

                let start = Instant::now();
                for i in 0..iters {
                    ping_tx.send(i).await.expect("channel closed");
                    let _ignored = black_box(pong_rx.recv().await.unwrap());
                }
                dur_copy.set(start.elapsed());

                task.await.unwrap();
            });
            duration.get()
        });
    });

    // This benchmark function helps us to understand the overshot introduced by uringruntime's async sleep operation
    // on microsecond scale.
    c.bench_function("async_sleep_overshot", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let start = Instant::now();
                for _i in 0..iters {
                    // By default, measuring the overshot of 700 us because 700 us is the average latency of SSDv2,
                    // which is the target latency cap we want to set for walfs.
                    operations::sleep(Duration::from_micros(700)).await.unwrap();
                }
                dur_copy.set(start.elapsed());
            });
            duration.get()
        });
    });
}

criterion_group!(
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(4));
    targets=benchmark
);
criterion_main!(benches);
