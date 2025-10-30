// Copyright (c) Microsoft Corporation. All rights reserved.
use std::cell::Cell;
use std::io::IoSlice;
use std::rc::Rc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};

use kimojio::pipe::bipipe;
use kimojio::{AsyncStreamRead, AsyncStreamWrite, OwnedFdStream};

pub fn benchmark(c: &mut Criterion) {
    c.bench_function("async_stream", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let (client, server) = bipipe();
                let mut stream1 = OwnedFdStream::new(client);
                let mut stream2 = OwnedFdStream::new(server);

                let start = Instant::now();
                for iter in 0..iters {
                    stream1.write(&iter.to_le_bytes(), None).await.unwrap();
                    let mut buffer = [0; 8];
                    stream2.read(&mut buffer, None).await.unwrap();
                }
                dur_copy.set(start.elapsed());

                stream1.shutdown().await.unwrap();
                stream2.shutdown().await.unwrap();
            });
            duration.get()
        });
    });

    c.bench_function("manual_channel_stream", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let (client, server) = bipipe();
                let mut stream1 = OwnedFdStream::new(client);
                let mut stream2 = OwnedFdStream::new(server);

                let start = Instant::now();
                for iter in 0..iters {
                    let id = 1u32;
                    let length = 12u32;
                    let length_bytes = length.to_le_bytes();
                    let id_bytes = id.to_le_bytes();
                    let iter_bytes = iter.to_le_bytes();
                    let iov = &mut [
                        IoSlice::new(&length_bytes),
                        IoSlice::new(&id_bytes),
                        IoSlice::new(&iter_bytes),
                    ];
                    stream1.writev(iov, None).await.unwrap();

                    let mut response = [0; 128];
                    stream2.read(&mut response[0..4], None).await.unwrap();
                    let length = u32::from_le_bytes(response[0..4].try_into().unwrap()) as usize;
                    stream2.read(&mut response[0..length], None).await.unwrap();
                    let id = u32::from_le_bytes(response[0..4].try_into().unwrap()) as usize;
                    assert_eq!(id, 1);
                    let response = &response[4..length];
                    assert_eq!(&iter.to_le_bytes(), response);
                }
                dur_copy.set(start.elapsed());

                stream1.shutdown().await.unwrap();
                stream2.shutdown().await.unwrap();
            });
            duration.get()
        });
    });

    c.bench_function("manual_read_all_macro_channel_stream", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let (client, server) = bipipe();
                let mut stream1 = OwnedFdStream::new(client);
                let mut stream2 = OwnedFdStream::new(server);

                let start = Instant::now();
                for iter in 0..iters {
                    let mut buffer = [0; 128];
                    // let mut buffer = BufferView::new(&mut buffer);
                    // buffer.set(&iter.to_le_bytes());
                    let id = 1u32;
                    // buffer.prefix(&id.to_le_bytes());
                    let length: i32 = 12;
                    // buffer.prefix(&length.to_le_bytes());
                    buffer[0..4].copy_from_slice(&length.to_le_bytes());
                    buffer[4..8].copy_from_slice(&id.to_le_bytes());
                    buffer[8..16].copy_from_slice(&iter.to_le_bytes());

                    stream1.write(&buffer[0..16], None).await.unwrap();

                    let mut response = [0; 128];
                    stream2.read(&mut response[0..4], None).await.unwrap();
                    let length = u32::from_le_bytes(response[0..4].try_into().unwrap()) as usize;
                    stream2.read(&mut response[0..length], None).await.unwrap();
                    let id = u32::from_le_bytes(response[0..4].try_into().unwrap()) as usize;
                    assert_eq!(id, 1);
                    let response = &response[4..length];
                    assert_eq!(&iter.to_le_bytes(), response);
                }
                dur_copy.set(start.elapsed());

                stream1.shutdown().await.unwrap();
                stream2.shutdown().await.unwrap();
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
