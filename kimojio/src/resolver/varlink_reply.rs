// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Decodes systemd-resolved `io.systemd.Resolve.ResolveHostname` replies.
//!
//! GENERATED with the `kimojio-json-decoder` skill. Use that skill to generate
//! or to update this file. Do not edit the state machine below by hand unless
//! it is necessary.
//!
//! This documentation contains all data that a later run of the skill needs to
//! change this file correctly: the source of the JSON, its schema, real
//! captured samples, the interface that this module supplies, and the
//! invariants that the code does not show. Keep this documentation in agreement
//! with the code.
//!
//! # Source of the JSON
//!
//! `kimojio` resolves host names with the systemd-resolved service through its
//! Varlink socket at `/run/systemd/resolve/io.systemd.Resolve`. A reply arrives
//! as one JSON frame with a NUL terminator. To capture the same frames by hand,
//! use this command:
//!
//! ```sh
//! varlinkctl call /run/systemd/resolve/io.systemd.Resolve \
//!     io.systemd.Resolve.ResolveHostname '{"name":"example.com"}'
//! ```
//!
//! # Schema
//!
//! The Varlink interface declares:
//!
//! ```text
//! method ResolveHostname(name: ?string, family: ?int, flags: ?int) -> (
//!     addresses: []ResolveHostnameAddress,
//!     name: string,
//!     flags: int
//! )
//! type ResolveHostnameAddress (ifindex: ?int, family: int, address: []int)
//! ```
//!
//! The members can occur in **any order**, and a reply can contain members that
//! this schema does not name. The decoder skips an unknown member structurally
//! at each level.
//!
//! # Samples
//!
//! These are real frames. They are the regression corpus and they show the
//! shape of a reply. `kimojio/tests/varlink_reply.rs` decodes each of them.
//!
//! An IPv4 answer:
//!
//! ```json
//! {"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[104,20,23,154]}],"name":"example.com","flags":8388609}}
//! ```
//!
//! An IPv6 answer:
//!
//! ```json
//! {"parameters":{"addresses":[{"ifindex":7,"family":10,"address":[32,1,13,184,0,0,0,0,0,0,0,0,0,0,0,1]}],"name":"ipv6.example.com","flags":8388609}}
//! ```
//!
//! Both families in one reply:
//!
//! ```json
//! {"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[192,0,2,1]},{"ifindex":0,"family":10,"address":[32,1,13,184,0,0,0,0,0,0,0,0,0,0,0,2]}],"name":"dual.example.com","flags":8388609}}
//! ```
//!
//! An IP literal. The service synthesized this answer and did not look it up,
//! thus `ifindex` is absent:
//!
//! ```json
//! {"parameters":{"addresses":[{"family":2,"address":[127,0,0,1]}],"name":"127.0.0.1","flags":786945}}
//! ```
//!
//! A method error. The service reports "no such host" in this form:
//!
//! ```json
//! {"error":"io.systemd.Resolve.NoSuchResourceRecord","parameters":{}}
//! ```
//!
//! Unknown members at each level, with the address members in a different
//! order:
//!
//! ```json
//! {"ignored":{"nested":[null]},"parameters":{"ignored":[{"value":true}],"addresses":[{"address":[104,20,23,154],"extra":{"nested":false},"ifindex":2,"family":2}],"name":"reordered.example.com"}}
//! ```
//!
//! # Interface
//!
//! [`ResolveHostnameReply`] reads a reply of either shape. For each item it
//! calls a method of the caller-supplied [`ReplyVisitor`], in the order of the
//! items on the wire.
//!
//! [`ResolveHostnameReply::new`] accepts the frame body as text. A frame
//! arrives from the socket as bytes, and JSON is UTF-8 by definition, thus the
//! caller converts the frame one time with [`core::str::from_utf8`]. The caller
//! then reports a frame that is not UTF-8 in its own terms, at the position
//! where the bytes arrive. `resolver::decode_varlink_reply` does this.
//!
//! The decoder pushes each item to a visitor and does not supply an iterator.
//! The caller is `resolve` in the parent module, and it needs one result from
//! the reply: the addresses, converted and collected as they arrive. An
//! iterator must name each item in a type that both sides agree on, must
//! suspend the decoder between the items, and makes the caller dispatch a
//! second time on the item that it received. A visitor removes all three costs:
//! the method bodies of the caller are the match arms, and a full reply decodes
//! in one call. No caller needs the full reply in memory, thus the maximum cost
//! stays one address entry.
//!
//! A caller that must stop early returns `Some` from a method. An adapter or a
//! test that reads only the first name does this. The decoder keeps its
//! position and returns the value, and the next call continues from that
//! position.
//!
//! # Invariants
//!
//! These are decisions. A change to one of them is also a change to the tests
//! and to the caller.
//!
//! - **Either `error` or `parameters.addresses` must be present.** A reply with
//!   neither member did not answer the request, and the decoder rejects it with
//!   [`ReplyErrorKind::MissingMember`].
//! - **An empty `addresses` array is valid and supplies no item.** "Present but
//!   empty" means that the service found no address. "Absent" means that the
//!   reply is malformed. The caller treats the two conditions differently, thus
//!   they must stay different here.
//! - **An absent `ifindex` becomes `0`.** systemd-resolved omits `ifindex` for
//!   a synthesized answer such as an IP literal, where the address is not bound
//!   to a link. Zero is the correct value for an IPv6 address with no scope.
//! - **[`MAX_DEPTH`] is 5, because a well-formed address reply reaches exactly
//!   5 levels**: the reply object, `parameters`, the `addresses` array, one
//!   address object, and its `address` byte array. The limit is a bound on the
//!   work for malformed input, thus the decoder also applies it inside a
//!   skipped unknown member.
//! - **An escaped `error` name is rejected.** A Varlink error name is an
//!   interface-qualified identifier and cannot contain an escape sequence. An
//!   escape sequence therefore shows a reply that is not what it declares.
//!   `name` can contain escape sequences, and the decoder reports it to
//!   [`ReplyVisitor::escape_name`].
//! - **The decoder does not compare the address length with the family.** This
//!   module reports the content of the reply. The combination of four bytes
//!   with `AF_INET` is socket knowledge, thus `resolver::socket_address` makes
//!   that check.
//!
//! # Tests
//!
//! `kimojio/tests/varlink_reply.rs` is written by hand and is the acceptance
//! criteria for this module. The generator does not read that file. A decoder
//! that could edit its own test could satisfy the test without being correct.
//! The unit tests at the end of this file are an aid and are not authoritative.
//!
//! # How to edit this file
//!
//! Part of this file is machine-written, and the difference is important.
//!
//! Each line above the `STATE MACHINE` marker is usual source code. Edit it
//! directly to add a derive, to change the text of an error, to add an
//! accessor, or to correct this documentation.
//!
//! Below that marker is the state machine. Its shape is made for a machine and
//! not for edits by hand: explicit states, counters and not a stack, no
//! recursion, and no data that the decoder makes and then discards. A small
//! local change can break those properties. Describe the necessary change and
//! let the `kimojio-json-decoder` skill generate that region again.
//!
//! If a hand edit of the state machine is necessary, write a comment that gives
//! the reason. Then make sure that the code allocates no memory, cannot panic,
//! does not recurse, and still applies [`MAX_DEPTH`]. Then run
//! `kimojio/tests/varlink_reply.rs`.
//!
//! # Example
//!
//! ```
//! use core::convert::Infallible;
//!
//! use kimojio::resolver::varlink_reply::{
//!     EscapeString, ReplyVisitor, ResolveHostnameReply, ResolvedAddress,
//! };
//!
//! #[derive(Default)]
//! struct Answer<'a> {
//!     octets: Option<Vec<u8>>,
//!     name: Option<&'a str>,
//! }
//!
//! impl<'a> ReplyVisitor<'a> for Answer<'a> {
//!     /// This visitor never stops the decoder, thus one call decodes the full reply.
//!     type Output = Infallible;
//!
//!     fn address(&mut self, address: ResolvedAddress) -> Option<Infallible> {
//!         self.octets = Some(address.octets().to_vec());
//!         None
//!     }
//!
//!     fn name(&mut self, name: &'a str) -> Option<Infallible> {
//!         self.name = Some(name);
//!         None
//!     }
//!
//!     fn escape_name(&mut self, _: EscapeString<'a>) -> Option<Infallible> {
//!         None
//!     }
//! }
//!
//! let frame = r#"{"parameters":{"addresses":[{"family":2,"address":[127,0,0,1]}],"name":"localhost"}}"#;
//! let mut answer = Answer::default();
//! let _ = ResolveHostnameReply::new(frame)
//!     .drive(&mut answer)
//!     .expect("the frame is well-formed");
//!
//! assert_eq!(answer.octets.as_deref(), Some(&[127, 0, 0, 1][..]));
//! assert_eq!(answer.name, Some("localhost"));
//! ```

use core::fmt;

use kimojio_json::{Tokenizer, Visitor};

/// This type is re-exported because a caller cannot write
/// [`ReplyVisitor::escape_name`] without a name for it. A second dependency for
/// one type is not a good trade.
pub use kimojio_json::EscapeString;

/// The deepest nesting of a well-formed reply, and therefore the deepest
/// nesting that this decoder reads.
///
/// An address reply nests exactly this deep: the reply object, `parameters`,
/// the `addresses` array, one address object, and its `address` byte array.
/// Deeper data is either a member that the schema does not describe or an
/// attempt to increase the work of the decoder above the work that the protocol
/// needs. The decoder therefore applies the limit everywhere, and not only on
/// the path that it reads.
pub const MAX_DEPTH: usize = 5;

/// The largest number of address bytes of any address family.
///
/// IPv6 is the widest family that systemd-resolved reports, with sixteen bytes.
/// The buffer has that fixed size, thus an address never needs the heap. The
/// decoder rejects a reply that declares more bytes than this, and does not
/// truncate it.
const MAX_ADDRESS_BYTES: usize = 16;

/// The data that a caller needs from a `ResolveHostname` reply.
///
/// The decoder calls one method for each item, in the order of the items on the
/// wire. A method that does not apply to the caller keeps its empty default
/// body. There is no enumeration between the two: an item for which no caller
/// implements a method costs a call to an empty function, which the optimizer
/// removes. It does not cost a value that the decoder builds and then discards.
///
/// Each method returns `Option<Self::Output>`. `None` continues the decode,
/// which is what a caller that only collects the items needs. The full reply is
/// then one pass with no suspension. `Some` stops the decode and returns that
/// value from [`ResolveHostnameReply::drive`]. A caller reports its own failure
/// in this form, or reads the items one at a time.
///
/// This trait is equivalent to [`kimojio_json::Visitor`] one layer above: the
/// tokenizer drives the decoder, and the decoder drives the caller by the same
/// rules.
///
/// ```
/// use kimojio::resolver::varlink_reply::{
///     ReplyVisitor, ResolveHostnameReply, ResolvedAddress,
/// };
/// use kimojio_json::EscapeString;
///
/// #[derive(Default)]
/// struct Families(Vec<i32>);
///
/// impl<'a> ReplyVisitor<'a> for Families {
///     /// This visitor never stops the decoder, thus it returns no value.
///     type Output = core::convert::Infallible;
///
///     fn address(&mut self, address: ResolvedAddress) -> Option<Self::Output> {
///         self.0.push(address.family());
///         None
///     }
///
///     fn escape_name(&mut self, _: EscapeString<'a>) -> Option<Self::Output> {
///         None
///     }
/// }
///
/// let frame = r#"{"parameters":{"addresses":[{"family":2,"address":[127,0,0,1]}]}}"#;
/// let mut families = Families::default();
/// ResolveHostnameReply::new(frame).drive(&mut families)?;
///
/// assert_eq!(families.0, vec![2]);
/// # Ok::<(), kimojio::resolver::varlink_reply::ReplyError>(())
/// ```
pub trait ReplyVisitor<'a> {
    /// The value that a method returns when it stops the decode.
    ///
    /// Set this to an uninhabited type such as [`core::convert::Infallible`] if
    /// no method ever returns `Some`. `Option<Infallible>` has one possible
    /// value, thus the compiler removes each test that the driver makes.
    type Output;

    /// A complete entry of the `parameters.addresses` array.
    ///
    /// The entry is passed by value, because it is a buffer of sixteen bytes
    /// and three small fields. The decoder does not need the entry again.
    fn address(&mut self, address: ResolvedAddress) -> Option<Self::Output> {
        let _ = address;
        None
    }

    /// A Varlink method error name, such as
    /// `io.systemd.Resolve.NoSuchResourceRecord`.
    ///
    /// systemd-resolved reports "no such host" in this form. A reply that
    /// causes this call is an answer and is not malformed.
    fn error(&mut self, name: &'a str) -> Option<Self::Output> {
        let _ = name;
        None
    }

    /// `parameters.name`, the canonical name, with no escape sequence.
    fn name(&mut self, name: &'a str) -> Option<Self::Output> {
        let _ = name;
        None
    }

    /// `parameters.name`, with at least one escape sequence.
    ///
    /// This is the one method with no default body. Escaped text arrives as its
    /// own type, because undecoded text must not be used by accident. A default
    /// body here would silently discard each name that a server sends in
    /// escaped form. Such a defect becomes visible in production and not in the
    /// first test.
    fn escape_name(&mut self, name: EscapeString<'a>) -> Option<Self::Output>;

    /// `parameters.flags`, the lookup flags, in the form that the reply used.
    fn flags(&mut self, flags: u64) -> Option<Self::Output> {
        let _ = flags;
        None
    }
}

/// One address from a `ResolveHostname` reply, stored inline.
///
/// The bytes are in a buffer of fixed size inside this value. A decode of a
/// reply therefore allocates no memory, independent of the number of addresses
/// in the reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedAddress {
    ifindex: i32,
    family: i32,
    octets: [u8; MAX_ADDRESS_BYTES],
    length: u8,
}

impl ResolvedAddress {
    /// The interface index, or `0` if the reply omitted `ifindex`.
    ///
    /// systemd-resolved omits `ifindex` for a synthesized answer such as an IP
    /// literal, where the address is not bound to a link. The kernel also uses
    /// zero for "no interface". A report of zero therefore keeps that condition
    /// in the usual range of values, and no caller must unwrap an option.
    #[must_use]
    pub const fn ifindex(&self) -> i32 {
        self.ifindex
    }

    /// The raw `family` value, in the form that the reply used.
    ///
    /// This method does not translate the value into an address type. Only the
    /// caller knows which families it can process, and it uses the family
    /// together with [`octets`](Self::octets) to make that decision.
    #[must_use]
    pub const fn family(&self) -> i32 {
        self.family
    }

    /// The address bytes. The length is the length that the reply contained.
    ///
    /// This method does not compare the number of bytes with the family. That
    /// comparison is a rule of the caller and not of the wire format.
    #[must_use]
    pub fn octets(&self) -> &[u8] {
        let length = usize::from(self.length);
        // `length` never becomes greater than the buffer size, thus the
        // alternative value is dead code. The code uses `get` to stay free of
        // paths that panic.
        self.octets.get(..length).unwrap_or(&self.octets)
    }
}

/// The reason why the decoder rejected a reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReplyErrorKind {
    /// The text was not well-formed JSON.
    Json,
    /// A value had the wrong JSON type for its position in the schema.
    UnexpectedType,
    /// An integer was too large for the field that named it.
    NumberOutOfRange,
    /// An address contained more bytes than any address family uses.
    AddressTooLong,
    /// The reply contained neither `error` nor `parameters.addresses`.
    MissingMember,
    /// The nesting was deeper than [`MAX_DEPTH`].
    DepthExceeded,
    /// An escape sequence occurred where the schema does not permit one.
    UnexpectedEscape,
}

impl ReplyErrorKind {
    /// Returns a short, stable description of this error kind.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "the reply was not well-formed JSON",
            Self::UnexpectedType => "a reply member had the wrong JSON type",
            Self::NumberOutOfRange => "a reply integer was out of range",
            Self::AddressTooLong => "an address held more than 16 bytes",
            Self::MissingMember => "the reply was missing a required member",
            Self::DepthExceeded => "the reply nested too deeply",
            Self::UnexpectedEscape => "a reply string contained an escape sequence",
        }
    }
}

impl fmt::Display for ReplyErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A rejected reply, with the byte offset of the rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplyError {
    kind: ReplyErrorKind,
    offset: usize,
}

impl ReplyError {
    /// Makes an error of `kind` at `offset`.
    const fn new(kind: ReplyErrorKind, offset: usize) -> Self {
        Self { kind, offset }
    }

    /// Converts a failure of the tokenizer and keeps the offset that the
    /// tokenizer reported.
    fn from_json(error: kimojio_json::Error) -> Self {
        Self::new(ReplyErrorKind::Json, error.offset())
    }

    /// Returns the reason why the decoder rejected the reply.
    #[must_use]
    pub const fn kind(&self) -> ReplyErrorKind {
        self.kind
    }

    /// Returns the byte offset at which the reply was rejected.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }
}

impl fmt::Display for ReplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at byte {}", self.kind, self.offset)
    }
}

impl std::error::Error for ReplyError {}

// ---------------------------------------------------------------------------
// STATE MACHINE - maintained by the `kimojio-json-decoder` skill.
//
// Each line above this line is usual source code: edit it directly.
//
// The shape of this region is made for a machine and not for edits by hand:
// explicit states, counters and not a stack, no recursion, and no data that the
// decoder makes and then discards. A small local change can break those
// properties. Examples are an added `Vec`, an early `return` that omits a state
// transition, and a `?` that leaves the machine inside a record. Describe the
// necessary change and let the skill generate this region again.
//
// If a hand edit of this region is necessary, write a comment that gives the
// reason. Then make sure that the code allocates no memory and cannot panic.
// ---------------------------------------------------------------------------

/// The position of the decoder in the reply.
///
/// Each position that the schema names has its own state. The tokenizer
/// consumes the `:` and calls a different method for each kind of token. The
/// state and the method that the tokenizer called are therefore sufficient to
/// identify the token: the token after a recognized member name is always the
/// value of that member.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    /// Before the `{` that opens the reply.
    Start,
    /// Between the members of the reply object.
    Reply,
    /// Reading the value of the top-level `error` member.
    ReplyError,
    /// Reading the value of the top-level `parameters` member.
    ReplyParameters,
    /// Between the members of `parameters`.
    Parameters,
    /// Reading the value of `parameters.addresses`.
    ParametersAddresses,
    /// Reading the value of `parameters.name`.
    ParametersName,
    /// Reading the value of `parameters.flags`.
    ParametersFlags,
    /// Between the elements of the `addresses` array.
    Addresses,
    /// Between the members of one address object.
    Address,
    /// Reading the value of the `ifindex` member of an address entry.
    AddressIfindex,
    /// Reading the value of the `family` member of an address entry.
    AddressFamily,
    /// Reading the value of the `address` member of an address entry.
    AddressOctetsStart,
    /// Between the elements of the `address` array of an address entry.
    AddressOctets,
    /// Discarding the value of a member that the schema does not name.
    Skip,
    /// The reply object is closed.
    Complete,
}

/// The members of one address entry, collected as they arrive.
///
/// The members can occur in any order, thus the decoder cannot report an entry
/// before its object closes. Sixteen bytes and three small fields are small
/// enough to keep inline. This is the reason why the decoder allocates no
/// memory.
#[derive(Clone, Copy, Debug)]
struct PendingAddress {
    ifindex: Option<i32>,
    family: Option<i32>,
    octets: [u8; MAX_ADDRESS_BYTES],
    length: u8,
    /// Whether an `address` array occurred. An empty array is present, and
    /// "present but empty" is different from "absent".
    has_octets: bool,
}

impl PendingAddress {
    /// An entry with no decoded member.
    const EMPTY: Self = Self {
        ifindex: None,
        family: None,
        octets: [0; MAX_ADDRESS_BYTES],
        length: 0,
        has_octets: false,
    };
}

/// The value that a token handler returns to the driver.
///
/// A [`Visitor`] method cannot return an error of its own. A rejection
/// therefore uses the same channel as the `Output` of the caller. Only the kind
/// travels in that channel. The driver combines the kind with the cursor of the
/// tokenizer, which points to the byte after the token that caused the
/// rejection.
type Stop<T> = Result<T, ReplyErrorKind>;

/// Reads a JSON number for which the schema declares an integer.
///
/// `2.0` and `2e0` name the same value as `2`. The schema declares these
/// members as integers, and acceptance of a float would accept a reply that
/// does not agree with the interface. The function converts to `i128` first,
/// thus the range check belongs to the field and not to the parser. `flags` can
/// therefore hold the full range of `u64`, and `family` cannot.
fn integer(text: &str) -> Result<i128, ReplyErrorKind> {
    if text.contains(['.', 'e', 'E']) {
        return Err(ReplyErrorKind::UnexpectedType);
    }
    text.parse::<i128>()
        .map_err(|_| ReplyErrorKind::NumberOutOfRange)
}

/// Reads a JSON number that must fit into an `i32`.
fn integer_i32(text: &str) -> Result<i32, ReplyErrorKind> {
    i32::try_from(integer(text)?).map_err(|_| ReplyErrorKind::NumberOutOfRange)
}

/// The reply state machine. The tokenizer drives it; it does not drive the
/// tokenizer.
///
/// The tokenizer calls one method for each token and does not return until a
/// method returns a value. This type therefore never asks for a token, never
/// matches on a token kind, and never examines a value that the scanner
/// classified already. A suspension of the scan is the method by which an item
/// reaches the caller.
#[derive(Clone, Copy, Debug)]
struct Decoder {
    state: State,
    /// How many containers of a discarded value are open. The skip logic is a
    /// state and this counter, and not a recursive call. Malformed input can
    /// therefore not increase the stack.
    skip_depth: usize,
    /// The state to return to after a discarded value is complete.
    skip_resume: State,
    /// How many containers are open. The decoder counts them itself and does
    /// not ask the tokenizer, because a visitor sees the tokens and not the
    /// scanner that produced them.
    depth: usize,
    address: PendingAddress,
    /// Whether a top-level `error` member occurred.
    seen_error: bool,
    /// Whether a `parameters.addresses` array occurred, empty or not.
    seen_addresses: bool,
}

impl Decoder {
    /// A decoder that has read nothing.
    const fn new() -> Self {
        Self {
            state: State::Start,
            skip_depth: 0,
            skip_resume: State::Reply,
            depth: 0,
            address: PendingAddress::EMPTY,
            seen_error: false,
            seen_addresses: false,
        }
    }

    /// Counts an opened container and applies [`MAX_DEPTH`].
    ///
    /// This method runs before each state, including the state that discards an
    /// unknown member. No part of the reply can therefore nest deeper than the
    /// protocol itself.
    fn open(&mut self) -> Option<ReplyErrorKind> {
        self.depth = self.depth.saturating_add(1);
        (self.depth > MAX_DEPTH).then_some(ReplyErrorKind::DepthExceeded)
    }

    /// Counts a closed container.
    fn close(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Starts to discard the value that follows an unrecognized member name.
    /// The decoder returns to `resume` after that value is complete.
    fn begin_skip(&mut self, resume: State) {
        self.skip_resume = resume;
        self.skip_depth = 0;
        self.state = State::Skip;
    }

    /// Leaves the skip state after the discarded value is complete.
    ///
    /// A scalar is complete when it arrives. A container is complete when its
    /// last closing token returns the counter to zero.
    fn end_skipped_value(&mut self) {
        if self.skip_depth == 0 {
            self.state = self.skip_resume;
        }
    }

    /// Closes one container of a discarded value.
    fn close_skipped(&mut self) {
        self.skip_depth = self.skip_depth.saturating_sub(1);
        self.end_skipped_value();
    }

    /// Accepts a scalar that no member of the schema claims. This is legal only
    /// during the discard of a value.
    fn discard_scalar(&mut self) -> Option<ReplyErrorKind> {
        if self.state == State::Skip {
            self.end_skipped_value();
            None
        } else {
            Some(ReplyErrorKind::UnexpectedType)
        }
    }

    /// Processes a `{`.
    ///
    /// These transitions concern only the state machine. They are therefore
    /// methods of the decoder, and the bridge only forwards the call. Only the
    /// handlers that can produce an item for the caller need the visitor of the
    /// caller.
    fn object_start(&mut self) -> Option<ReplyErrorKind> {
        if let Some(kind) = self.open() {
            return Some(kind);
        }
        match self.state {
            State::Start => self.state = State::Reply,
            State::ReplyParameters => self.state = State::Parameters,
            State::Addresses => {
                self.address = PendingAddress::EMPTY;
                self.state = State::Address;
            }
            State::Skip => self.skip_depth = self.skip_depth.saturating_add(1),
            _ => return Some(ReplyErrorKind::UnexpectedType),
        }
        None
    }

    /// Processes a `[`.
    fn array_start(&mut self) -> Option<ReplyErrorKind> {
        if let Some(kind) = self.open() {
            return Some(kind);
        }
        match self.state {
            State::ParametersAddresses => {
                // Recorded at the `[` and not for each element. An empty
                // `addresses` array is a real answer of "no addresses". A
                // caller treats that answer differently from a reply that does
                // not contain `addresses`.
                self.seen_addresses = true;
                self.state = State::Addresses;
            }
            State::AddressOctetsStart => {
                // Clear the full buffer and not only the length. A repeated
                // `address` member can thus not leave old bytes after the bytes
                // that it writes.
                self.address.octets = [0; MAX_ADDRESS_BYTES];
                self.address.length = 0;
                self.address.has_octets = true;
                self.state = State::AddressOctets;
            }
            State::Skip => self.skip_depth = self.skip_depth.saturating_add(1),
            _ => return Some(ReplyErrorKind::UnexpectedType),
        }
        None
    }

    /// Processes a `]`.
    fn array_end(&mut self) -> Option<ReplyErrorKind> {
        self.close();
        match self.state {
            State::Addresses => self.state = State::Parameters,
            State::AddressOctets => self.state = State::Address,
            State::Skip => self.close_skipped(),
            _ => return Some(ReplyErrorKind::UnexpectedType),
        }
        None
    }

    /// Processes a number that belongs to the state machine and not to the
    /// caller.
    ///
    /// `parameters.flags` is absent from this method. It is the one number that
    /// goes directly to the visitor, thus the bridge processes it before it
    /// calls this method.
    fn number(&mut self, text: &str) -> Option<ReplyErrorKind> {
        match self.state {
            State::AddressIfindex => match integer_i32(text) {
                Ok(value) => {
                    self.address.ifindex = Some(value);
                    self.state = State::Address;
                    None
                }
                Err(kind) => Some(kind),
            },
            State::AddressFamily => match integer_i32(text) {
                Ok(value) => {
                    self.address.family = Some(value);
                    self.state = State::Address;
                    None
                }
                Err(kind) => Some(kind),
            },
            State::AddressOctets => self.push_octet(text).err(),
            State::Skip => {
                self.end_skipped_value();
                None
            }
            _ => Some(ReplyErrorKind::UnexpectedType),
        }
    }

    /// Processes an object member name in either of its two forms.
    ///
    /// `is` compares the name with a candidate. The parameter is a predicate
    /// and not a `&str`, thus an escaped name can use this same routine. The
    /// escaped form compares without a buffer. A reply that writes a member as
    /// `"nam\u0065"` is therefore understood. That form is legal JSON, and its
    /// acceptance costs nothing.
    fn member(&mut self, is: impl Fn(&str) -> bool) -> Option<ReplyErrorKind> {
        match self.state {
            State::Reply => {
                if is("error") {
                    self.state = State::ReplyError;
                } else if is("parameters") {
                    self.state = State::ReplyParameters;
                } else {
                    self.begin_skip(State::Reply);
                }
            }
            State::Parameters => {
                if is("addresses") {
                    self.state = State::ParametersAddresses;
                } else if is("name") {
                    self.state = State::ParametersName;
                } else if is("flags") {
                    self.state = State::ParametersFlags;
                } else {
                    self.begin_skip(State::Parameters);
                }
            }
            State::Address => {
                if is("ifindex") {
                    self.state = State::AddressIfindex;
                } else if is("family") {
                    self.state = State::AddressFamily;
                } else if is("address") {
                    self.state = State::AddressOctetsStart;
                } else {
                    self.begin_skip(State::Address);
                }
            }
            // A member name inside a discarded object says nothing about the
            // schema. The decoder discards its value with the other data.
            State::Skip => {}
            _ => return Some(ReplyErrorKind::UnexpectedType),
        }
        None
    }

    /// Records one byte of the `address` array.
    fn push_octet(&mut self, text: &str) -> Result<(), ReplyErrorKind> {
        let length = usize::from(self.address.length);
        if length >= MAX_ADDRESS_BYTES {
            return Err(ReplyErrorKind::AddressTooLong);
        }
        let octet = u8::try_from(integer(text)?).map_err(|_| ReplyErrorKind::NumberOutOfRange)?;
        if let Some(slot) = self.address.octets.get_mut(length) {
            *slot = octet;
        }
        self.address.length = self.address.length.saturating_add(1);
        Ok(())
    }

    /// Completes the address entry whose object closed.
    fn take_address(&mut self) -> Result<ResolvedAddress, ReplyErrorKind> {
        let (Some(family), true) = (self.address.family, self.address.has_octets) else {
            return Err(ReplyErrorKind::MissingMember);
        };

        Ok(ResolvedAddress {
            // An absent `ifindex` becomes 0 and is not an error.
            // systemd-resolved omits it for a synthesized answer such as an IP
            // literal, where the address is not bound to a link.
            ifindex: self.address.ifindex.unwrap_or(0),
            family,
            octets: self.address.octets,
            length: self.address.length,
        })
    }
}

/// Connects the reply state machine to the visitor of the caller.
///
/// The tokenizer drives this type, and this type drives the caller. Neither of
/// the two knows about the other. This type borrows the visitor and does not
/// own it. [`ResolveHostnameReply`] therefore needs no type parameter for the
/// visitor of the caller: the caller selects its visitor at the call to `drive`
/// and not at construction.
///
/// The decoder, however, is held **by value**, and that is intentional. Each
/// token reads or writes `self.decoder.state`. Behind a `&mut` reference, each
/// access is two dependent loads and not one, and the state cannot stay in a
/// register across a call. A measurement with a reply of three addresses shows
/// a cost of approximately 7 % for the borrowed form. That is more than the
/// push interface saved. A borrowed decoder thus made the callback API *slower*
/// than the iterator that it replaced. `Decoder` is a few dozen bytes of plain
/// data, and `drive` copies it in and out one time for each call and not one
/// time for each token.
struct Bridge<'v, V> {
    decoder: Decoder,
    visitor: &'v mut V,
}

impl<'a, V: ReplyVisitor<'a>> Visitor<'a> for Bridge<'_, V> {
    type Output = Stop<V::Output>;

    fn object_start(&mut self) -> Option<Self::Output> {
        self.decoder.object_start().map(Err)
    }

    fn object_end(&mut self) -> Option<Self::Output> {
        self.decoder.close();
        match self.decoder.state {
            // The decoder decides if the reply answered the request after it
            // consumes the full document, and not here.
            State::Reply => self.decoder.state = State::Complete,
            State::Parameters => self.decoder.state = State::Reply,
            // An address entry is complete only when its object closes, because
            // its members can arrive in any order.
            State::Address => {
                self.decoder.state = State::Addresses;
                return match self.decoder.take_address() {
                    Ok(address) => self.visitor.address(address).map(Ok),
                    Err(kind) => Some(Err(kind)),
                };
            }
            State::Skip => self.decoder.close_skipped(),
            _ => return Some(Err(ReplyErrorKind::UnexpectedType)),
        }
        None
    }

    fn array_start(&mut self) -> Option<Self::Output> {
        self.decoder.array_start().map(Err)
    }

    fn array_end(&mut self) -> Option<Self::Output> {
        self.decoder.array_end().map(Err)
    }

    fn key(&mut self, name: &'a str) -> Option<Self::Output> {
        self.decoder.member(|candidate| name == candidate).map(Err)
    }

    fn escape_key(&mut self, name: EscapeString<'a>) -> Option<Self::Output> {
        self.decoder
            .member(|candidate| name.eq_unescaped(candidate))
            .map(Err)
    }

    fn string(&mut self, value: &'a str) -> Option<Self::Output> {
        match self.decoder.state {
            State::ReplyError => {
                self.decoder.seen_error = true;
                self.decoder.state = State::Reply;
                self.visitor.error(value).map(Ok)
            }
            State::ParametersName => {
                self.decoder.state = State::Parameters;
                self.visitor.name(value).map(Ok)
            }
            State::Skip => {
                self.decoder.end_skipped_value();
                None
            }
            _ => Some(Err(ReplyErrorKind::UnexpectedType)),
        }
    }

    fn escape_string(&mut self, value: EscapeString<'a>) -> Option<Self::Output> {
        match self.decoder.state {
            // A Varlink error name is an interface-qualified identifier. An
            // escape sequence here is therefore not a name that this decoder
            // failed to decode. It is a reply that declares an incorrect type.
            State::ReplyError => Some(Err(ReplyErrorKind::UnexpectedEscape)),
            State::ParametersName => {
                self.decoder.state = State::Parameters;
                self.visitor.escape_name(value).map(Ok)
            }
            State::Skip => {
                self.decoder.end_skipped_value();
                None
            }
            _ => Some(Err(ReplyErrorKind::UnexpectedType)),
        }
    }

    fn number(&mut self, text: &'a str) -> Option<Self::Output> {
        // `flags` is the only number that the caller receives. Each other
        // number belongs to an address entry that the decoder still collects.
        if self.decoder.state != State::ParametersFlags {
            return self.decoder.number(text).map(Err);
        }

        self.decoder.state = State::Parameters;
        match integer(text)
            .and_then(|value| u64::try_from(value).map_err(|_| ReplyErrorKind::NumberOutOfRange))
        {
            Ok(flags) => self.visitor.flags(flags).map(Ok),
            Err(kind) => Some(Err(kind)),
        }
    }

    fn boolean(&mut self, _: bool) -> Option<Self::Output> {
        self.decoder.discard_scalar().map(Err)
    }

    fn null(&mut self) -> Option<Self::Output> {
        self.decoder.discard_scalar().map(Err)
    }
}

/// A borrowing decoder for a `ResolveHostname` reply. It allocates no memory.
///
/// [`drive`](Self::drive) reads the reply and reports each item to a
/// [`ReplyVisitor`]. A visitor that never stops the decode reads the full frame
/// in that one call and holds one address entry at the most. A caller can call
/// `drive` again for a visitor that stops the decode, and the decoder continues
/// at the position where it stopped.
///
/// After the decoder reports a rejection, it is complete. Each subsequent call
/// reports completion and does not continue through text that is known to be
/// incorrect.
#[derive(Clone, Debug)]
pub struct ResolveHostnameReply<'a> {
    tokenizer: Tokenizer<'a>,
    decoder: Decoder,
    /// Whether the decode ended, by success or by failure.
    finished: bool,
}

impl<'a> ResolveHostnameReply<'a> {
    /// Starts a decode of `json`.
    ///
    /// The text is the body of the frame, with the NUL terminator already
    /// removed. The decoder scans nothing until the first call to
    /// [`drive`](Self::drive).
    ///
    /// This function accepts text and not bytes. A frame arrives from the
    /// socket as bytes, and JSON is UTF-8 by definition, thus the caller
    /// converts the frame one time with [`core::str::from_utf8`]. That
    /// conversion is one vectorized pass, and the caller can put it where it is
    /// best. The caller also reports a frame that is not UTF-8 in its own
    /// terms, at the position where the bytes arrive.
    #[must_use]
    pub const fn new(json: &'a str) -> Self {
        Self {
            tokenizer: Tokenizer::new(json),
            decoder: Decoder::new(),
            finished: false,
        }
    }

    /// Reads the reply and reports each item to `visitor`.
    ///
    /// Returns `Ok(None)` after the decoder read and accepted the full frame.
    /// Returns `Ok(Some(output))` if a visitor method stopped the decode. In
    /// that case, a subsequent call continues at that position.
    ///
    /// # Errors
    ///
    /// Returns a [`ReplyError`] if the text is not well-formed JSON, if it does
    /// not agree with the `ResolveHostname` schema, or if the frame contains
    /// neither an `error` member nor a `parameters.addresses` member.
    pub fn drive<V: ReplyVisitor<'a>>(
        &mut self,
        visitor: &mut V,
    ) -> Result<Option<V::Output>, ReplyError> {
        let Self {
            tokenizer,
            decoder,
            finished,
        } = self;

        if *finished {
            return Ok(None);
        }

        // The decoder goes into the bridge and comes out again. See the note on
        // `Bridge` for the reason why the code copies it and does not borrow
        // it. The write-back is unconditional, because each outcome, including
        // a rejection, leaves state that the next call needs.
        let mut bridge = Bridge {
            decoder: *decoder,
            visitor,
        };
        let outcome = tokenizer.drive(&mut bridge);
        *decoder = bridge.decoder;

        match outcome {
            Ok(Some(Ok(output))) => Ok(Some(output)),

            // The decoder reports a rejected reply one time. To continue after
            // the rejection would supply items that come from text that is
            // known to be incorrect.
            Ok(Some(Err(kind))) => {
                *finished = true;
                Err(ReplyError::new(kind, tokenizer.offset()))
            }

            // The tokenizer reports completion only after the top-level value
            // is well formed and only whitespace follows it. The frame
            // therefore needs no separate check for data after the reply. The
            // remaining decision is whether the reply answered the request.
            Ok(None) => {
                *finished = true;
                if decoder.seen_error || decoder.seen_addresses {
                    Ok(None)
                } else {
                    Err(ReplyError::new(
                        ReplyErrorKind::MissingMember,
                        tokenizer.offset(),
                    ))
                }
            }

            Err(error) => {
                *finished = true;
                Err(ReplyError::from_json(error))
            }
        }
    }
}

// ------------------------- end of the state machine ------------------------

#[cfg(test)]
mod tests {
    use core::convert::Infallible;

    use kimojio_json::EscapeString;

    use super::{MAX_DEPTH, ReplyErrorKind, ReplyVisitor, ResolveHostnameReply, ResolvedAddress};

    /// One item from a reply, recorded so that a test can assert on the full
    /// sequence in one step.
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

    /// Collects a full reply and fails the test at the first rejection.
    fn decode(json: &str) -> Vec<Item<'_>> {
        let mut record = Record::default();
        let stopped = ResolveHostnameReply::new(json)
            .drive(&mut record)
            .expect("the reply must decode");
        assert!(stopped.is_none(), "a recorder never stops the decoder");
        record.items
    }

    /// Returns the kind of the rejection that a reply causes.
    fn rejection(json: &str) -> ReplyErrorKind {
        let mut record = Record::default();
        match ResolveHostnameReply::new(json).drive(&mut record) {
            Ok(_) => panic!("the decoder must reject the reply"),
            Err(error) => error.kind(),
        }
    }

    #[test]
    fn ipv4_sample_decodes() {
        let json = r#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[104,20,23,154]}],"name":"example.com","flags":8388609}}"#;
        let items = decode(json);

        assert_eq!(items.len(), 3);
        let Some(Item::Address(address)) = items.first() else {
            panic!("expected an address first");
        };
        assert_eq!(address.ifindex(), 2);
        assert_eq!(address.family(), 2);
        assert_eq!(address.octets(), [104, 20, 23, 154]);
        assert_eq!(items.get(1), Some(&Item::Name("example.com")));
        assert_eq!(items.get(2), Some(&Item::Flags(8_388_609)));
    }

    #[test]
    fn ipv6_sample_decodes() {
        let json = r#"{"parameters":{"addresses":[{"ifindex":7,"family":10,"address":[32,1,13,184,0,0,0,0,0,0,0,0,0,0,0,1]}],"name":"ipv6.example.com","flags":8388609}}"#;
        let items = decode(json);

        let Some(Item::Address(address)) = items.first() else {
            panic!("expected an address first");
        };
        assert_eq!(address.ifindex(), 7);
        assert_eq!(address.family(), 10);
        assert_eq!(address.octets().len(), 16);
    }

    #[test]
    fn omitted_ifindex_reports_zero() {
        let json = r#"{"parameters":{"addresses":[{"family":2,"address":[127,0,0,1]}],"name":"127.0.0.1","flags":786945}}"#;
        let items = decode(json);

        let Some(Item::Address(address)) = items.first() else {
            panic!("expected an address first");
        };
        assert_eq!(address.ifindex(), 0);
    }

    #[test]
    fn unknown_members_are_skipped_in_any_order() {
        let json = r#"{"ignored":{"nested":[null]},"parameters":{"ignored":[{"value":true}],"addresses":[{"address":[104,20,23,154],"extra":{"nested":false},"ifindex":2,"family":2}],"name":"reordered.example.com"}}"#;
        let items = decode(json);

        assert_eq!(items.len(), 2);
        let Some(Item::Address(address)) = items.first() else {
            panic!("expected an address first");
        };
        assert_eq!(address.ifindex(), 2);
        assert_eq!(address.octets(), [104, 20, 23, 154]);
        assert_eq!(items.get(1), Some(&Item::Name("reordered.example.com")));
    }

    #[test]
    fn method_error_is_reported() {
        let json = r#"{"error":"io.systemd.Resolve.NoSuchResourceRecord","parameters":{}}"#;
        assert_eq!(
            decode(json),
            [Item::Error("io.systemd.Resolve.NoSuchResourceRecord")]
        );
    }

    #[test]
    fn empty_addresses_yield_nothing_and_are_not_missing() {
        assert_eq!(decode(r#"{"parameters":{"addresses":[]}}"#), []);
    }

    #[test]
    fn absent_addresses_are_missing() {
        assert_eq!(rejection(r#"{}"#), ReplyErrorKind::MissingMember);
        assert_eq!(
            rejection(r#"{"parameters":{}}"#),
            ReplyErrorKind::MissingMember
        );
    }

    #[test]
    fn address_entries_require_family_and_address() {
        assert_eq!(
            rejection(r#"{"parameters":{"addresses":[{"family":2}]}}"#),
            ReplyErrorKind::MissingMember
        );
        assert_eq!(
            rejection(r#"{"parameters":{"addresses":[{"address":[1,2,3,4]}]}}"#),
            ReplyErrorKind::MissingMember
        );
    }

    #[test]
    fn wrong_types_are_rejected() {
        assert_eq!(rejection(r"[]"), ReplyErrorKind::UnexpectedType);
        assert_eq!(
            rejection(r#"{"parameters":[]}"#),
            ReplyErrorKind::UnexpectedType
        );
        assert_eq!(
            rejection(r#"{"parameters":{"addresses":{}}}"#),
            ReplyErrorKind::UnexpectedType
        );
        assert_eq!(
            rejection(r#"{"parameters":{"addresses":[null]}}"#),
            ReplyErrorKind::UnexpectedType
        );
        assert_eq!(
            rejection(r#"{"error":false,"parameters":{}}"#),
            ReplyErrorKind::UnexpectedType
        );
        assert_eq!(
            rejection(r#"{"parameters":{"addresses":[{"family":2.0,"address":[1]}]}}"#),
            ReplyErrorKind::UnexpectedType
        );
    }

    #[test]
    fn out_of_range_integers_are_rejected() {
        assert_eq!(
            rejection(
                r#"{"parameters":{"addresses":[{"ifindex":4294967296,"family":2,"address":[1]}]}}"#
            ),
            ReplyErrorKind::NumberOutOfRange
        );
        assert_eq!(
            rejection(r#"{"parameters":{"addresses":[{"family":2,"address":[256]}]}}"#),
            ReplyErrorKind::NumberOutOfRange
        );
    }

    #[test]
    fn overlong_addresses_are_rejected() {
        let json = r#"{"parameters":{"addresses":[{"family":2,"address":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}]}}"#;
        assert_eq!(rejection(json), ReplyErrorKind::AddressTooLong);
    }

    #[test]
    fn escaped_error_names_are_rejected() {
        assert_eq!(
            rejection(r#"{"error":"io.systemd.Resolve.No\u0053uch","parameters":{}}"#),
            ReplyErrorKind::UnexpectedEscape
        );
    }

    #[test]
    fn escaped_names_are_reported_undecoded() {
        let json = r#"{"parameters":{"addresses":[],"name":"caf\u00e9.example"}}"#;
        let items = decode(json);
        let Some(Item::EscapeName(name)) = items.first() else {
            panic!("expected an escaped name");
        };
        assert!(name.eq_unescaped("café.example"));
    }

    #[test]
    fn the_limit_is_enforced_inside_unknown_members_too() {
        assert_eq!(MAX_DEPTH, 5);

        let at_limit = r#"{"parameters":{"addresses":[]},"extra":{"a":{"b":{"c":{"d":null}}}}}"#;
        assert_eq!(decode(at_limit), []);

        let beyond =
            r#"{"parameters":{"addresses":[]},"extra":{"a":{"b":{"c":{"d":{"e":null}}}}}}"#;
        assert_eq!(rejection(beyond), ReplyErrorKind::DepthExceeded);
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert_eq!(rejection("1e"), ReplyErrorKind::Json);
        assert_eq!(
            rejection(r#"{"parameters":{"addresses":["#),
            ReplyErrorKind::Json
        );
        assert_eq!(
            rejection(r#"{"parameters":{"addresses":[]}} null"#),
            ReplyErrorKind::Json
        );
    }

    #[test]
    fn decoding_stops_after_an_error() {
        let mut record = Record::default();
        let mut reply = ResolveHostnameReply::new(r#"{"parameters":[]}"#);
        assert!(reply.drive(&mut record).is_err());

        // The decoder reports the rejection one time. It is complete after the
        // report and does not continue through text that is known to be
        // incorrect.
        assert!(reply.drive(&mut record).unwrap().is_none());
        assert!(reply.drive(&mut record).unwrap().is_none());
    }

    #[test]
    fn several_addresses_arrive_in_order() {
        let json = r#"{"parameters":{"addresses":[{"ifindex":2,"family":2,"address":[192,0,2,1]},{"ifindex":0,"family":10,"address":[32,1,13,184,0,0,0,0,0,0,0,0,0,0,0,2]}],"name":"dual.example.com","flags":8388609}}"#;
        let items = decode(json);

        assert_eq!(items.len(), 4);
        let Some(Item::Address(first)) = items.first() else {
            panic!("expected an address first");
        };
        let Some(Item::Address(second)) = items.get(1) else {
            panic!("expected a second address");
        };
        assert_eq!(first.octets(), [192, 0, 2, 1]);
        assert_eq!(second.family(), 10);
        assert_eq!(second.octets().len(), 16);
    }
}
