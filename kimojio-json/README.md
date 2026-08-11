# kimojio-json

A JSON tokenizer. This crate is not a parser and not a deserializer.

The tokenizer reads a JSON document and calls one method of a caller-supplied
visitor for each token. Strings and numbers are slices that borrow from the
document. The crate has no DOM, no `serde` integration, no schema, and no
dependencies. It allocates no memory. It is `#![no_std]` and
`#![forbid(unsafe_code)]`.

```rust
use core::convert::Infallible;
use kimojio_json::{EscapeString, Tokenizer, Visitor};

#[derive(Default)]
struct Package<'a> {
    name: &'a str,
    size: u64,
    key: &'a str,
}

impl<'a> Visitor<'a> for Package<'a> {
    /// An uninhabited `Output` shows that this visitor never suspends the scan.
    type Output = Infallible;

    fn key(&mut self, name: &'a str) -> Option<Infallible> {
        self.key = name;
        None
    }

    fn string(&mut self, value: &'a str) -> Option<Infallible> {
        if self.key == "name" {
            self.name = value;
        }
        None
    }

    fn number(&mut self, text: &'a str) -> Option<Infallible> {
        if self.key == "size" {
            self.size = text.parse().unwrap_or(0);
        }
        None
    }

    fn escape_key(&mut self, _: EscapeString<'a>) -> Option<Infallible> { None }
    fn escape_string(&mut self, _: EscapeString<'a>) -> Option<Infallible> { None }
}

let mut package = Package::default();
let mut tokenizer = Tokenizer::new(r#"{"name":"kimojio","size":42}"#);
let suspended = tokenizer.drive(&mut package)?;

assert!(suspended.is_none(), "this visitor never suspends the tokenizer");
assert_eq!(package.name, "kimojio");
assert_eq!(package.size, 42);
# Ok::<(), kimojio_json::Error>(())
```

## Why the tokenizer calls the visitor

A pull interface, that is `next_token()` in a loop, is the usual shape. It also
costs more. For each token, the tokenizer must put the token into a value and
return that value through a call boundary. The caller must then examine the
value again with a `match`. The tokenizer must also be able to stop between any
two tokens. A Varlink reply has less than four bytes for each token. For such a
document, this dispatch costs several times more than the examination of the
bytes.

Therefore this crate has no `Token` type and no `next_token` method.
[`Tokenizer::drive`] accepts a [`Visitor`] and calls one method for each token.
The scanner keeps its cursor in a register. A visitor that uses only three token
kinds keeps the empty default body of the other methods, and the optimizer
removes them.

A caller can still stop the tokenizer. Each method returns
`Option<Self::Output>`, and a result of `Some` suspends the scan. `drive`
returns that value, and the next call to `drive` continues at the same position.
This makes an iterator possible above a push interface. It is also the reason
why `Output` is an associated type. A visitor that never suspends the scan sets
`Output` to an uninhabited type. `Option<Infallible>` then has one possible
value, and the compiler removes each suspension test in the driver.

Suspension records no resume point. The tokenizer commits the cursor, the state,
and the container stack before it calls the visitor. To continue, the tokenizer
only enters its loop again.

## Operations the tokenizer does not do

**The tokenizer does not expand escape sequences.** A string body is a subslice
of the input, and sequences such as `\n` and `\u00e9` keep their source form. An
expansion needs storage for the result, and this crate has no storage of its
own.

The tokenizer does validate each escape sequence, thus it rejects `"\q"` and
`"\u12"`. Only the expansion is deferred.

The method that the tokenizer calls tells the caller which strings need a
decode. There is no flag:

| JSON | Method |
| --- | --- |
| `"plain"` | `string("plain")` |
| `"caf\u00e9"` | `escape_string(..)` |
| `{"plain":1}` | `key("plain")` |
| `{"a\nb":1}` | `escape_key(..)` |

The usual case is therefore a plain `&str` that needs no decode. The escaped
case gets an `EscapeString`. That type has no `Deref` and no `PartialEq<str>`,
because undecoded text must not be used by accident. `escape_key` and
`escape_string` are the two methods with no default body. A visitor that omits
them is incorrect only for rare input, and such a defect can stay hidden until
the software is in production.

```rust
# use kimojio_json::{EscapeString, Tokenizer, Visitor};
/// Suspends at the first escaped string and gives it to the caller.
struct FirstEscape;

impl<'a> Visitor<'a> for FirstEscape {
    type Output = EscapeString<'a>;

    fn escape_key(&mut self, name: EscapeString<'a>) -> Option<EscapeString<'a>> { Some(name) }
    fn escape_string(&mut self, value: EscapeString<'a>) -> Option<EscapeString<'a>> { Some(value) }
}

let mut tokenizer = Tokenizer::new(r#""caf\u00e9""#);
let text = tokenizer.drive(&mut FirstEscape)?.expect("the string is escaped");

assert_eq!(text.as_raw_str(), r"caf\u00e9");   // not decoded
assert!(text.eq_unescaped("café"));             // a comparison that needs no buffer

let mut buffer = [0_u8; 16];                    // `text.raw_len()` is always sufficient
assert_eq!(text.unescape_into(&mut buffer)?, "café");
# Ok::<(), kimojio_json::Error>(())
```

**The tokenizer does not convert numbers.** The `number` method receives the raw
text. Only the caller knows if it must have a `u8`, an `i64`, or an `f64`. Only
the caller knows what to do with a value that is too large for its type. The
text always agrees with the JSON number grammar, thus `text.parse()` is safe.

## Validation

The tokenizer validates the full JSON grammar. It rejects each of these inputs
and reports the byte offset of the failure:

| Input | Error |
| --- | --- |
| `{"a":1` | `UnexpectedEof` |
| `[1,]` | `UnexpectedByte` |
| `{a:1}` | `ExpectedKey` |
| `{"a" 1}` | `ExpectedColon` |
| `01`, `1.`, `.5`, `1e` | `InvalidNumber` |
| `"abc` | `UnterminatedString` |
| a raw newline inside a string | `ControlCharacter` |
| `"\q"`, `"\u12"` | `InvalidEscape` |
| `{} {}` | `TrailingData` |
| 257 nested arrays | `DepthExceeded` |

`Tokenizer::new` accepts a `&str` and not a `&[u8]`. Therefore the table has no
`InvalidUtf8` row and `ErrorKind` has no such variant. The type makes that error
impossible.

This is an intentional trade. Safe Rust cannot make a `&str` from a `&[u8]`
without a validation. A tokenizer that holds bytes must therefore validate each
string and each number that it gives to the caller. That is a second pass over
bytes that the scanner read already, behind a call that the optimizer does not
inline. A tokenizer that holds text moves the validation into one vectorized
pass, and it moves the validation to the caller. The caller knows more than the
tokenizer: a caller that has a `&str` already pays nothing, and a caller that
holds a batch of frames can validate them together. For input with many strings
and numbers, this is a difference of 15 % to 25 %.

A caller that holds bytes writes `core::str::from_utf8(bytes)?` and reports the
failure in its own terms. JSON is UTF-8 by definition, thus bytes that are not
UTF-8 are not JSON. A report at the position where the bytes arrive is clearer
than a tokenizer error in the middle of a document.

The tokenizer records the nesting as one bit for each level in a `[u64; 4]`.
Deeply nested input thus causes a `DepthExceeded` error, and not an exhausted
heap or an exhausted call stack. `MAX_DEPTH` is 256.

`drive` returns `Ok(None)` only for a complete, well-formed document that has
only whitespace after its top-level value. Therefore there is no separate step
to complete the scan. Errors persist: after the tokenizer rejects an input, each
subsequent call reports the same error and does not continue after the incorrect
byte.

## How to ignore an unwanted value

The tokenizer has no `skip_value` method, because only the visitor knows which
values it does not want. A count of the containers costs less than a second scan
of the value. It also needs no recursion and no storage:

```rust
# use core::convert::Infallible;
# use kimojio_json::{EscapeString, Tokenizer, Visitor};
#[derive(Default)]
struct Keep {
    value: u64,
    /// How many containers of a discarded value are still open.
    depth: usize,
    /// Whether the current member is a member that this visitor keeps; `None`
    /// outside of a member.
    wanted: Option<bool>,
}

impl Keep {
    fn open(&mut self) {
        if self.depth > 0 {
            self.depth += 1;             // one more level inside a discarded value
        } else if self.wanted == Some(false) {
            self.depth = 1;              // the discarded value starts here
        } else {
            self.wanted = None;          // a container that this visitor reads
        }
    }

    fn close(&mut self) {
        self.depth = self.depth.saturating_sub(1);
        if self.depth == 0 {
            self.wanted = None;
        }
    }
}

impl<'a> Visitor<'a> for Keep {
    type Output = Infallible;

    fn object_start(&mut self) -> Option<Infallible> { self.open(); None }
    fn array_start(&mut self) -> Option<Infallible> { self.open(); None }
    fn object_end(&mut self) -> Option<Infallible> { self.close(); None }
    fn array_end(&mut self) -> Option<Infallible> { self.close(); None }

    fn key(&mut self, name: &'a str) -> Option<Infallible> {
        if self.depth == 0 {
            self.wanted = Some(name == "keep");
        }
        None
    }

    fn number(&mut self, text: &'a str) -> Option<Infallible> {
        if self.depth == 0 && self.wanted == Some(true) {
            self.value = text.parse().unwrap_or(0);
        }
        None
    }

    fn escape_key(&mut self, _: EscapeString<'a>) -> Option<Infallible> { None }
    fn escape_string(&mut self, _: EscapeString<'a>) -> Option<Infallible> { None }
}

let mut keep = Keep::default();
let mut tokenizer = Tokenizer::new(r#"{"skip":{"a":[1,2]},"keep":7}"#);
let suspended = tokenizer.drive(&mut keep)?;

assert!(suspended.is_none());
assert_eq!(keep.value, 7);
# Ok::<(), kimojio_json::Error>(())
```

## How to make structs from tokens

The tokenizer stops at the token level by design. The step from a token stream
to a typed Rust value is the step where JSON libraries usually use derive
macros, generic trait code, or a schema interpreter that runs at execution time.
Each of these costs compilation time, binary size, or speed.

This repository uses a different method. A developer writes down the JSON shape
and the necessary Rust interface. A language model makes a state machine above
this tokenizer from that description. The result is committed to the repository
and is reviewed like all other source code. There are no macros, no generics,
and no reflection. No part of the released binary depends on a model.

A `Visitor` is a state machine: one method for each token kind, with the state
in the struct. The generator thus writes the same shape that the interface
already has. State machines are difficult to write correctly by hand, which is
the reason to generate them. A generated state machine can also use the direct
path from the tokens to the final value.

The `kimojio-json-decoder` skill in `.github/skills/` writes and updates these
decoders. `kimojio/src/resolver/varlink_reply.rs` is an example. Its module
documentation contains the schema, the samples, and the invariants that a
subsequent run of the skill needs.

## Benchmarks

```sh
cargo bench -p kimojio-json
```

## Fuzzing

```sh
cd fuzz && cargo +nightly fuzz run json_tokenizer
```

The target makes sure of these properties. The tokenizer does not panic. Its
cursor stays inside the input. Each slice that it gives to the visitor borrows
from the input. An escaped body always decodes into a buffer of its own raw
length. The result is the same when the visitor suspends at each token and when
the tokenizer scans the full document in one call.

[`Tokenizer::drive`]: https://docs.rs/kimojio-json/latest/kimojio_json/struct.Tokenizer.html#method.drive
[`Visitor`]: https://docs.rs/kimojio-json/latest/kimojio_json/trait.Visitor.html
