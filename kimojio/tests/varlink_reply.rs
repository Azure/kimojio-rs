// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Acceptance tests for the generated decoder of Varlink replies.
//!
//! These tests are written **by hand**. The `kimojio-json-decoder` skill that
//! generates `kimojio/src/resolver/varlink_reply.rs` does not read them. A
//! generator that also wrote its own tests could be incorrect and
//! self-consistent at the same time, and no test would report the defect. The
//! separation means that the generated code passes only if it implements the
//! requested contract.
//!
//! These are integration tests by design: they use only the public API. A
//! generated file can therefore not satisfy them with internal details.
//!
//! Consequence: the interface is fixed. These tests, the module documentation
//! in `kimojio/src/resolver/varlink_reply.rs`, and the call sites in
//! `kimojio/src/resolver.rs` change together or not at all.

use std::convert::Infallible;

use kimojio::resolver::varlink_reply::{
    EscapeString, MAX_DEPTH, ReplyError, ReplyErrorKind, ReplyVisitor, ResolveHostnameReply,
    ResolvedAddress,
};

/// One item that the decoder supplied. A test uses this type to assert on the
/// full sequence and its order in one step.
///
/// The decoder has no enumeration of its own, because it calls one method for
/// each item. This type is therefore the vocabulary of the tests, which is the
/// correct place for it: these tests describe the contract, and the contract is
/// "these items, in this order".
#[derive(Debug, Eq, PartialEq)]
enum Item<'a> {
    Address(ResolvedAddress),
    Name(&'a str),
    EscapeName(EscapeString<'a>),
    Flags(u64),
    Error(&'a str),
}

/// Records each item in order and never stops the decoder.
#[derive(Default)]
struct Record<'a> {
    items: Vec<Item<'a>>,
}

impl<'a> ReplyVisitor<'a> for Record<'a> {
    /// An uninhabited output type shows that this visitor never stops the
    /// decoder. One call therefore decodes a full reply.
    type Output = Infallible;

    fn address(&mut self, address: ResolvedAddress) -> Option<Infallible> {
        self.items.push(Item::Address(address));
        None
    }

    fn error(&mut self, name: &'a str) -> Option<Infallible> {
        self.items.push(Item::Error(name));
        None
    }

    fn name(&mut self, name: &'a str) -> Option<Infallible> {
        self.items.push(Item::Name(name));
        None
    }

    fn escape_name(&mut self, name: EscapeString<'a>) -> Option<Infallible> {
        self.items.push(Item::EscapeName(name));
        None
    }

    fn flags(&mut self, flags: u64) -> Option<Infallible> {
        self.items.push(Item::Flags(flags));
        None
    }
}

/// Decodes `reply` completely and fails at the first rejection.
fn decode(reply: &str) -> Result<Vec<Item<'_>>, ReplyError> {
    let mut record = Record::default();
    let stopped = ResolveHostnameReply::new(reply).drive(&mut record)?;
    assert!(stopped.is_none(), "a recorder never stops the decoder");
    Ok(record.items)
}

/// Returns the reason why the decoder rejected `reply`, and requires a
/// rejection.
fn rejection(reply: &str) -> ReplyErrorKind {
    match decode(reply) {
        Ok(items) => panic!("expected a rejection, decoded {items:?}"),
        Err(error) => error.kind(),
    }
}

/// Returns the addresses in `reply`, and requires that `reply` decodes.
fn addresses(reply: &str) -> Vec<ResolvedAddress> {
    decode(reply)
        .expect("the reply must decode")
        .into_iter()
        .filter_map(|item| match item {
            Item::Address(address) => Some(address),
            _ => None,
        })
        .collect()
}

/// The captured frames from the module documentation of the decoder.
///
/// They are duplicated here by design. The module documentation is the corpus
/// that a later run of the generator reads, and this copy causes a test failure
/// if the two copies become different.
const SAMPLES: &[&str] = &[
    // IPv4.
    r#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[104,20,23,154]}],"name":"example.com","flags":8388609}}"#,
    // IPv6.
    r#"{"parameters":{"addresses":[{"ifindex":7,"family":10,"address":[32,1,13,184,0,0,0,0,0,0,0,0,0,0,0,1]}],"name":"ipv6.example.com","flags":8388609}}"#,
    // Both families in one reply.
    r#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[192,0,2,1]},{"ifindex":0,"family":10,"address":[32,1,13,184,0,0,0,0,0,0,0,0,0,0,0,2]}],"name":"dual.example.com","flags":8388609}}"#,
    // An IP literal. The service synthesized this answer, thus `ifindex` is absent.
    r#"{"parameters":{"addresses":[{"family":2,"address":[127,0,0,1]}],"name":"127.0.0.1","flags":786945}}"#,
    // A method error. The service reports "no such host" in this form.
    r#"{"error":"io.systemd.Resolve.NoSuchResourceRecord","parameters":{}}"#,
    // Unknown members at each level, with the address members in a different order.
    r#"{"ignored":{"nested":[null]},"parameters":{"ignored":[{"value":true}],"addresses":[{"address":[104,20,23,154],"extra":{"nested":false},"ifindex":2,"family":2}],"name":"reordered.example.com"}}"#,
];

#[test]
fn ipv4_reply_yields_address_name_and_flags() {
    let reply = r#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[104,20,23,154]}],"name":"example.com","flags":8388609}}"#;

    let items = decode(reply).unwrap();
    assert_eq!(items.len(), 3);

    let Item::Address(address) = items[0] else {
        panic!("expected an address first, got {:?}", items[0]);
    };
    assert_eq!(address.ifindex(), 2);
    assert_eq!(address.family(), 2);
    assert_eq!(address.octets(), &[104, 20, 23, 154]);

    assert_eq!(items[1], Item::Name("example.com"));
    assert_eq!(items[2], Item::Flags(8_388_609));
}

#[test]
fn ipv6_reply_carries_all_sixteen_octets() {
    let reply = r#"{"parameters":{"addresses":[{"ifindex":7,"family":10,"address":[32,1,13,184,0,0,0,0,0,0,0,0,0,0,0,1]}]}}"#;

    let decoded = addresses(reply);
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].ifindex(), 7);
    assert_eq!(decoded[0].family(), 10);
    assert_eq!(
        decoded[0].octets(),
        &[32, 1, 13, 184, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
    );
}

#[test]
fn every_address_in_the_array_is_yielded_in_order() {
    let reply = r#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[192,0,2,1]},{"ifindex":0,"family":10,"address":[32,1,13,184,0,0,0,0,0,0,0,0,0,0,0,2]}]}}"#;

    let decoded = addresses(reply);
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].family(), 2);
    assert_eq!(decoded[0].octets(), &[192, 0, 2, 1]);
    assert_eq!(decoded[1].family(), 10);
    assert_eq!(decoded[1].octets().len(), 16);
}

/// systemd-resolved omits `ifindex` for a synthesized answer such as an IP
/// literal, where the address is not bound to a link.
#[test]
fn absent_ifindex_reports_zero() {
    let reply = r#"{"parameters":{"addresses":[{"family":2,"address":[127,0,0,1]}]}}"#;
    let decoded = addresses(reply);
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].ifindex(), 0);
}

#[test]
fn method_error_yields_the_error_name() {
    let reply = r#"{"error":"io.systemd.Resolve.NoSuchResourceRecord","parameters":{}}"#;
    assert_eq!(
        decode(reply).unwrap(),
        vec![Item::Error("io.systemd.Resolve.NoSuchResourceRecord")]
    );
}

#[test]
fn error_member_is_recognized_after_parameters() {
    let reply = r#"{"parameters":{},"error":"io.systemd.Resolve.NoSuchResourceRecord"}"#;
    assert_eq!(
        decode(reply).unwrap(),
        vec![Item::Error("io.systemd.Resolve.NoSuchResourceRecord")]
    );
}

/// An empty array and an absent array mean different things to the caller: the
/// service found no address, or the reply did not answer the request.
#[test]
fn empty_addresses_array_is_valid_and_yields_nothing() {
    assert_eq!(
        decode(r#"{"parameters":{"addresses":[]}}"#).unwrap(),
        vec![]
    );
}

#[test]
fn absent_addresses_without_an_error_is_a_missing_member() {
    assert_eq!(rejection(r#"{}"#), ReplyErrorKind::MissingMember);
    assert_eq!(
        rejection(r#"{"parameters":{}}"#),
        ReplyErrorKind::MissingMember
    );
    assert_eq!(
        rejection(r#"{"parameters":{"name":"example.com"}}"#),
        ReplyErrorKind::MissingMember
    );
}

#[test]
fn unknown_members_are_skipped_at_every_level() {
    let reply = r#"{"ignored":{"nested":[null]},"parameters":{"ignored":[{"value":true}],"addresses":[{"address":[104,20,23,154],"extra":{"nested":false},"ifindex":2,"family":2}]}}"#;

    let decoded = addresses(reply);
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].ifindex(), 2);
    assert_eq!(decoded[0].family(), 2);
    assert_eq!(decoded[0].octets(), &[104, 20, 23, 154]);
}

#[test]
fn address_members_may_appear_in_any_order() {
    let ordered = r#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[1,2,3,4]}]}}"#;
    let shuffled = r#"{"parameters":{"addresses":[{"address":[1,2,3,4],"family":2,"ifindex":2}]}}"#;
    assert_eq!(addresses(ordered), addresses(shuffled));
}

#[test]
fn escaped_name_is_reported_separately_from_a_plain_one() {
    let reply = r#"{"parameters":{"addresses":[],"name":"caf\u00e9.example.com"}}"#;
    let items = decode(reply).unwrap();
    assert_eq!(items.len(), 1);

    let Item::EscapeName(name) = items[0] else {
        panic!("expected an escaped name, got {:?}", items[0]);
    };
    assert_eq!(name.as_raw_str(), r"caf\u00e9.example.com");
    assert!(name.eq_unescaped("café.example.com"));
}

#[test]
fn plain_name_is_borrowed_from_the_reply() {
    let reply = r#"{"parameters":{"addresses":[],"name":"example.com"}}"#;
    assert_eq!(decode(reply).unwrap(), vec![Item::Name("example.com")]);
}

#[test]
fn malformed_json_is_reported_as_json() {
    for reply in [
        "",
        "1e",
        "-",
        "{",
        r#"{"parameters":{"addresses":["#,
        r#"{"parameters":{"addresses":[],}}"#,
    ] {
        assert_eq!(
            rejection(reply),
            ReplyErrorKind::Json,
            "expected a JSON rejection for {reply:?}"
        );
    }
}

#[test]
fn trailing_data_after_the_reply_is_rejected() {
    assert_eq!(
        rejection(r#"{"parameters":{"addresses":[]}} null"#),
        ReplyErrorKind::Json
    );
}

#[test]
fn a_reply_that_is_not_an_object_is_rejected() {
    for reply in [r#"[]"#, r#""text""#, r#"7"#, r#"null"#, r#"true"#] {
        assert_eq!(
            rejection(reply),
            ReplyErrorKind::UnexpectedType,
            "expected a type rejection for {reply:?}"
        );
    }
}

#[test]
fn wrongly_typed_members_are_rejected() {
    for reply in [
        r#"{"parameters":[]}"#,
        r#"{"parameters":"text"}"#,
        r#"{"parameters":{"addresses":{}}}"#,
        r#"{"parameters":{"addresses":7}}"#,
        r#"{"parameters":{"addresses":[null]}}"#,
        r#"{"parameters":{"addresses":[[]]}}"#,
        r#"{"parameters":{"addresses":[],"name":7}}"#,
        r#"{"parameters":{"addresses":[],"flags":"8"}}"#,
        r#"{"error":false,"parameters":{}}"#,
        r#"{"parameters":{"addresses":[{"ifindex":2,"family":"2","address":[127,0,0,1]}]}}"#,
        r#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":"abcd"}]}}"#,
        r#"{"parameters":{"addresses":[{"ifindex":true,"family":2,"address":[127,0,0,1]}]}}"#,
    ] {
        assert_eq!(
            rejection(reply),
            ReplyErrorKind::UnexpectedType,
            "expected a type rejection for {reply:?}"
        );
    }
}

/// `2.0` has the numerical value two but is not an integer. Acceptance of `2.0`
/// would also accept `2.5`.
#[test]
fn non_integer_numbers_are_rejected() {
    for reply in [
        r#"{"parameters":{"addresses":[{"family":2.0,"address":[127,0,0,1]}]}}"#,
        r#"{"parameters":{"addresses":[{"family":2,"address":[127,0,0,1.0]}]}}"#,
        r#"{"parameters":{"addresses":[{"ifindex":2e1,"family":2,"address":[127,0,0,1]}]}}"#,
        r#"{"parameters":{"addresses":[],"flags":1.5}}"#,
    ] {
        assert_eq!(
            rejection(reply),
            ReplyErrorKind::UnexpectedType,
            "expected a type rejection for {reply:?}"
        );
    }
}

#[test]
fn out_of_range_integers_are_rejected() {
    for reply in [
        // Greater than i32, which is the width of a C `int`.
        r#"{"parameters":{"addresses":[{"ifindex":4294967296,"family":2,"address":[127,0,0,1]}]}}"#,
        r#"{"parameters":{"addresses":[{"ifindex":2,"family":4294967296,"address":[127,0,0,1]}]}}"#,
        // Greater than a byte.
        r#"{"parameters":{"addresses":[{"family":2,"address":[127,0,0,256]}]}}"#,
        r#"{"parameters":{"addresses":[{"family":2,"address":[-1,0,0,1]}]}}"#,
        // Greater than u64.
        r#"{"parameters":{"addresses":[],"flags":18446744073709551616}}"#,
        r#"{"parameters":{"addresses":[],"flags":-1}}"#,
    ] {
        assert_eq!(
            rejection(reply),
            ReplyErrorKind::NumberOutOfRange,
            "expected a range rejection for {reply:?}"
        );
    }
}

#[test]
fn an_address_longer_than_sixteen_bytes_is_rejected() {
    let mut reply = String::from(r#"{"parameters":{"addresses":[{"family":2,"address":["#);
    for index in 0..17 {
        if index != 0 {
            reply.push(',');
        }
        reply.push('1');
    }
    reply.push_str("]}]}}");
    assert_eq!(rejection(&reply), ReplyErrorKind::AddressTooLong);
}

/// The caller compares the length with the family. The decoder only reports the
/// content of the reply.
#[test]
fn a_short_address_is_reported_verbatim_not_rejected() {
    let reply = r#"{"parameters":{"addresses":[{"family":2,"address":[127,0,1]}]}}"#;
    let decoded = addresses(reply);
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].octets(), &[127, 0, 1]);
}

#[test]
fn an_empty_address_array_is_reported_verbatim() {
    let reply = r#"{"parameters":{"addresses":[{"family":2,"address":[]}]}}"#;
    let decoded = addresses(reply);
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].octets(), &[] as &[u8]);
}

#[test]
fn address_entries_missing_a_required_member_are_rejected() {
    for reply in [
        r#"{"parameters":{"addresses":[{"ifindex":2,"address":[127,0,0,1]}]}}"#,
        r#"{"parameters":{"addresses":[{"ifindex":2,"family":2}]}}"#,
        r#"{"parameters":{"addresses":[{}]}}"#,
    ] {
        assert_eq!(
            rejection(reply),
            ReplyErrorKind::MissingMember,
            "expected a missing-member rejection for {reply:?}"
        );
    }
}

/// A Varlink error name is an interface-qualified identifier. An escape
/// sequence in such a name shows that the reply is not what it declares.
#[test]
fn an_escaped_error_name_is_rejected() {
    let reply = r#"{"error":"io.systemd.Resolve.No\u0053uchResourceRecord","parameters":{}}"#;
    assert_eq!(rejection(reply), ReplyErrorKind::UnexpectedEscape);
}

/// A well-formed address reply reaches exactly `MAX_DEPTH`. A lower limit would
/// therefore reject a real reply.
#[test]
fn a_real_address_reply_reaches_exactly_the_depth_limit() {
    assert_eq!(MAX_DEPTH, 5);
    let reply = r#"{"parameters":{"addresses":[{"family":2,"address":[127,0,0,1]}]}}"#;
    assert_eq!(addresses(reply).len(), 1);
}

#[test]
fn nesting_beyond_the_depth_limit_is_rejected() {
    let at_limit = r#"{"parameters":{"addresses":[]},"extra":{"a":{"b":{"c":{"d":null}}}}}"#;
    assert_eq!(decode(at_limit).unwrap(), vec![]);

    let beyond = r#"{"parameters":{"addresses":[]},"extra":{"a":{"b":{"c":{"d":{"e":null}}}}}}"#;
    assert_eq!(rejection(beyond), ReplyErrorKind::DepthExceeded);
}

#[test]
fn decoding_stops_after_reporting_an_error() {
    let mut reply = ResolveHostnameReply::new(r#"{"parameters":{"addresses":[null,null]}}"#);
    let mut record = Record::default();
    assert!(reply.drive(&mut record).is_err());
    assert_eq!(reply.drive(&mut record), Ok(None));
    assert_eq!(reply.drive(&mut record), Ok(None));
}

#[test]
fn errors_carry_an_offset_inside_the_reply() {
    let reply = r#"{"parameters":{"addresses":[{"family":"2","address":[1,2,3,4]}]}}"#;
    let error = decode(reply).unwrap_err();
    assert!(
        error.offset() <= reply.len(),
        "offset {} escaped a {}-byte reply",
        error.offset(),
        reply.len()
    );
    assert!(error.offset() > 0);
}

#[test]
fn every_captured_sample_decodes() {
    for reply in SAMPLES {
        assert!(decode(reply).is_ok(), "the sample must decode: {reply:?}");
    }
}

#[test]
fn a_reply_the_size_of_the_wire_limit_is_handled() {
    let mut reply = String::from(r#"{"parameters":{"addresses":["#);
    let entry = r#"{"ifindex":2,"family":2,"address":[127,0,0,1]}"#;
    let mut count = 0;
    while reply.len() + entry.len() + 8 < 64 * 1024 {
        if count != 0 {
            reply.push(',');
        }
        reply.push_str(entry);
        count += 1;
    }
    reply.push_str("]}}");

    assert_eq!(addresses(&reply).len(), count);
}

#[test]
fn decoding_borrows_from_the_reply_without_copying_it() {
    /// Stops the decoder at the canonical name and returns that name.
    struct FirstName;

    impl<'a> ReplyVisitor<'a> for FirstName {
        type Output = &'a str;

        fn name(&mut self, name: &'a str) -> Option<&'a str> {
            Some(name)
        }

        fn escape_name(&mut self, _: EscapeString<'a>) -> Option<&'a str> {
            None
        }
    }

    // The decoder holds no storage of its own. An item therefore has a longer
    // life than the decoder that produced it, for as long as the reply buffer
    // exists. The decoder here is a temporary value, and the name that it
    // supplies has a longer life.
    fn first_name(reply: &str) -> &str {
        ResolveHostnameReply::new(reply)
            .drive(&mut FirstName)
            .expect("the reply must decode")
            .expect("the reply had no name")
    }

    let reply = String::from(r#"{"parameters":{"addresses":[],"name":"example.com"}}"#);
    assert_eq!(first_name(&reply), "example.com");
}

#[test]
fn reply_errors_display_usefully() {
    let error = decode(r#"{"parameters":[]}"#).unwrap_err();
    let rendered = error.to_string();
    assert!(!rendered.is_empty());
    assert!(
        std::error::Error::source(&error).is_none(),
        "ReplyError must be self-contained"
    );
}
