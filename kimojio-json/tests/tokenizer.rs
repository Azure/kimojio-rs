// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the JSON tokenizer.
//!
//! These tests use only the public API. They cover the grammar, the reported
//! errors and their offsets, the two paths for string bodies, the suspension
//! and continuation of a scan, and the decode of escape sequences.

use std::{convert::Infallible, fmt::Write as _};

use kimojio_json::{Error, ErrorKind, EscapeString, MAX_DEPTH, Tokenizer, Unescape, Visitor};

#[derive(Debug, Eq, PartialEq)]
enum SeenToken<'a> {
    ObjectStart,
    ObjectEnd,
    ArrayStart,
    ArrayEnd,
    Key(&'a str),
    EscapeKey(&'a str),
    String(&'a str),
    EscapeString(&'a str),
    Number(&'a str),
    Bool(bool),
    Null,
}

/// Records each token in order.
///
/// If `suspend` is false, this visitor scans a full document in one call. A
/// caller that fills a struct in one pass has this behavior. If `suspend` is
/// true, the tokenizer stops at each token. Each token boundary then becomes a
/// resume point, and the test uses many more resume points than a real caller.
#[derive(Default)]
struct Record<'a> {
    seen: Vec<SeenToken<'a>>,
    suspend: bool,
}

impl<'a> Record<'a> {
    /// Makes a recorder that suspends the tokenizer at each token.
    fn stepping() -> Self {
        Self {
            seen: Vec::new(),
            suspend: true,
        }
    }

    /// Records a token, and suspends the scan if this recorder must do so.
    fn push(&mut self, token: SeenToken<'a>) -> Option<()> {
        self.seen.push(token);
        self.suspend.then_some(())
    }
}

impl<'a> Visitor<'a> for Record<'a> {
    type Output = ();

    fn object_start(&mut self) -> Option<()> {
        self.push(SeenToken::ObjectStart)
    }

    fn object_end(&mut self) -> Option<()> {
        self.push(SeenToken::ObjectEnd)
    }

    fn array_start(&mut self) -> Option<()> {
        self.push(SeenToken::ArrayStart)
    }

    fn array_end(&mut self) -> Option<()> {
        self.push(SeenToken::ArrayEnd)
    }

    fn key(&mut self, name: &'a str) -> Option<()> {
        self.push(SeenToken::Key(name))
    }

    fn escape_key(&mut self, name: EscapeString<'a>) -> Option<()> {
        self.push(SeenToken::EscapeKey(name.as_raw_str()))
    }

    fn string(&mut self, value: &'a str) -> Option<()> {
        self.push(SeenToken::String(value))
    }

    fn escape_string(&mut self, value: EscapeString<'a>) -> Option<()> {
        self.push(SeenToken::EscapeString(value.as_raw_str()))
    }

    fn number(&mut self, text: &'a str) -> Option<()> {
        self.push(SeenToken::Number(text))
    }

    fn boolean(&mut self, value: bool) -> Option<()> {
        self.push(SeenToken::Bool(value))
    }

    fn null(&mut self) -> Option<()> {
        self.push(SeenToken::Null)
    }
}

/// A visitor that cannot suspend the scan, written with the two methods that
/// the trait requires.
///
/// `Option<Infallible>` has exactly one possible value, thus the compiler
/// removes each suspension test in the driver. Each other method keeps its
/// default `None` body. A visitor with two methods is therefore complete.
struct NeverSuspends;

impl<'a> Visitor<'a> for NeverSuspends {
    type Output = Infallible;

    fn escape_key(&mut self, _: EscapeString<'a>) -> Option<Infallible> {
        None
    }

    fn escape_string(&mut self, _: EscapeString<'a>) -> Option<Infallible> {
        None
    }
}

/// Suspends the scan at the first string or member name that has no escape
/// sequence, and returns it.
///
/// Each other method keeps its default `None` body. The tokenizer therefore
/// scans through a container around the string, and this visitor does not
/// examine those tokens.
struct FirstText;

impl<'a> Visitor<'a> for FirstText {
    type Output = &'a str;

    fn key(&mut self, name: &'a str) -> Option<&'a str> {
        Some(name)
    }

    fn string(&mut self, value: &'a str) -> Option<&'a str> {
        Some(value)
    }

    // These helpers do not ask for an escaped string. The tokenizer thus scans
    // through it, the document ends without a suspension, and the helper
    // reports that condition with a better message than a panic in the visitor.
    fn escape_key(&mut self, _: EscapeString<'a>) -> Option<&'a str> {
        None
    }

    fn escape_string(&mut self, _: EscapeString<'a>) -> Option<&'a str> {
        None
    }
}

/// Suspends the scan at the first escaped string or member name, and returns
/// it.
struct FirstEscape;

impl<'a> Visitor<'a> for FirstEscape {
    type Output = EscapeString<'a>;

    fn escape_key(&mut self, name: EscapeString<'a>) -> Option<EscapeString<'a>> {
        Some(name)
    }

    fn escape_string(&mut self, value: EscapeString<'a>) -> Option<EscapeString<'a>> {
        Some(value)
    }
}

fn tokens(input: &str) -> Vec<SeenToken<'_>> {
    let mut record = Record::default();
    let mut tokenizer = Tokenizer::new(input);
    let finished = tokenizer.drive(&mut record).expect("valid JSON");
    assert!(finished.is_none(), "a plain recorder never suspends");
    record.seen
}

fn first_error(input: &str) -> Error {
    let mut record = Record::default();
    let mut tokenizer = Tokenizer::new(input);
    tokenizer
        .drive(&mut record)
        .expect_err("expected the document to be rejected")
}

fn assert_error(input: &str, kind: ErrorKind, offset: usize) {
    let error = first_error(input);
    assert_eq!(error.kind(), kind, "{input:?}");
    assert_eq!(error.offset(), offset, "{input:?}");
}

fn only_string(input: &str) -> &str {
    let mut tokenizer = Tokenizer::new(input);
    let Some(value) = tokenizer.drive(&mut FirstText).unwrap() else {
        panic!("expected an unescaped string from {input:?}");
    };
    value
}

fn only_escape_string(input: &str) -> EscapeString<'_> {
    let mut tokenizer = Tokenizer::new(input);
    let Some(value) = tokenizer.drive(&mut FirstEscape).unwrap() else {
        panic!("expected an escaped string from {input:?}");
    };
    value
}

fn key_from(input: &str) -> &str {
    only_string(input)
}

fn escape_key_from(input: &str) -> EscapeString<'_> {
    only_escape_string(input)
}

fn unescaped(input: EscapeString<'_>) -> Result<String, Error> {
    let mut buffer = vec![0_u8; input.raw_len()];
    input.unescape_into(&mut buffer).map(str::to_owned)
}
#[test]
fn scalar_top_level_values_are_tokenized() {
    assert_eq!(tokens("null"), vec![SeenToken::Null]);
    assert_eq!(tokens("true"), vec![SeenToken::Bool(true)]);
    assert_eq!(tokens("false"), vec![SeenToken::Bool(false)]);
    assert_eq!(tokens("42"), vec![SeenToken::Number("42")]);
    assert_eq!(tokens(r#""hello""#), vec![SeenToken::String("hello")]);
}

#[test]
fn every_valid_number_form_preserves_raw_text() {
    let mut long_digits = String::from("1234567890");
    for _ in 0..8 {
        long_digits.push_str("1234567890");
    }

    for input in [
        "0",
        "-0",
        "42",
        "-42",
        "3.14",
        "-0.5",
        "1e10",
        "1E10",
        "1e+10",
        "1e-10",
        "-2.5E-3",
        "0.0e0",
        &long_digits,
    ] {
        assert_eq!(tokens(input), vec![SeenToken::Number(input)]);
    }
}

#[test]
fn empty_and_nested_containers_are_tokenized() {
    assert_eq!(
        tokens("{}"),
        vec![SeenToken::ObjectStart, SeenToken::ObjectEnd]
    );
    assert_eq!(
        tokens("[]"),
        vec![SeenToken::ArrayStart, SeenToken::ArrayEnd]
    );
    assert_eq!(
        tokens(r#"{"a":[{"b":null},[],{}],"c":{"d":true}}"#),
        vec![
            SeenToken::ObjectStart,
            SeenToken::Key("a"),
            SeenToken::ArrayStart,
            SeenToken::ObjectStart,
            SeenToken::Key("b"),
            SeenToken::Null,
            SeenToken::ObjectEnd,
            SeenToken::ArrayStart,
            SeenToken::ArrayEnd,
            SeenToken::ObjectStart,
            SeenToken::ObjectEnd,
            SeenToken::ArrayEnd,
            SeenToken::Key("c"),
            SeenToken::ObjectStart,
            SeenToken::Key("d"),
            SeenToken::Bool(true),
            SeenToken::ObjectEnd,
            SeenToken::ObjectEnd,
        ]
    );
}

#[test]
fn whitespace_is_accepted_in_every_legal_position() {
    let input = " \t\n\r { \n \"a\" \t : \r [ \n 1 \t , \r true \n ] \t , \n \"b\" \r : \t { \n } \r } \t\n ";
    assert_eq!(
        tokens(input),
        vec![
            SeenToken::ObjectStart,
            SeenToken::Key("a"),
            SeenToken::ArrayStart,
            SeenToken::Number("1"),
            SeenToken::Bool(true),
            SeenToken::ArrayEnd,
            SeenToken::Key("b"),
            SeenToken::ObjectStart,
            SeenToken::ObjectEnd,
            SeenToken::ObjectEnd,
        ]
    );
}

#[test]
fn key_tokens_consume_the_colon() {
    // The tokenizer commits its cursor before it calls the visitor. A
    // suspension is therefore a position at which the caller can read `offset`.
    // The offset points to the byte after the colon, and not to the byte after
    // the member name.
    let mut tokenizer = Tokenizer::new(r#"{"answer":42}"#);
    assert_eq!(tokenizer.drive(&mut FirstText).unwrap(), Some("answer"));
    assert_eq!(tokenizer.offset(), r#"{"answer":"#.len());
}

#[test]
fn escape_key_tokens_consume_the_colon() {
    let mut tokenizer = Tokenizer::new(r#"{"a\nb":1}"#);
    let Some(key) = tokenizer.drive(&mut FirstEscape).unwrap() else {
        panic!("expected an escaped key");
    };
    assert_eq!(key.as_raw_str(), r"a\nb");
    assert_eq!(tokenizer.offset(), r#"{"a\nb":"#.len());
}

#[test]
fn a_suspended_tokenizer_resumes_where_it_stopped() {
    // A suspension records no resume point. The tokenizer commits all of its
    // state before the suspension. A different visitor can therefore read the
    // remainder of the document.
    let mut tokenizer = Tokenizer::new(r#"{"answer":42}"#);
    assert_eq!(tokenizer.drive(&mut FirstText).unwrap(), Some("answer"));

    let mut rest = Record::default();
    assert_eq!(tokenizer.drive(&mut rest), Ok(None));
    assert_eq!(
        rest.seen,
        vec![SeenToken::Number("42"), SeenToken::ObjectEnd]
    );
}

#[test]
fn suspending_on_every_token_sees_what_racing_through_sees() {
    let input = r#"{"a":[1,"two",false],"b":{}}"#;

    let mut stepper = Record::stepping();
    let mut tokenizer = Tokenizer::new(input);
    let mut resumes = 0;
    while tokenizer.drive(&mut stepper).unwrap().is_some() {
        resumes += 1;
    }

    assert_eq!(stepper.seen, tokens(input));
    assert_eq!(resumes, stepper.seen.len());
}

#[test]
fn a_visitor_that_never_suspends_still_drives_a_whole_document() {
    let input = r#"{"a":[1,"x\n",true,null],"b\t":{}}"#;
    let mut tokenizer = Tokenizer::new(input);
    assert!(tokenizer.drive(&mut NeverSuspends).unwrap().is_none());
    assert_eq!(tokenizer.offset(), input.len());
    assert_eq!(tokenizer.depth(), 0);
}

#[test]
fn driving_a_finished_document_keeps_reporting_completion() {
    let mut tokenizer = Tokenizer::new("[1,2]");
    let mut record = Record::default();
    assert_eq!(tokenizer.drive(&mut record), Ok(None));

    // No input remains, thus a second call to `drive` does not scan the input
    // again.
    assert_eq!(tokenizer.drive(&mut record), Ok(None));
    assert_eq!(record.seen.len(), 4);
}

#[test]
fn offset_advances_monotonically_and_stays_within_input() {
    let input = r#" {"a":[0,{"b":"c"}],"d":false} "#;
    let mut tokenizer = Tokenizer::new(input);
    let mut stepper = Record::stepping();
    let mut last = tokenizer.offset();
    while tokenizer.drive(&mut stepper).unwrap().is_some() {
        let current = tokenizer.offset();
        assert!(current >= last, "{current} regressed below {last}");
        assert!(current <= input.len(), "{current} exceeded {}", input.len());
        last = current;
    }
    assert!(tokenizer.offset() <= input.len());
}

#[test]
fn depth_reports_open_containers_at_each_token() {
    let input = r#"{"a":[{},[]]}"#;
    let mut tokenizer = Tokenizer::new(input);
    let mut stepper = Record::stepping();
    let mut depths = Vec::new();
    while tokenizer.drive(&mut stepper).unwrap().is_some() {
        depths.push(tokenizer.depth());
    }

    assert_eq!(
        stepper.seen,
        vec![
            SeenToken::ObjectStart,
            SeenToken::Key("a"),
            SeenToken::ArrayStart,
            SeenToken::ObjectStart,
            SeenToken::ObjectEnd,
            SeenToken::ArrayStart,
            SeenToken::ArrayEnd,
            SeenToken::ArrayEnd,
            SeenToken::ObjectEnd,
        ]
    );
    assert_eq!(depths, vec![1, 1, 2, 3, 2, 3, 2, 1, 0]);
}

#[test]
fn a_visitor_can_discard_a_member_it_does_not_want() {
    // The tokenizer has no skip method. A visitor that does not want a value
    // counts the containers instead. That costs less than a second scan of the
    // value by the tokenizer.
    let mut keep = KeepOnly::default();
    let mut tokenizer = Tokenizer::new(r#"{"skip":{"nested":[1,2,{"x":3}]},"keep":4}"#);
    assert_eq!(tokenizer.drive(&mut keep), Ok(None));
    assert_eq!(keep.kept, vec!["4"]);
}

#[test]
fn discarding_a_member_handles_arrays_and_mixed_nesting() {
    let mut keep = KeepOnly::default();
    let mut tokenizer =
        Tokenizer::new(r#"{"skip":[{"a":[true,{"b":null}]},"after"],"keep":"yes"}"#);
    assert_eq!(tokenizer.drive(&mut keep), Ok(None));
    assert_eq!(keep.kept, vec!["yes"]);
}

/// Collects the scalar value of each `keep` member and discards the other
/// members.
///
/// This visitor replaces a `skip_value` method on the tokenizer.
#[derive(Default)]
struct KeepOnly<'a> {
    kept: Vec<&'a str>,
    /// How many containers of a discarded value are open.
    skipping: usize,
    /// Whether this visitor keeps the current member; `None` outside of a
    /// member.
    wanted: Option<bool>,
}

impl<'a> KeepOnly<'a> {
    fn enter(&mut self) {
        if self.skipping > 0 {
            self.skipping += 1;
        } else if self.wanted == Some(false) {
            self.skipping = 1;
        } else {
            self.wanted = None;
        }
    }

    fn leave(&mut self) {
        if self.skipping > 0 {
            self.skipping -= 1;
            if self.skipping == 0 {
                self.wanted = None;
            }
        }
    }

    fn scalar(&mut self, text: &'a str) {
        if self.skipping > 0 {
            return;
        }
        if self.wanted == Some(true) {
            self.kept.push(text);
        }
        self.wanted = None;
    }
}

impl<'a> Visitor<'a> for KeepOnly<'a> {
    type Output = Infallible;

    fn object_start(&mut self) -> Option<Infallible> {
        self.enter();
        None
    }

    fn array_start(&mut self) -> Option<Infallible> {
        self.enter();
        None
    }

    fn object_end(&mut self) -> Option<Infallible> {
        self.leave();
        None
    }

    fn array_end(&mut self) -> Option<Infallible> {
        self.leave();
        None
    }

    fn key(&mut self, name: &'a str) -> Option<Infallible> {
        if self.skipping == 0 {
            self.wanted = Some(name == "keep");
        }
        None
    }

    fn escape_key(&mut self, name: EscapeString<'a>) -> Option<Infallible> {
        if self.skipping == 0 {
            self.wanted = Some(name.eq_unescaped("keep"));
        }
        None
    }

    fn string(&mut self, value: &'a str) -> Option<Infallible> {
        self.scalar(value);
        None
    }

    fn escape_string(&mut self, _: EscapeString<'a>) -> Option<Infallible> {
        self.scalar("");
        None
    }

    fn number(&mut self, text: &'a str) -> Option<Infallible> {
        self.scalar(text);
        None
    }

    fn boolean(&mut self, _: bool) -> Option<Infallible> {
        self.scalar("");
        None
    }

    fn null(&mut self) -> Option<Infallible> {
        self.scalar("");
        None
    }
}

#[test]
fn empty_and_whitespace_only_inputs_are_rejected() {
    assert_error("", ErrorKind::UnexpectedEof, 0);
    assert_error(" \t\n\r", ErrorKind::UnexpectedEof, 4);
}

#[test]
fn unbalanced_and_mismatched_containers_are_rejected() {
    for (input, kind, offset) in [
        ("{", ErrorKind::UnexpectedEof, 1),
        ("[", ErrorKind::UnexpectedEof, 1),
        ("{]", ErrorKind::ExpectedKey, 1),
        ("[}", ErrorKind::UnexpectedByte, 1),
        ("]", ErrorKind::UnexpectedByte, 0),
        ("}", ErrorKind::UnexpectedByte, 0),
        (r#"{"a":1]"#, ErrorKind::UnexpectedByte, 6),
    ] {
        assert_error(input, kind, offset);
    }
}

#[test]
fn trailing_data_is_rejected() {
    for (input, offset) in [("1 2", 2), ("{} {}", 3), ("null x", 5), ("[] junk", 3)] {
        assert_error(input, ErrorKind::TrailingData, offset);
    }
}

#[test]
fn trailing_commas_are_rejected() {
    assert_error("[1,]", ErrorKind::UnexpectedByte, 3);
    assert_error(r#"{"a":1,}"#, ErrorKind::ExpectedKey, 7);
}

#[test]
fn leading_commas_and_empty_array_slots_are_rejected() {
    assert_error("[,1]", ErrorKind::UnexpectedByte, 1);
    assert_error("[1,,2]", ErrorKind::UnexpectedByte, 3);
}

#[test]
fn non_string_object_keys_are_rejected() {
    for (input, offset) in [("{a:1}", 1), ("{1:2}", 1), ("{true:1}", 1)] {
        assert_error(input, ErrorKind::ExpectedKey, offset);
    }
}

#[test]
fn missing_colons_are_rejected() {
    assert_error(r#"{"a" 1}"#, ErrorKind::ExpectedColon, 5);
    assert_error(r#"{"a"}"#, ErrorKind::ExpectedColon, 4);
}

#[test]
fn bad_numbers_are_rejected() {
    for (input, kind, offset) in [
        ("01", ErrorKind::InvalidNumber, 1),
        ("-01", ErrorKind::InvalidNumber, 2),
        ("1.", ErrorKind::InvalidNumber, 2),
        (".5", ErrorKind::UnexpectedByte, 0),
        ("+1", ErrorKind::UnexpectedByte, 0),
        ("-", ErrorKind::UnexpectedEof, 1),
        ("1e", ErrorKind::InvalidNumber, 2),
        ("1e+", ErrorKind::InvalidNumber, 3),
        ("1.e5", ErrorKind::InvalidNumber, 2),
        ("--1", ErrorKind::InvalidNumber, 1),
        ("1..2", ErrorKind::InvalidNumber, 2),
    ] {
        assert_error(input, kind, offset);
    }
}

#[test]
fn unterminated_strings_are_rejected() {
    assert_error(r#""abc"#, ErrorKind::UnterminatedString, 4);
    assert_error(r#""abc\"#, ErrorKind::UnterminatedString, 5);
}

#[test]
fn raw_control_bytes_inside_strings_are_rejected() {
    assert_error("\"a\nb\"", ErrorKind::ControlCharacter, 2);
    assert_error("\"\0\"", ErrorKind::ControlCharacter, 1);
    assert_error("\"\x1f\"", ErrorKind::ControlCharacter, 1);
}

#[test]
fn bad_escapes_are_rejected() {
    for (input, offset) in [
        (r#""\q""#, 1),
        (r#""\u12""#, 1),
        (r#""\uZZZZ""#, 1),
        (r#""\u12g4""#, 1),
    ] {
        assert_error(input, ErrorKind::InvalidEscape, offset);
    }
}

#[test]
fn multibyte_text_is_borrowed_whole() {
    // The tokenizer accepts a `&str`, thus a string body cannot be invalid
    // UTF-8 and there is no validation for each token. The property to test is
    // that a character of more than one byte does not change the offsets at
    // which the scanner makes a slice.
    assert_eq!(
        tokens(r#"{"café":"naïve été"}"#),
        vec![
            SeenToken::ObjectStart,
            SeenToken::Key("café"),
            SeenToken::String("naïve été"),
            SeenToken::ObjectEnd,
        ]
    );
}

#[test]
fn bad_literals_are_rejected() {
    for (input, kind, offset) in [
        ("tru", ErrorKind::UnexpectedEof, 0),
        ("truX", ErrorKind::UnexpectedByte, 0),
        ("nul", ErrorKind::UnexpectedEof, 0),
        ("fals", ErrorKind::UnexpectedEof, 0),
        ("nulll", ErrorKind::TrailingData, 4),
    ] {
        assert_error(input, kind, offset);
    }
}

#[test]
fn max_depth_is_allowed_but_one_more_is_rejected() {
    let exactly = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
    assert_eq!(tokens(&exactly).len(), MAX_DEPTH * 2);

    let too_deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
    // The offset points to the bracket that opens level MAX_DEPTH + 1.
    assert_error(&too_deep, ErrorKind::DepthExceeded, MAX_DEPTH);
}

#[test]
fn tokenizer_is_poisoned_after_error() {
    let mut tokenizer = Tokenizer::new("[1,]");
    let mut record = Record::stepping();
    assert_eq!(tokenizer.drive(&mut record), Ok(Some(())));
    assert_eq!(tokenizer.drive(&mut record), Ok(Some(())));

    let first = tokenizer.drive(&mut record).unwrap_err();
    let second = tokenizer.drive(&mut record).unwrap_err();
    let third = tokenizer.drive(&mut record).unwrap_err();
    assert_eq!(first, second);
    assert_eq!(first, third);
    assert_eq!(
        record.seen,
        vec![SeenToken::ArrayStart, SeenToken::Number("1")]
    );
}

#[test]
fn an_unfinished_document_is_rejected_rather_than_reported_complete() {
    // `drive` reports completion only for a complete, well-formed document. A
    // caller therefore cannot confuse "the input ended" with "the document
    // ended".
    for input in [r#"{"a":1"#, "[1", r#"{"a":"#, "{"] {
        let mut record = Record::default();
        let error = Tokenizer::new(input).drive(&mut record).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnexpectedEof, "{input:?}");
        assert_eq!(error.offset(), input.len(), "{input:?}");
    }
}

#[test]
fn trailing_data_after_the_document_is_rejected() {
    let mut record = Record::default();
    let error = Tokenizer::new("1 2").drive(&mut record).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TrailingData);
    assert_eq!(error.offset(), 2);
    assert_eq!(record.seen, vec![SeenToken::Number("1")]);
}

#[test]
fn escape_string_layout_stays_compact() {
    assert_eq!(core::mem::size_of::<EscapeString<'_>>(), 16);
}

#[test]
fn string_values_are_split_by_escape_presence() {
    assert_eq!(tokens(r#""plain""#), vec![SeenToken::String("plain")]);
    assert_eq!(tokens(r#""""#), vec![SeenToken::String("")]);

    for (input, raw) in [
        (r#""\nfirst""#, r"\nfirst"),
        (r#""a\nb""#, r"a\nb"),
        (r#""last\n""#, r"last\n"),
        (r#""a\\b""#, r"a\\b"),
    ] {
        assert_eq!(tokens(input), vec![SeenToken::EscapeString(raw)]);
    }
}

#[test]
fn object_keys_are_split_by_escape_presence() {
    assert_eq!(key_from(r#"{"plain":1}"#), "plain");

    // RFC 8259 permits escape sequences in a member name. The tokenizer must
    // therefore not reject such a name, and must not report it as decoded text.
    let key = escape_key_from(r#"{"a\nb":1}"#);
    assert_eq!(key.as_raw_str(), r"a\nb");
    assert!(key.eq_unescaped("a\nb"));
}

#[test]
fn empty_string_is_never_escape_string() {
    assert_eq!(tokens(r#""""#), vec![SeenToken::String("")]);
    assert_eq!(only_string(r#""""#), "");
}

#[test]
fn non_ascii_utf8_without_escapes_is_plain_string_and_round_trips() {
    for value in ["héllo", "😀", "héllo😀"] {
        let input = format!(r#""{value}""#);
        assert_eq!(tokens(&input), vec![SeenToken::String(value)]);
        let string = only_string(&input);
        assert_eq!(string.as_bytes(), value.as_bytes());
    }
}

#[test]
fn escape_string_raw_accessors_expose_undecoded_body() {
    let raw = only_escape_string(r#""a\nb""#);
    assert_eq!(raw.as_raw_str(), r"a\nb");
    assert_eq!(raw.raw_len(), 4);
}

#[test]
fn raw_len_buffer_always_suffices_for_unescape_into() {
    for (input, expected) in [
        (r#""a\nb""#, "a\nb"),
        (r#""\u00e9""#, "é"),
        (r#""\ud83d\ude00""#, "😀"),
        (r#""x\t\u0041\u00e9\ud83d\ude00z""#, "x\tAé😀z"),
        (r#""héllo\n😀""#, "héllo\n😀"),
    ] {
        let raw = only_escape_string(input);
        let mut buffer = vec![0_u8; raw.raw_len()];
        let decoded = raw.unescape_into(&mut buffer).unwrap();
        assert_eq!(decoded, expected, "{input:?}");
        assert!(raw.raw_len() >= decoded.len(), "{input:?}");
    }
}

#[test]
fn borrowed_string_outlives_tokenizer() {
    fn borrowed_after_tokenizer_drop(input: &str) -> &str {
        {
            let mut tokenizer = Tokenizer::new(input);
            let Some(value) = tokenizer.drive(&mut FirstText).unwrap() else {
                panic!("expected string");
            };
            value
        }
    }

    let input = String::from(r#""borrowed""#);
    assert_eq!(borrowed_after_tokenizer_drop(&input), "borrowed");
}

#[test]
fn unescape_into_decodes_simple_unicode_surrogate_and_mixed_text() {
    let raw = only_escape_string(r#""\"\\\/\b\f\n\r\t""#);
    assert_eq!(unescaped(raw).unwrap(), "\"\\/\u{0008}\u{000c}\n\r\t");

    assert_eq!(
        unescaped(only_escape_string(r#""\u00e9 \u0041""#)).unwrap(),
        "é A"
    );
    assert_eq!(
        unescaped(only_escape_string(r#""\ud83d\ude00""#)).unwrap(),
        "😀"
    );
    assert_eq!(
        unescaped(only_escape_string(r#""a\n\u0041😀z""#)).unwrap(),
        "a\nA😀z"
    );
}

#[test]
fn unescape_into_honors_buffer_size() {
    let raw = only_escape_string(r#""a\n😀""#);
    let expected_len = "a\n😀".len();
    let mut exact = vec![0_u8; expected_len];
    assert_eq!(raw.unescape_into(&mut exact).unwrap(), "a\n😀");

    let mut too_small = vec![0_u8; expected_len - 1];
    let error = raw.unescape_into(&mut too_small).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BufferTooSmall);
    assert_eq!(error.offset(), 2);
}

#[test]
fn lone_surrogates_are_accepted_by_tokenizer_but_rejected_by_unescaping() {
    // Tokenization validates the syntax of the JSON escape sequences only. The
    // validation of the Unicode scalar values occurs during the decode.
    for input in [
        r#""\ud800""#,
        r#""\ud800x""#,
        r#""\udc00""#,
        r#""\ud800\u0041""#,
    ] {
        let raw = only_escape_string(input);
        let error = unescaped(raw).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::LoneSurrogate, "{input:?}");
        assert_eq!(error.offset(), 0, "{input:?}");
    }
}

/// A loop over `unescape()` must terminate. The tokenizer accepts a lone
/// surrogate and leaves the check for the decode. An iterator that repeated its
/// error would therefore not terminate for JSON from an attacker.
#[test]
fn unescape_reports_an_error_once_and_then_ends() {
    for input in [
        r#""\ud800""#,
        r#""\ud800x""#,
        r#""\udc00""#,
        r#""\ud800\u0041""#,
        r#""ok\ud800tail""#,
    ] {
        let raw = only_escape_string(input);

        let decoded: Vec<_> = raw.unescape().collect();
        assert_eq!(
            decoded.iter().filter(|item| item.is_err()).count(),
            1,
            "{input:?} should report exactly one error"
        );
        assert!(
            decoded.last().is_some_and(Result::is_err),
            "{input:?} should end on the error"
        );

        let mut iterator = raw.unescape();
        for item in iterator.by_ref() {
            if item.is_err() {
                break;
            }
        }
        assert_eq!(iterator.next(), None, "{input:?} should be exhausted");
        assert_eq!(iterator.next(), None, "{input:?} should stay exhausted");
    }
}

#[test]
fn eq_unescaped_compares_decoded_content() {
    assert!(only_escape_string(r#""\u0041""#).eq_unescaped("A"));
    assert!(only_escape_string(r#""a\nb""#).eq_unescaped("a\nb"));
    assert!(!only_escape_string(r#""a\nb""#).eq_unescaped(r"a\nb"));
    assert!(!only_escape_string(r#""prefix\n""#).eq_unescaped("prefix"));
    assert!(!only_escape_string(r#""pref\n""#).eq_unescaped("prefix\n"));
}

#[test]
fn unescape_iterator_matches_unescape_into() {
    let raw = only_escape_string(r#""a\n\u00e9\ud83d\ude00z""#);
    let iterator: Unescape<'_> = raw.unescape();
    let decoded = iterator.collect::<Result<String, Error>>().unwrap();
    assert_eq!(decoded, unescaped(raw).unwrap());
}

#[test]
fn escape_string_traits_expose_only_raw_debug_and_value_identity() {
    let raw = only_escape_string(r#""a\nb""#);
    let copy = raw;
    assert_eq!(copy, raw);
    assert_eq!(format!("{raw:?}"), r#"EscapeString("a\\nb")"#);
}

#[test]
fn object_escape_keys_can_be_decoded() {
    let key = escape_key_from(r#"{"a\nb":1}"#);
    assert_eq!(key.as_raw_str(), r"a\nb");
    assert!(key.eq_unescaped("a\nb"));
}

#[test]
fn systemd_resolved_varlink_reply_tokenizes_as_expected() {
    let input = r#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[104,20,23,154]}],"name":"example.com","flags":8388609}}"#;
    assert_eq!(
        tokens(input),
        vec![
            SeenToken::ObjectStart,
            SeenToken::Key("parameters"),
            SeenToken::ObjectStart,
            SeenToken::Key("addresses"),
            SeenToken::ArrayStart,
            SeenToken::ObjectStart,
            SeenToken::Key("ifindex"),
            SeenToken::Number("2"),
            SeenToken::Key("family"),
            SeenToken::Number("2"),
            SeenToken::Key("address"),
            SeenToken::ArrayStart,
            SeenToken::Number("104"),
            SeenToken::Number("20"),
            SeenToken::Number("23"),
            SeenToken::Number("154"),
            SeenToken::ArrayEnd,
            SeenToken::ObjectEnd,
            SeenToken::ArrayEnd,
            SeenToken::Key("name"),
            SeenToken::String("example.com"),
            SeenToken::Key("flags"),
            SeenToken::Number("8388609"),
            SeenToken::ObjectEnd,
            SeenToken::ObjectEnd,
        ]
    );
}

#[test]
fn error_display_and_kind_descriptions_are_stable_enough_for_debugging() {
    let error = Error::new(ErrorKind::ExpectedKey, 7);
    assert_eq!(
        ErrorKind::ExpectedKey.as_str(),
        "expected an object member name"
    );
    assert_eq!(
        error.to_string(),
        "expected an object member name at byte 7"
    );
}

#[test]
fn escape_string_debug_shows_the_undecoded_body() {
    // The output shows the raw body. A decode here would show text that the
    // caller did not receive.
    let mut output = String::new();
    write!(&mut output, "{:?}", only_escape_string(r#""caf\u00e9""#)).unwrap();
    assert_eq!(output, r#"EscapeString("caf\\u00e9")"#);
}
