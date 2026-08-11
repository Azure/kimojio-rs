// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A JSON tokenizer that allocates no memory.
//!
//! This crate does one task: it reads a JSON document and reports each token
//! that it finds. It is not a deserializer. It has no DOM, no `serde`
//! integration, and no schema. No part of the crate allocates memory.
//!
//! The tokenizer *pushes* each token to a [`Visitor`]. The caller does not pull
//! the tokens one at a time. This is a decision about performance and not about
//! style. This crate is written for small messages with many tokens. A reply
//! from systemd-resolved has less than four bytes for each token. For such a
//! message, the scan of the bytes costs much less than the transfer of each
//! token to the caller. That transfer includes the return of the token, a
//! `match` on the token, and a new entry into the scanner. A push interface
//! removes the transfer.
//!
//! # Operations this crate does not do
//!
//! - **It does not expand escape sequences.** A string body arrives as a
//!   subslice of the input, and sequences such as `\n` and `\u00e9` keep their
//!   source form. The tokenizer does *validate* each escape sequence and
//!   rejects an incorrect one. An expansion needs storage for the result.
//!
//!   The method that the tokenizer calls tells the caller which strings need a
//!   decode. There is no flag. [`Visitor::string`] and [`Visitor::key`] receive
//!   a plain `&str` that needs no more work. [`Visitor::escape_string`] and
//!   [`Visitor::escape_key`] receive an [`EscapeString`]. The purpose of that
//!   type is to prevent an accidental use of text that is not decoded. Decode
//!   the text into a caller-supplied buffer with
//!   [`EscapeString::unescape_into`], compare it without a buffer with
//!   [`EscapeString::eq_unescaped`], or read the characters one at a time with
//!   [`EscapeString::unescape`].
//! - **It does not convert numbers.** [`Visitor::number`] receives the raw
//!   text. Only the caller knows if it must have a `u8`, an `i64`, or an `f64`.
//!   Only the caller knows what to do with a value that is too large for its
//!   type.
//!
//! # Validation
//!
//! The tokenizer validates the full JSON grammar. It rejects unbalanced
//! containers, misplaced commas, member names that are not strings, malformed
//! numbers, unterminated strings, raw control bytes, incorrect escape
//! sequences, and data after the top-level value. Each error reports the byte
//! offset of the failure. The tokenizer records the nesting in a bitmap of
//! fixed size. Deeply nested input therefore causes an
//! [`ErrorKind::DepthExceeded`] error and does not exhaust the memory or the
//! stack.
//!
//! [`Tokenizer::drive`] returns `Ok(None)` only if the document is well formed
//! *and* fully consumed. Therefore there is no separate step to complete the
//! scan.
//!
//! # Example
//!
//! ```
//! use core::convert::Infallible;
//! use kimojio_json::{EscapeString, Tokenizer, Visitor};
//!
//! #[derive(Default)]
//! struct CountKeys(usize);
//!
//! impl<'a> Visitor<'a> for CountKeys {
//!     type Output = Infallible;
//!
//!     fn key(&mut self, _: &'a str) -> Option<Infallible> {
//!         self.0 += 1;
//!         None
//!     }
//!     fn escape_key(&mut self, _: EscapeString<'a>) -> Option<Infallible> {
//!         self.0 += 1;
//!         None
//!     }
//!     fn escape_string(&mut self, _: EscapeString<'a>) -> Option<Infallible> {
//!         None
//!     }
//! }
//!
//! let document = r#"{"name":"kimojio","tags":["fast","small"],"size":42}"#;
//! let mut keys = CountKeys::default();
//!
//! assert!(Tokenizer::new(document).drive(&mut keys)?.is_none());
//! assert_eq!(keys.0, 3);
//! # Ok::<(), kimojio_json::Error>(())
//! ```
//!
//! # Suspension
//!
//! A visitor method that returns `Some(value)` suspends the scan.
//! [`Tokenizer::drive`] returns that value. A subsequent call to `drive`
//! continues at the position where the scan stopped. One tokenizer thus serves
//! two types of caller: a decoder that fills a full struct in one pass, and an
//! iterator that supplies each item as it arrives.
//!
//! # How to make structs from tokens
//!
//! The related method in this repository has three steps. A developer writes
//! down the JSON shape and the necessary Rust interface. A language model makes
//! a state machine above this tokenizer from that description. The result is
//! committed to the repository. State machines are difficult to write correctly
//! by hand, which is the reason to generate them.
//!
//! The `kimojio-json-decoder` skill in `.github/skills/` does this.
//! `kimojio/src/resolver/varlink_reply.rs` is an example. No part of the
//! released binary depends on a model: the generated code is usual Rust code,
//! and it is reviewed and tested like all other source code.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod escape_string;
mod tokenizer;

pub use crate::{
    error::{Error, ErrorKind},
    escape_string::{EscapeString, Unescape},
    tokenizer::{MAX_DEPTH, Tokenizer, Visitor},
};

/// Compiles the code blocks in `README.md` as doctests, thus the README stays
/// in agreement with the API that it describes.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct Readme;
