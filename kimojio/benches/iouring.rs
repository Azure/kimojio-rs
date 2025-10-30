// Copyright (c) Microsoft Corporation. All rights reserved.
use std::cell::Cell;
use std::collections::VecDeque;
use std::hint::black_box;
use std::rc::Rc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};

use kimojio::pipe::{bipipe, pipe};
use kimojio::{MutInPlaceCell, operations};

/**
 * Simple benchmark for the IOUring event loop
 */
pub fn benchmark(c: &mut Criterion) {
    // Iters is the number of iterations which will be run. The closure
    // must run the function under test 'iters' times and then return the
    // duration for how long that many iterations took. We use this
    // approach because the default criterion b.iter() approach would
    // fold the IOUring initialization time into the benchmark measurement
    // itself

    c.bench_function("nop raw", |b| {
        let uring = rustix_uring::IoUring::builder();
        let mut uring: rustix_uring::IoUring = uring.build(256).unwrap();
        b.iter(|| {
            let op = rustix_uring::opcode::Nop::new().build();
            unsafe { uring.submission().push(&op).unwrap() };
            uring.submit_and_wait(1).unwrap();
        })
    });

    c.bench_function("nop", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let f = async move {
                    let start = Instant::now();
                    for _ in 0..iters {
                        operations::nop().await.unwrap();
                    }
                    dur_copy.set(start.elapsed());
                };
                operations::spawn_task(f);
            });
            duration.get()
        });
    });

    c.bench_function("parallel nop", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            {
                let duration = duration.clone();
                kimojio::run(0, async move {
                    // create N tasks that iterate iters/N times for a total of iters iterations.
                    let parallel_count = 100;
                    let inner_iter = iters / parallel_count;
                    let tasks: Vec<_> = (0..parallel_count)
                        .map(|_| {
                            operations::spawn_task(async move {
                                for _ in 0..inner_iter {
                                    operations::nop().await.unwrap();
                                }
                            })
                        })
                        .collect();

                    // don't include task creation in loop
                    let start = Instant::now();
                    // now just wait for all the tasks to exit - in theory they should run and complete in lock-step
                    for task in tasks {
                        task.await.unwrap();
                    }
                    duration.set(start.elapsed());
                });
            }
            duration.get()
        });
    });

    // Benchmark a simple yield() call to measure scheduling time
    c.bench_function("yield single", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            let func = async move {
                let start = Instant::now();
                for _ in 0..iters {
                    operations::yield_io().await;
                }
                dur_copy.set(start.elapsed());
            };

            kimojio::run(0, async {
                operations::spawn_task(func);
            });
            duration.get()
        });
    });

    c.bench_function("spawn", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            let func = async move {
                let start = Instant::now();
                for _ in 0..iters {
                    operations::spawn_task(async {}).await.unwrap();
                }
                dur_copy.set(start.elapsed());
            };

            kimojio::run(0, async {
                operations::spawn_task(func);
            });
            duration.get()
        });
    });

    c.bench_function("socket oneway", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            let func = async move {
                const PORT: u16 = 59876;
                let server_sock = operations::socket(
                    operations::AddressFamily::INET,
                    operations::SocketType::STREAM,
                    None,
                )
                .await
                .unwrap();
                rustix::net::sockopt::set_socket_reuseaddr(&server_sock, true).unwrap();
                let address = std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, PORT);
                operations::bind(&server_sock, &std::net::SocketAddr::V4(address)).unwrap();
                operations::listen(&server_sock, 1).unwrap();

                let address: std::net::SocketAddr = format!("127.0.0.1:{PORT}").parse().unwrap();
                let b = operations::socket(
                    operations::AddressFamily::INET,
                    operations::SocketType::STREAM,
                    Some(operations::ipproto::TCP),
                )
                .await
                .unwrap();
                rustix::net::sockopt::set_tcp_nodelay(&b, true).unwrap();
                operations::connect(&b, &address).await.unwrap();

                let a = operations::accept(&server_sock).await.unwrap();
                rustix::net::sockopt::set_tcp_nodelay(&a, true).unwrap();

                // Actual benchmark code here to measure a one way call through a pipe
                let mut buf = [0u8; 64];
                let start = Instant::now();
                for _ in 0..iters {
                    operations::write(&a, &[0u8; 64]).await.unwrap();
                    operations::read(&b, &mut buf).await.unwrap();
                }
                dur_copy.set(start.elapsed());
            };

            kimojio::run(0, async {
                operations::spawn_task(func);
            });
            duration.get()
        });
    });

    c.bench_function("bipipe oneway", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            let (a, b) = bipipe();

            let func = async move {
                // Actual benchmark code here to measure a one way call through a pipe
                let mut buf = [0u8; 64];
                let start = Instant::now();
                for _ in 0..iters {
                    operations::write(&a, &[0u8; 64]).await.unwrap();
                    operations::read(&b, &mut buf).await.unwrap();
                }
                dur_copy.set(start.elapsed());
            };

            kimojio::run(0, async {
                operations::spawn_task(func);
            });
            duration.get()
        });
    });

    c.bench_function("pipe oneway", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            let (read, write) = pipe().unwrap();

            let func = async move {
                // Actual benchmark code here to measure a one way call through a pipe
                let mut buf = [0u8; 64];
                let start = Instant::now();
                for _ in 0..iters {
                    operations::write(&write, &[0u8; 64]).await.unwrap();
                    operations::read(&read, &mut buf).await.unwrap();
                }
                dur_copy.set(start.elapsed());
            };

            kimojio::run(0, async {
                operations::spawn_task(func);
            });
            duration.get()
        });
    });

    thread_local! {
        static COUNTER: MutInPlaceCell<VecDeque<Rc<i32>>> = MutInPlaceCell::new(VecDeque::new());
    }

    COUNTER.with(|counter| {
        counter.use_mut(|counter| counter.push_back(Rc::new(0)));
    });

    c.bench_function("thread local storage", |b| {
        b.iter(|| {
            COUNTER.with(|counter| {
                counter.use_mut(|counter| {
                    let front = black_box(counter.pop_front().unwrap());
                    counter.push_back(front);
                })
            });
        });
    });
}

criterion_group!(
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(4));
    targets=benchmark
);
criterion_main!(benches);
