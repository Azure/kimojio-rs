// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Throughput measurements for the tokenizer.
//!
//! The inputs have the shape of real traffic, and not of synthetic worst cases:
//! a Varlink reply from systemd-resolved, an array of numbers, and an object
//! with many strings.
//!
//! The benchmark measures two types of visitor, because their costs differ.
//! `Count` never suspends the scan. The optimizer therefore removes each
//! suspension test, and the scanner reads the full document in one call.
//! `Yield` suspends at each value. This is the shape of an iterator, and it
//! costs one return and one new entry for each item.

use core::convert::Infallible;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use kimojio_json::{EscapeString, Tokenizer, Visitor};

const VARLINK_REPLY: &str = r#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[104,20,23,154]},{"ifindex":2,"family":10,"address":[32,1,13,184,0,0,0,0,0,0,0,0,0,0,0,1]}],"name":"example.com","flags":8388609}}"#;

const NUMBERS: &str = r#"[0,-1,42,3.14159,1e10,-2.5E-3,1234567890,0.5,-0.25,6.02e23,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16]"#;

const STRINGS: &str = r#"{"a":"plain ascii value","b":"another one with spaces","c":"unicode: \u00e9\u00e8\u00ea","d":"escapes \\ \" \n \t","e":"a fairly long string value that exercises the bulk scanning path in the tokenizer"}"#;

/// Counts the tokens and the bytes that they borrow. This visitor never
/// suspends the scan.
#[derive(Default)]
struct Count {
    tokens: usize,
    bytes: usize,
}

impl<'a> Visitor<'a> for Count {
    type Output = Infallible;

    fn object_start(&mut self) -> Option<Infallible> {
        self.tokens += 1;
        None
    }

    fn object_end(&mut self) -> Option<Infallible> {
        self.tokens += 1;
        None
    }

    fn array_start(&mut self) -> Option<Infallible> {
        self.tokens += 1;
        None
    }

    fn array_end(&mut self) -> Option<Infallible> {
        self.tokens += 1;
        None
    }

    fn key(&mut self, name: &'a str) -> Option<Infallible> {
        self.tokens += 1;
        self.bytes += name.len();
        None
    }

    fn escape_key(&mut self, name: EscapeString<'a>) -> Option<Infallible> {
        self.tokens += 1;
        self.bytes += name.raw_len();
        None
    }

    fn string(&mut self, value: &'a str) -> Option<Infallible> {
        self.tokens += 1;
        self.bytes += value.len();
        None
    }

    fn escape_string(&mut self, value: EscapeString<'a>) -> Option<Infallible> {
        self.tokens += 1;
        self.bytes += value.raw_len();
        None
    }

    fn number(&mut self, text: &'a str) -> Option<Infallible> {
        self.tokens += 1;
        self.bytes += text.len();
        None
    }

    fn boolean(&mut self, _: bool) -> Option<Infallible> {
        self.tokens += 1;
        None
    }

    fn null(&mut self) -> Option<Infallible> {
        self.tokens += 1;
        None
    }
}

/// Suspends the scan at each member name and each scalar value. An iterator has
/// this shape.
struct Yield;

impl<'a> Visitor<'a> for Yield {
    type Output = usize;

    fn key(&mut self, name: &'a str) -> Option<usize> {
        Some(name.len())
    }

    fn escape_key(&mut self, name: EscapeString<'a>) -> Option<usize> {
        Some(name.raw_len())
    }

    fn string(&mut self, value: &'a str) -> Option<usize> {
        Some(value.len())
    }

    fn escape_string(&mut self, value: EscapeString<'a>) -> Option<usize> {
        Some(value.raw_len())
    }

    fn number(&mut self, text: &'a str) -> Option<usize> {
        Some(text.len())
    }
}

/// Scans `input` completely with a visitor that never suspends the scan.
fn count(input: &str) -> usize {
    let mut visitor = Count::default();
    let mut tokenizer = Tokenizer::new(input);
    let finished = tokenizer.drive(&mut visitor).expect("valid JSON");
    assert!(finished.is_none(), "the document must be complete");
    black_box(visitor.bytes);
    visitor.tokens
}

/// Scans `input` completely and continues after each suspension.
fn drain(input: &str) -> usize {
    let mut visitor = Yield;
    let mut tokenizer = Tokenizer::new(input);
    let mut items = 0_usize;
    while let Some(length) = tokenizer.drive(&mut visitor).expect("valid JSON") {
        black_box(length);
        items += 1;
    }
    items
}

pub fn benchmark(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("tokenize");

    for (name, input) in [
        ("varlink_reply", VARLINK_REPLY),
        ("numbers", NUMBERS),
        ("strings", STRINGS),
    ] {
        group.throughput(Throughput::Bytes(input.len() as u64));
        group.bench_function(name, |bencher| {
            bencher.iter(|| count(black_box(input)));
        });
        group.bench_function(format!("{name}_yield"), |bencher| {
            bencher.iter(|| drain(black_box(input)));
        });
    }

    group.finish();
}

criterion_group!(benches, benchmark);
criterion_main!(benches);
