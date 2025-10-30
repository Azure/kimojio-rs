// Copyright (c) Microsoft Corporation. All rights reserved.
use std::{cell::RefCell, hint::black_box, ops::DerefMut, rc::Rc, time::Duration};

use criterion::{Criterion, criterion_group, criterion_main};
use kimojio::MutInPlaceCell;

pub fn benchmark(c: &mut Criterion) {
    fn inc(mut value: impl DerefMut<Target = usize>) {
        *value.deref_mut() += 1;
    }

    c.bench_function("RefCell::borrow_mut", |b| {
        let x = Rc::new(RefCell::new(0usize));
        b.iter(|| inc(black_box(x.borrow_mut())));
    });

    c.bench_function("MutInPlaceCell::borrow_mut", |b| {
        let x = Rc::new(MutInPlaceCell::new(0usize));
        b.iter(|| x.use_mut(|x| inc(black_box(x))));
    });
}

criterion_group!(
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(4));
    targets=benchmark
);
criterion_main!(benches);
