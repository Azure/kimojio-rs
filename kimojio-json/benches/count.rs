// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The cost of one token when the visitor never suspends the scan.
//!
//! The visitor in this benchmark does the minimum: it increments a counter and
//! returns `None`. Each suspension test in the scanner is therefore provably
//! dead code, and the optimizer removes it. A full document is one call, and
//! the loop stays inside the tokenizer. This is the lower limit for the push
//! interface, and the value to compare a real decoder against.
//!
//! The report shows the throughput in **elements**. Criterion thus prints
//! tokens for each second. `benches/tokenize.rs` reports the same runs in bytes
//! for each second, and its visitor also reads the payload. The two files
//! answer different questions, and both are necessary.
//!
//! One kind of token is dominant in each input, because the cost is a function
//! of the token kind. A model of "a cost for each byte plus a cost for each
//! token" does not agree with these measurements. `digits` and `structure` both
//! have approximately two bytes for each token, but a number of one digit costs
//! almost three times as much as a brace. The cost of a token is mostly a
//! function of its kind. To calculate the cost of a document, calculate the
//! cost of its token kinds. `varlink_reply` is a real mixture of kinds. It
//! shows if the parts predict the whole.
//!
//! A profile of the `digits` case shows where the time of a number goes: `run`,
//! `scan_value`, `scan_number`, and `from_utf8` are four separate calls that
//! the optimizer did not merge. The push interface removed the cost of a return
//! to the caller for each token. The remaining cost is the movement inside the
//! tokenizer.
//!
//! Note the limits of this measurement. The visitor ignores the `&str` that it
//! receives. The compiler can therefore remove the work that produces that
//! value together with the value. This is a true property of the path that
//! always returns `None`, and not a defect in the benchmark. It does mean that
//! these values are a lower limit: a decoder that reads its input costs more.

use core::convert::Infallible;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use kimojio_json::{EscapeString, Tokenizer, Visitor};

/// A reply from systemd-resolved. Keep this value equal to the value in
/// `benches/tokenize.rs`.
const VARLINK_REPLY: &str = r#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[104,20,23,154]},{"ifindex":2,"family":10,"address":[32,1,13,184,0,0,0,0,0,0,0,0,0,0,0,1]}],"name":"example.com","flags":8388609}}"#;

/// Counts the tokens and does nothing else.
///
/// `Infallible` as the output type is the important part. It makes "this
/// visitor never suspends the scan" a fact that the compiler can use, and not
/// only a comment.
#[derive(Default)]
struct Count {
    tokens: u64,
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

    fn key(&mut self, _: &'a str) -> Option<Infallible> {
        self.tokens += 1;
        None
    }

    fn escape_key(&mut self, _: EscapeString<'a>) -> Option<Infallible> {
        self.tokens += 1;
        None
    }

    fn string(&mut self, _: &'a str) -> Option<Infallible> {
        self.tokens += 1;
        None
    }

    fn escape_string(&mut self, _: EscapeString<'a>) -> Option<Infallible> {
        self.tokens += 1;
        None
    }

    fn number(&mut self, _: &'a str) -> Option<Infallible> {
        self.tokens += 1;
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

/// Tokenizes `input` completely and returns the number of tokens in it.
fn count(input: &str) -> u64 {
    let mut visitor = Count::default();
    let mut tokenizer = Tokenizer::new(input);
    let stopped = tokenizer.drive(&mut visitor).expect("valid JSON");
    assert!(stopped.is_none(), "a counter never suspends the tokenizer");
    visitor.tokens
}

/// Makes an array of empty objects: three tokens for each four bytes.
fn structure(entries: usize) -> String {
    let mut json = String::from("[");
    for index in 0..entries {
        if index > 0 {
            json.push(',');
        }
        json.push_str("{}");
    }
    json.push(']');
    json
}

/// Makes an array of numbers of one digit: one token for each two bytes.
fn digits(entries: usize) -> String {
    let mut json = String::from("[");
    for index in 0..entries {
        if index > 0 {
            json.push(',');
        }
        json.push(char::from(b'0' + (index % 10) as u8));
    }
    json.push(']');
    json
}

/// Makes an array of literals, which have no payload.
fn literals(entries: usize) -> String {
    let mut json = String::from("[");
    for index in 0..entries {
        if index > 0 {
            json.push(',');
        }
        json.push_str(["true", "false", "null"][index % 3]);
    }
    json.push(']');
    json
}

/// Makes an array of strings of one character. This input uses the string path
/// with almost no bytes to scan. It thus separates the cost of a string token
/// from the cost of its bytes.
fn short_strings(entries: usize) -> String {
    let mut json = String::from("[");
    for index in 0..entries {
        if index > 0 {
            json.push(',');
        }
        json.push('"');
        json.push(char::from(b'a' + (index % 26) as u8));
        json.push('"');
    }
    json.push(']');
    json
}

/// Makes an object of long string values with no escape sequences. In this
/// input the bytes cost more than the tokens.
fn prose(entries: usize) -> String {
    let mut json = String::from("{");
    for index in 0..entries {
        if index > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "\"key{index}\":\"a fairly long string value that keeps the scanner in its bulk path\""
        ));
    }
    json.push('}');
    json
}

pub fn benchmark(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("count");

    let inputs = [
        ("structure", structure(64)),
        ("literals", literals(64)),
        ("short_strings", short_strings(64)),
        ("digits", digits(64)),
        ("prose", prose(8)),
        ("varlink_reply", String::from(VARLINK_REPLY)),
    ];

    for (name, input) in &inputs {
        let tokens = count(input);

        // Elements and not bytes, thus the report shows tokens for each second.
        // The count comes from the tokenizer itself, thus the report stays
        // correct after a change to an input.
        group.throughput(Throughput::Elements(tokens));
        group.bench_function(*name, |bencher| {
            bencher.iter(|| black_box(count(black_box(input))));
        });
    }

    group.finish();
}

criterion_group!(benches, benchmark);
criterion_main!(benches);
