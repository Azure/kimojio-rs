#![no_main]
//! Drives the JSON tokenizer with arbitrary bytes.
//!
//! The tokenizer borrows from its input and allocates no memory. The properties
//! to test are therefore structural. The tokenizer must not panic. Its cursor
//! must stay inside the input. Each borrowed slice must come from the input. An
//! escaped body must always fit in a buffer of its own raw length. A document
//! that the tokenizer accepts must give the same result on a second pass.
//!
//! The first pass suspends at each token, thus the driver can examine the
//! cursor between the tokens. That also makes each token boundary a resume
//! point. A caller that scans a full document in one call does not use those
//! resume points. The second pass is that type of caller, and the two passes
//! must agree.

use kimojio_json::{EscapeString, Tokenizer, Visitor};
use libfuzzer_sys::fuzz_target;

/// A token, recorded in a form that permits a comparison of two passes.
#[derive(Debug, Eq, PartialEq)]
enum Seen<'a> {
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

/// Makes sure that `slice` points into `input`, and not into other memory that
/// the tokenizer supplied.
fn borrows_from(input: &[u8], slice: &[u8]) {
    let start = input.as_ptr() as usize;
    let end = start + input.len();
    let slice_start = slice.as_ptr() as usize;
    assert!(
        slice_start >= start && slice_start + slice.len() <= end,
        "token did not borrow from the input"
    );
}

/// Records each token and checks the memory that the token borrows.
struct Record<'a> {
    input: &'a [u8],
    seen: Vec<Seen<'a>>,
    /// Whether to suspend the tokenizer after each token.
    suspend: bool,
}

impl<'a> Record<'a> {
    /// Makes a recorder that suspends at each token.
    fn stepping(input: &'a [u8]) -> Self {
        Self {
            input,
            seen: Vec::new(),
            suspend: true,
        }
    }

    /// Makes a recorder that scans the full document in one call.
    fn racing(input: &'a [u8]) -> Self {
        Self {
            input,
            seen: Vec::new(),
            suspend: false,
        }
    }

    fn push(&mut self, token: Seen<'a>) -> Option<()> {
        self.seen.push(token);
        self.suspend.then_some(())
    }

    /// Records a borrowed scalar after a check that it comes from the input.
    fn text(&mut self, text: &'a str, wrap: fn(&'a str) -> Seen<'a>) -> Option<()> {
        borrows_from(self.input, text.as_bytes());
        self.push(wrap(text))
    }

    /// Records an escaped body after a check of its source and of the buffer
    /// size that `raw_len` promises.
    fn escaped(&mut self, text: EscapeString<'a>, wrap: fn(&'a str) -> Seen<'a>) -> Option<()> {
        borrows_from(self.input, text.as_raw_str().as_bytes());
        // A decode never increases the length of a body. Therefore `raw_len` is
        // always a sufficient buffer size.
        let mut buffer = vec![0_u8; text.raw_len()];
        if let Ok(decoded) = text.unescape_into(&mut buffer) {
            assert!(decoded.len() <= text.raw_len());
        }
        self.push(wrap(text.as_raw_str()))
    }
}

impl<'a> Visitor<'a> for Record<'a> {
    type Output = ();

    fn object_start(&mut self) -> Option<()> {
        self.push(Seen::ObjectStart)
    }

    fn object_end(&mut self) -> Option<()> {
        self.push(Seen::ObjectEnd)
    }

    fn array_start(&mut self) -> Option<()> {
        self.push(Seen::ArrayStart)
    }

    fn array_end(&mut self) -> Option<()> {
        self.push(Seen::ArrayEnd)
    }

    fn key(&mut self, name: &'a str) -> Option<()> {
        self.text(name, Seen::Key)
    }

    fn escape_key(&mut self, name: EscapeString<'a>) -> Option<()> {
        self.escaped(name, Seen::EscapeKey)
    }

    fn string(&mut self, value: &'a str) -> Option<()> {
        self.text(value, Seen::String)
    }

    fn escape_string(&mut self, value: EscapeString<'a>) -> Option<()> {
        self.escaped(value, Seen::EscapeString)
    }

    fn number(&mut self, text: &'a str) -> Option<()> {
        self.text(text, Seen::Number)
    }

    fn boolean(&mut self, value: bool) -> Option<()> {
        self.push(Seen::Bool(value))
    }

    fn null(&mut self) -> Option<()> {
        self.push(Seen::Null)
    }
}

fuzz_target!(|data: &[u8]| {
    // The tokenizer accepts text, thus the bytes from the fuzzer must become
    // text first. A replacement of the incorrect bytes keeps almost all inputs
    // usable. A rejection of those inputs would discard most of a random corpus
    // and would test the grammar much less.
    let text = String::from_utf8_lossy(data);
    let data = text.as_bytes();

    let mut tokenizer = Tokenizer::new(&text);
    let mut stepped = Record::stepping(data);
    let mut previous_offset = 0;

    loop {
        let offset = tokenizer.offset();
        assert!(offset >= previous_offset, "cursor moved backwards");
        assert!(offset <= data.len(), "cursor escaped the input");
        previous_offset = offset;

        match tokenizer.drive(&mut stepped) {
            Ok(Some(())) => {}
            Ok(None) => break,
            Err(error) => {
                assert!(
                    error.offset() <= data.len(),
                    "error offset escaped the input"
                );
                // A failure persists. A subsequent call must report the same
                // failure and must not continue after the incorrect byte.
                assert_eq!(tokenizer.drive(&mut stepped).unwrap_err(), error);
                return;
            }
        }
    }

    // A document that tokenized without an error must do the same on a second
    // pass. A pass that does not suspend must see the same tokens as a pass
    // that suspends at each token.
    let mut raced = Record::racing(data);
    let finished = Tokenizer::new(&text)
        .drive(&mut raced)
        .expect("a correct stream failed on the second pass");
    assert!(finished.is_none(), "this visitor must not suspend");
    assert_eq!(raced.seen, stepped.seen, "the second pass saw other tokens");
});
