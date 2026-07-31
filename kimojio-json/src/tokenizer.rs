// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The single-pass JSON scanner that allocates no memory.

use crate::{
    error::{Error, ErrorKind},
    escape_string::EscapeString,
};

/// The number of `u64` words that hold the container stack.
const STACK_WORDS: usize = 4;

/// The maximum container nesting depth that the tokenizer accepts.
///
/// The tokenizer records the nesting in a bitmap of fixed size, and not on the
/// heap or on the call stack. Deeply nested input therefore causes an
/// [`ErrorKind::DepthExceeded`] error, and not an allocation or a stack
/// overflow.
pub const MAX_DEPTH: usize = STACK_WORDS * 64;

/// Receives one call for each JSON token during the scan.
///
/// Each token kind has its own method. Therefore the tokenizer does not build a
/// token enumeration, return it, match on it, and then discard it. The scanner
/// calls the visitor directly and continues. This is the purpose of the design:
/// for a small document, the transfer of each token to the caller costs more
/// than the scan of the bytes.
///
/// String bodies and numbers borrow from the document. The tokenizer validates
/// the escape sequences but does not expand them, and it does not convert the
/// numbers.
///
/// The tokenizer reports strings with two methods, and not with one method and
/// an "is escaped" flag. The scanner knows which of the two applies. One more
/// method costs nothing, and the usual path for a string with no escape
/// sequence then needs no decode logic.
///
/// The methods for the usual token kinds have a default body that returns
/// `None`. A visitor thus implements only the methods that it needs.
///
/// Do not mistakenly skip processing the two methods for escaped strings.
/// If your strings may be escaped, you must handle those cases. For this
/// reason they have no default body and must be written.
///
/// # Suspension
///
/// Each method returns `Option<Self::Output>`. This is how a visitor gets
/// control from the scanner:
///
/// - `None` continues the scan. [`Tokenizer::drive`] does not return and
///   continues with the next token.
/// - `Some(value)` stops the scan. `drive` returns `Ok(Some(value))`. The
///   scanner commits the state transition for this token *before* it calls the
///   visitor. The next call to `drive` therefore continues at the position
///   where the scan stopped. There is no resume point to record and nothing to
///   scan again.
///
/// # The two types of visitor
///
/// A visitor that fills a full struct in one pass never suspends. Give it
/// `type Output = Infallible` and return `None` from each method.
/// `Option<Infallible>` is a zero-sized type with one possible value. The
/// optimizer thus proves that each suspension test is false and removes it. The
/// scanner then reads the full document without a return to the caller.
///
/// ```
/// use core::convert::Infallible;
/// use kimojio_json::{EscapeString, Tokenizer, Visitor};
///
/// #[derive(Default)]
/// struct CountKeys(usize);
///
/// impl<'a> Visitor<'a> for CountKeys {
///     type Output = Infallible;
///
///     fn key(&mut self, _: &'a str) -> Option<Infallible> {
///         self.0 += 1;
///         None
///     }
///
///     // An escaped member name is also a member name.
///     fn escape_key(&mut self, _: EscapeString<'a>) -> Option<Infallible> {
///         self.0 += 1;
///         None
///     }
///
///     // This visitor does not read string values.
///     fn escape_string(&mut self, _: EscapeString<'a>) -> Option<Infallible> {
///         None
///     }
/// }
///
/// let document = r#"{"name":"kimojio","tags":["fast","small"],"size":42}"#;
/// let mut counter = CountKeys::default();
/// Tokenizer::new(document).drive(&mut counter).unwrap();
/// assert_eq!(counter.0, 3);
/// ```
///
/// A visitor that supplies an iterator suspends when it has an item ready, and
/// the caller calls `drive` in a loop. `kimojio::resolver::varlink_reply` is an
/// example of that type.
pub trait Visitor<'a> {
    /// The value that a visitor gives to its caller when it suspends the scan.
    ///
    /// Use [`core::convert::Infallible`] for a visitor that never suspends.
    type Output;

    /// Reports an object member name that contains at least one escape
    /// sequence.
    ///
    /// This is rare but legal: `{"a\nb": 1}` is valid JSON. The tokenizer
    /// consumes the `:` that follows. This method has no default body; see the
    /// note on the trait.
    fn escape_key(&mut self, name: EscapeString<'a>) -> Option<Self::Output>;

    /// Reports a string value that contains at least one escape sequence.
    ///
    /// This method has no default body; see the note on the trait.
    fn escape_string(&mut self, value: EscapeString<'a>) -> Option<Self::Output>;

    /// Reports `{`.
    fn object_start(&mut self) -> Option<Self::Output> {
        None
    }

    /// Reports `}`.
    fn object_end(&mut self) -> Option<Self::Output> {
        None
    }

    /// Reports `[`.
    fn array_start(&mut self) -> Option<Self::Output> {
        None
    }

    /// Reports `]`.
    fn array_end(&mut self) -> Option<Self::Output> {
        None
    }

    /// Reports an object member name that contains no escape sequence.
    ///
    /// The tokenizer consumes the `:` that follows.
    fn key(&mut self, name: &'a str) -> Option<Self::Output> {
        let _ = name;
        None
    }

    /// Reports a string value that contains no escape sequence.
    fn string(&mut self, value: &'a str) -> Option<Self::Output> {
        let _ = value;
        None
    }

    /// Reports a number as the raw text from the input.
    ///
    /// The text always agrees with the JSON number grammar. The conversion into
    /// a numeric type is the task of the visitor. Only the visitor knows which
    /// type it needs and what to do with a value that is too large for that
    /// type.
    fn number(&mut self, text: &'a str) -> Option<Self::Output> {
        let _ = text;
        None
    }

    /// Reports `true` or `false`.
    fn boolean(&mut self, value: bool) -> Option<Self::Output> {
        let _ = value;
        None
    }

    /// Reports `null`.
    fn null(&mut self) -> Option<Self::Output> {
        None
    }
}

/// The position of the scanner in the document grammar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    /// Before the top-level value.
    Start,
    /// The top-level value is complete. Only whitespace can follow.
    Done,
    /// After `{`. A member name or `}` can follow.
    ObjectFirstKey,
    /// After a `,` inside an object. A member name must follow.
    ObjectNextKey,
    /// After `name:`. A value must follow.
    ObjectValue,
    /// After a member value. A `,` or a `}` must follow.
    ObjectComma,
    /// After `[`. A value or `]` can follow.
    ArrayFirstValue,
    /// After a `,` inside an array. A value must follow.
    ArrayNextValue,
    /// After an element. A `,` or a `]` must follow.
    ArrayComma,
}

/// A forward-only JSON scanner. All of its output borrows from the input.
///
/// The tokenizer allocates no memory, copies no data, expands no escape
/// sequences, and converts no numbers. It does validate the full JSON grammar.
/// A call to [`drive`](Tokenizer::drive) that reaches the end of the document
/// without an error therefore read a well-formed document.
///
/// ```
/// # use core::convert::Infallible;
/// # use kimojio_json::{EscapeString, Tokenizer, Visitor};
/// #[derive(Default)]
/// struct Sum(u64);
///
/// impl<'a> Visitor<'a> for Sum {
///     type Output = Infallible;
///
///     fn number(&mut self, text: &'a str) -> Option<Infallible> {
///         self.0 += text.parse::<u64>().unwrap_or(0);
///         None
///     }
///
///     fn escape_key(&mut self, _: EscapeString<'a>) -> Option<Infallible> { None }
///     fn escape_string(&mut self, _: EscapeString<'a>) -> Option<Infallible> { None }
/// }
///
/// let mut sum = Sum::default();
/// Tokenizer::new(r#"{"a":1,"b":[2,3]}"#).drive(&mut sum).unwrap();
/// assert_eq!(sum.0, 6);
/// ```
#[derive(Clone, Debug)]
pub struct Tokenizer<'a> {
    /// The document, held as text and not as bytes.
    ///
    /// Safe Rust cannot make a `&str` from a `&[u8]` without a validation. A
    /// tokenizer that holds bytes must therefore validate each string and each
    /// number that it gives to the caller. That is a second pass over bytes
    /// that the scanner read already, behind a call that the optimizer does not
    /// inline. With text, each slice needs only two character-boundary checks.
    /// A caller that holds bytes does the one necessary validation itself, and
    /// can move it, share it, or omit it if it has text already.
    input: &'a str,
    offset: usize,
    state: State,
    /// One bit per open container: `1` for an array, `0` for an object.
    stack: [u64; STACK_WORDS],
    depth: usize,
    failure: Option<Error>,
}

impl<'a> Tokenizer<'a> {
    /// Makes a tokenizer for `input`.
    ///
    /// This method accepts text and not bytes, because JSON is UTF-8 by
    /// definition. Text is also the reason why a borrowed `&str` for each token
    /// costs nothing. A caller that holds bytes converts them one time with
    /// [`core::str::from_utf8`]. That conversion is one vectorized pass, and
    /// the caller can put it where it is best. A caller that has a `&str`
    /// already does not pay for it.
    #[must_use]
    #[inline]
    pub const fn new(input: &'a str) -> Self {
        Self {
            input,
            offset: 0,
            state: State::Start,
            stack: [0; STACK_WORDS],
            depth: 0,
            failure: None,
        }
    }

    /// Returns the byte offset of the scanner cursor.
    ///
    /// After [`drive`](Tokenizer::drive) suspends, this offset points to the
    /// byte after the token at which the visitor stopped. An error message
    /// about that token quotes this offset.
    #[must_use]
    #[inline]
    pub const fn offset(&self) -> usize {
        self.offset
    }

    /// Returns the number of containers that are open.
    #[must_use]
    #[inline]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// Runs the scanner and reports each token to `visitor`.
    ///
    /// Returns:
    ///
    /// - `Ok(None)` after the document is complete and fully consumed. There is
    ///   no separate step to complete the scan. This result shows that the
    ///   top-level value is well formed and that only whitespace follows it. A
    ///   subsequent call returns `Ok(None)` again.
    /// - `Ok(Some(value))` if the visitor suspended the scan with a result of
    ///   `Some(value)`. Call `drive` again to continue at the byte after that
    ///   token.
    /// - `Err(error)` for the first violation of the JSON grammar. A failure
    ///   persists: after the tokenizer reports one failure, each subsequent
    ///   call reports the same failure. The tokenizer does not scan again from
    ///   a position that is known to be incorrect.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] that describes the first violation of the JSON
    /// grammar and the byte offset of that violation. This includes
    /// [`ErrorKind::TrailingData`] for bytes after the top-level value and
    /// [`ErrorKind::UnexpectedEof`] for a document that stops too early.
    #[inline]
    pub fn drive<V>(&mut self, visitor: &mut V) -> Result<Option<V::Output>, Error>
    where
        V: Visitor<'a>,
    {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        match self.run(visitor) {
            Ok(output) => Ok(output),
            Err(failure) => {
                self.failure = Some(failure);
                Err(failure)
            }
        }
    }

    /// Skips insignificant whitespace and returns the byte at the cursor.
    ///
    /// The method returns the byte and does not only move the cursor. The
    /// caller thus does not do a second bounds-checked load of a byte that is
    /// already in a register. This code runs one time for each token.
    #[inline]
    fn skip_whitespace(&mut self) -> Option<u8> {
        while let Some(&byte) = self.input.as_bytes().get(self.offset) {
            // Each byte that JSON accepts as whitespace is less than or equal
            // to `b' '`, and almost each byte in a document is greater than
            // that. One comparison thus completes the usual case before the
            // specific tests run.
            if byte > b' ' {
                return Some(byte);
            }
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' => self.offset = self.offset.saturating_add(1),
                _ => return Some(byte),
            }
        }
        None
    }

    /// Makes an error of `kind` at the current cursor.
    #[inline]
    fn error(&self, kind: ErrorKind) -> Error {
        Error::new(kind, self.offset)
    }

    /// Opens a container and records if the container is an array.
    fn push(&mut self, is_array: bool) -> Result<(), Error> {
        if self.depth >= MAX_DEPTH {
            return Err(self.error(ErrorKind::DepthExceeded));
        }
        let word = self.depth / 64;
        let bit = 1_u64 << (self.depth % 64);
        if let Some(slot) = self.stack.get_mut(word) {
            if is_array {
                *slot |= bit;
            } else {
                *slot &= !bit;
            }
        }
        self.depth = self.depth.saturating_add(1);
        Ok(())
    }

    /// Returns `true` if the innermost open container is an array.
    fn in_array(&self) -> bool {
        match self.depth.checked_sub(1) {
            Some(level) => self
                .stack
                .get(level / 64)
                .is_some_and(|word| (word >> (level % 64)) & 1 == 1),
            None => false,
        }
    }

    /// Moves to the state that follows a completed value.
    #[inline]
    fn after_value(&mut self) {
        self.state = if self.depth == 0 {
            State::Done
        } else if self.in_array() {
            State::ArrayComma
        } else {
            State::ObjectComma
        };
    }

    /// Closes the innermost container and moves the cursor after it.
    #[inline]
    fn close_container(&mut self) {
        self.offset = self.offset.saturating_add(1);
        self.depth = self.depth.saturating_sub(1);
        self.after_value();
    }

    /// Scans until the visitor suspends the scan, the document ends, or the
    /// input violates the grammar.
    ///
    /// Each arm commits the cursor and the grammar state for its token *before*
    /// it gives that token to the visitor. This order makes a suspension free
    /// of cost: there is no incomplete transition to record, thus the tokenizer
    /// only enters this loop again to continue.
    fn run<V>(&mut self, visitor: &mut V) -> Result<Option<V::Output>, Error>
    where
        V: Visitor<'a>,
    {
        loop {
            let Some(byte) = self.skip_whitespace() else {
                return match self.state {
                    State::Done => Ok(None),
                    _ => Err(self.error(ErrorKind::UnexpectedEof)),
                };
            };

            let suspended = match self.state {
                State::Done => return Err(self.error(ErrorKind::TrailingData)),

                State::Start | State::ObjectValue | State::ArrayNextValue => {
                    self.scan_value(byte, visitor)?
                }

                State::ArrayFirstValue => {
                    if byte == b']' {
                        self.close_container();
                        visitor.array_end()
                    } else {
                        self.scan_value(byte, visitor)?
                    }
                }

                State::ObjectFirstKey => {
                    if byte == b'}' {
                        self.close_container();
                        visitor.object_end()
                    } else {
                        self.scan_key(byte, visitor)?
                    }
                }

                State::ObjectNextKey => self.scan_key(byte, visitor)?,

                State::ObjectComma => match byte {
                    b',' => {
                        self.offset = self.offset.saturating_add(1);
                        self.state = State::ObjectNextKey;
                        None
                    }
                    b'}' => {
                        self.close_container();
                        visitor.object_end()
                    }
                    _ => return Err(self.error(ErrorKind::UnexpectedByte)),
                },

                State::ArrayComma => match byte {
                    b',' => {
                        self.offset = self.offset.saturating_add(1);
                        self.state = State::ArrayNextValue;
                        None
                    }
                    b']' => {
                        self.close_container();
                        visitor.array_end()
                    }
                    _ => return Err(self.error(ErrorKind::UnexpectedByte)),
                },
            };

            // A visitor that never suspends makes `V::Output` an uninhabited
            // type. The compiler thus removes this test, and the loop becomes a
            // continuous scan.
            if suspended.is_some() {
                return Ok(suspended);
            }
        }
    }

    /// Scans an object member name and the `:` that must follow it.
    fn scan_key<V>(&mut self, byte: u8, visitor: &mut V) -> Result<Option<V::Output>, Error>
    where
        V: Visitor<'a>,
    {
        if byte != b'"' {
            return Err(self.error(ErrorKind::ExpectedKey));
        }
        let (name, escaped) = self.scan_string()?;

        if self.skip_whitespace() != Some(b':') {
            return Err(self.error(ErrorKind::ExpectedColon));
        }
        self.offset = self.offset.saturating_add(1);
        self.state = State::ObjectValue;

        Ok(if escaped {
            visitor.escape_key(EscapeString::new(name))
        } else {
            visitor.key(name)
        })
    }

    /// Scans a value that starts with `byte`.
    fn scan_value<V>(&mut self, byte: u8, visitor: &mut V) -> Result<Option<V::Output>, Error>
    where
        V: Visitor<'a>,
    {
        match byte {
            b'{' => {
                self.push(false)?;
                self.offset = self.offset.saturating_add(1);
                self.state = State::ObjectFirstKey;
                Ok(visitor.object_start())
            }
            b'[' => {
                self.push(true)?;
                self.offset = self.offset.saturating_add(1);
                self.state = State::ArrayFirstValue;
                Ok(visitor.array_start())
            }
            b'"' => {
                let (text, escaped) = self.scan_string()?;
                self.after_value();
                Ok(if escaped {
                    visitor.escape_string(EscapeString::new(text))
                } else {
                    visitor.string(text)
                })
            }
            b't' => {
                self.scan_literal(b"true")?;
                self.after_value();
                Ok(visitor.boolean(true))
            }
            b'f' => {
                self.scan_literal(b"false")?;
                self.after_value();
                Ok(visitor.boolean(false))
            }
            b'n' => {
                self.scan_literal(b"null")?;
                self.after_value();
                Ok(visitor.null())
            }
            b'-' | b'0'..=b'9' => {
                let number = self.scan_number()?;
                self.after_value();
                Ok(visitor.number(number))
            }
            _ => Err(self.error(ErrorKind::UnexpectedByte)),
        }
    }

    /// Consumes the keyword `word`.
    fn scan_literal(&mut self, word: &[u8]) -> Result<(), Error> {
        let end = self.offset.saturating_add(word.len());
        match self.input.as_bytes().get(self.offset..end) {
            Some(found) if found == word => {
                self.offset = end;
                Ok(())
            }
            Some(_) => Err(self.error(ErrorKind::UnexpectedByte)),
            None => Err(self.error(ErrorKind::UnexpectedEof)),
        }
    }

    /// Scans a string body and leaves the cursor after the closing quote.
    ///
    /// The cursor must be at the opening quote. The method validates each
    /// escape sequence but does not expand it, thus the returned slice points
    /// into the input. The flag reports if the body contains an escape
    /// sequence. The caller uses the flag to select between the plain visitor
    /// method and the escaped visitor method.
    fn scan_string(&mut self) -> Result<(&'a str, bool), Error> {
        let bytes = self.input.as_bytes();
        let start = self.offset.saturating_add(1);
        let mut cursor = start;
        let mut escaped = false;

        let end = loop {
            // Most bytes are ordinary bytes. Scan them without a change to any
            // other state.
            while cursor < bytes.len() {
                let byte = bytes[cursor];
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                cursor = cursor.saturating_add(1);
            }

            match bytes.get(cursor) {
                None => return Err(Error::new(ErrorKind::UnterminatedString, cursor)),
                Some(b'"') => break cursor,
                Some(b'\\') => {
                    escaped = true;
                    cursor = validate_escape(bytes, cursor)?;
                }
                Some(_) => return Err(Error::new(ErrorKind::ControlCharacter, cursor)),
            }
        };

        // Both ends are at a quote. This call thus fails only if the scan above
        // is incorrect. It is a bounds check and not a second pass over the
        // body.
        let body = self
            .input
            .get(start..end)
            .ok_or_else(|| Error::new(ErrorKind::UnterminatedString, end))?;

        self.offset = end.saturating_add(1);
        Ok((body, escaped))
    }

    /// Scans a number and leaves the cursor after its last byte.
    fn scan_number(&mut self) -> Result<&'a str, Error> {
        let bytes = self.input.as_bytes();
        let start = self.offset;
        let mut cursor = start;

        if bytes.get(cursor) == Some(&b'-') {
            cursor = cursor.saturating_add(1);
        }

        match bytes.get(cursor) {
            Some(b'0') => {
                cursor = cursor.saturating_add(1);
                // More digits must not follow a leading zero.
                if bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
                    return Err(Error::new(ErrorKind::InvalidNumber, cursor));
                }
            }
            Some(digit) if digit.is_ascii_digit() => {
                cursor = skip_digits(bytes, cursor);
            }
            Some(_) => return Err(Error::new(ErrorKind::InvalidNumber, cursor)),
            None => return Err(Error::new(ErrorKind::UnexpectedEof, cursor)),
        }

        if bytes.get(cursor) == Some(&b'.') {
            cursor = cursor.saturating_add(1);
            let after = skip_digits(bytes, cursor);
            if after == cursor {
                return Err(Error::new(ErrorKind::InvalidNumber, cursor));
            }
            cursor = after;
        }

        if matches!(bytes.get(cursor), Some(b'e' | b'E')) {
            cursor = cursor.saturating_add(1);
            if matches!(bytes.get(cursor), Some(b'+' | b'-')) {
                cursor = cursor.saturating_add(1);
            }
            let after = skip_digits(bytes, cursor);
            if after == cursor {
                return Err(Error::new(ErrorKind::InvalidNumber, cursor));
            }
            cursor = after;
        }

        // The scan accepts ASCII only. Both ends are therefore character
        // boundaries and this call cannot fail. It costs two boundary checks
        // and not a pass over the digits.
        let text = self
            .input
            .get(start..cursor)
            .ok_or_else(|| Error::new(ErrorKind::InvalidNumber, start))?;

        self.offset = cursor;
        Ok(text)
    }
}

/// Moves the cursor after a sequence of ASCII digits.
#[inline]
fn skip_digits(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor = cursor.saturating_add(1);
    }
    cursor
}

/// Validates the escape sequence at `cursor` and returns the index of the byte
/// after the sequence.
fn validate_escape(bytes: &[u8], cursor: usize) -> Result<usize, Error> {
    let next = cursor.saturating_add(1);
    let escape = *bytes
        .get(next)
        .ok_or_else(|| Error::new(ErrorKind::UnterminatedString, next))?;

    match escape {
        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => Ok(cursor.saturating_add(2)),
        b'u' => {
            let end = cursor.saturating_add(6);
            let digits = bytes
                .get(cursor.saturating_add(2)..end)
                .ok_or_else(|| Error::new(ErrorKind::InvalidEscape, cursor))?;
            if digits.iter().all(u8::is_ascii_hexdigit) {
                Ok(end)
            } else {
                Err(Error::new(ErrorKind::InvalidEscape, cursor))
            }
        }
        _ => Err(Error::new(ErrorKind::InvalidEscape, cursor)),
    }
}
