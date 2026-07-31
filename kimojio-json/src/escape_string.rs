// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A borrowed JSON string body that contains escape sequences.

use core::fmt;

use crate::error::{Error, ErrorKind};

/// The body of a JSON string that contains **at least one escape sequence**.
/// The body borrows from the input, and the escape sequences keep the form that
/// they have in the input.
///
/// The tokenizer knows if a string contains an escape sequence. It reports that
/// fact with the choice of the method that it calls, and not with a flag. A
/// body with no escape sequence arrives at
/// [`Visitor::string`](crate::Visitor::string) or
/// [`Visitor::key`](crate::Visitor::key) as a plain `&str`. Only a body that
/// needs a decode arrives here.
///
/// This type has no `Deref<Target = str>` and no `PartialEq<str>`. The purpose
/// of the type is to make an undecoded body clearly visible to the caller.
/// Either of those two traits would silently supply text in which sequences
/// such as `\n` and `\u00e9` keep their source form.
///
/// ```
/// # use kimojio_json::{EscapeString, Tokenizer, Visitor};
/// struct First;
///
/// impl<'a> Visitor<'a> for First {
///     type Output = EscapeString<'a>;
///
///     fn escape_key(&mut self, name: EscapeString<'a>) -> Option<EscapeString<'a>> {
///         Some(name)
///     }
///
///     fn escape_string(&mut self, value: EscapeString<'a>) -> Option<EscapeString<'a>> {
///         Some(value)
///     }
/// }
///
/// let mut tokenizer = Tokenizer::new(r#""caf\u00e9""#);
/// let text = tokenizer.drive(&mut First).unwrap().expect("the string is escaped");
///
/// assert_eq!(text.as_raw_str(), r"caf\u00e9");
/// assert!(text.eq_unescaped("café"));
///
/// let mut buffer = [0_u8; 16];
/// assert_eq!(text.unescape_into(&mut buffer).unwrap(), "café");
/// ```
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct EscapeString<'a> {
    raw: &'a str,
}

impl<'a> EscapeString<'a> {
    pub(crate) const fn new(raw: &'a str) -> Self {
        Self { raw }
    }

    /// Returns the body in the form that it has in the input, with the escape
    /// sequences unchanged.
    ///
    /// This is a subslice of the document, thus it costs nothing. It is *not*
    /// the decoded text.
    #[must_use]
    #[inline]
    pub const fn as_raw_str(self) -> &'a str {
        self.raw
    }

    /// Returns the length of the undecoded body in bytes.
    ///
    /// A decode never increases the length of a body. This length is therefore
    /// always a sufficient buffer size for
    /// [`unescape_into`](Self::unescape_into).
    #[must_use]
    #[inline]
    pub const fn raw_len(self) -> usize {
        self.raw.len()
    }

    /// Compares the decoded value with `other`, and uses no buffer.
    ///
    /// This method decodes one character at a time and then discards it. A
    /// decoder that compares a known member name usually needs no more than
    /// this.
    #[must_use]
    pub fn eq_unescaped(self, other: &str) -> bool {
        let mut expected = other.chars();
        for decoded in self.unescape() {
            match (decoded, expected.next()) {
                (Ok(actual), Some(wanted)) if actual == wanted => {}
                _ => return false,
            }
        }
        expected.next().is_none()
    }

    /// Decodes the body into `buffer` and returns the result.
    ///
    /// The crate allocates no memory, thus the caller supplies the storage.
    /// [`raw_len`](Self::raw_len) is always a sufficient size.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::BufferTooSmall`] if `buffer` cannot hold the result.
    /// Returns [`ErrorKind::LoneSurrogate`] if a `\u` escape sequence names one
    /// half of a surrogate pair and there is no valid partner for it.
    pub fn unescape_into(self, buffer: &mut [u8]) -> Result<&str, Error> {
        let mut written = 0_usize;
        let mut encoded = [0_u8; 4];

        for decoded in self.unescape() {
            let character = decoded?;
            let bytes = character.encode_utf8(&mut encoded).as_bytes();
            let end = written
                .checked_add(bytes.len())
                .ok_or_else(|| Error::new(ErrorKind::BufferTooSmall, written))?;
            let target = buffer
                .get_mut(written..end)
                .ok_or_else(|| Error::new(ErrorKind::BufferTooSmall, written))?;
            target.copy_from_slice(bytes);
            written = end;
        }

        let decoded = buffer
            .get(..written)
            .ok_or_else(|| Error::new(ErrorKind::BufferTooSmall, written))?;

        // Each write above is one call to `char::encode_utf8`, complete and
        // contiguous. Therefore `buffer[..written]` is a sequence of character
        // encodings and is always valid UTF-8. Safe Rust must still do the
        // check, and the type system cannot hold the reason.
        Ok(core::str::from_utf8(decoded).expect("unescaping writes whole characters"))
    }

    /// Returns an iterator over the decoded characters.
    ///
    /// The decode occurs during the iteration, thus this method needs no
    /// storage. The iterator reports one error at the most and then stops.
    /// A loop over all its items therefore always terminates.
    #[must_use]
    pub const fn unescape(self) -> Unescape<'a> {
        Unescape {
            rest: self.raw,
            offset: 0,
        }
    }
}

impl fmt::Debug for EscapeString<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The output shows the raw body, because the raw body is the content of
        // this type.
        write!(formatter, "EscapeString({:?})", self.raw)
    }
}

/// An iterator over the decoded characters of an [`EscapeString`].
///
/// [`EscapeString::unescape`] makes this iterator. The iterator supplies one
/// error at the most. The tokenizer does not validate surrogate pairs and
/// leaves that check for the decode, thus malformed input can cause a failure
/// here. An iterator that repeated that failure without an end would stop the
/// progress of a caller that only reads all its items.
#[derive(Clone, Debug)]
pub struct Unescape<'a> {
    rest: &'a str,
    offset: usize,
}

impl Unescape<'_> {
    /// Consumes `count` bytes of the remaining body.
    fn advance(&mut self, count: usize) {
        self.rest = self.rest.get(count..).unwrap_or("");
        self.offset = self.offset.saturating_add(count);
    }

    /// Reads the four hexadecimal digits that start `at` bytes into the body.
    fn hex4(&self, at: usize) -> Result<u32, Error> {
        let digits = self
            .rest
            .as_bytes()
            .get(at..at.saturating_add(4))
            .ok_or_else(|| Error::new(ErrorKind::InvalidEscape, self.offset))?;

        let mut value = 0_u32;
        for digit in digits {
            let nibble = match digit {
                b'0'..=b'9' => u32::from(digit - b'0'),
                b'a'..=b'f' => u32::from(digit - b'a') + 10,
                b'A'..=b'F' => u32::from(digit - b'A') + 10,
                _ => return Err(Error::new(ErrorKind::InvalidEscape, self.offset)),
            };
            value = (value << 4) | nibble;
        }
        Ok(value)
    }

    /// Decodes the `\u` escape sequence at the start of the body. If a
    /// surrogate pair is present, this method joins the two halves.
    fn decode_unicode_escape(&mut self) -> Result<char, Error> {
        let leading = self.hex4(2)?;

        if (0xD800..0xDC00).contains(&leading) {
            if self.rest.as_bytes().get(6..8) != Some(b"\\u") {
                return Err(Error::new(ErrorKind::LoneSurrogate, self.offset));
            }
            let trailing = self.hex4(8)?;
            if !(0xDC00..0xE000).contains(&trailing) {
                return Err(Error::new(ErrorKind::LoneSurrogate, self.offset));
            }
            let combined = 0x1_0000 + ((leading - 0xD800) << 10) + (trailing - 0xDC00);
            let character = char::from_u32(combined)
                .ok_or_else(|| Error::new(ErrorKind::LoneSurrogate, self.offset))?;
            self.advance(12);
            return Ok(character);
        }

        if (0xDC00..0xE000).contains(&leading) {
            return Err(Error::new(ErrorKind::LoneSurrogate, self.offset));
        }

        let character = char::from_u32(leading)
            .ok_or_else(|| Error::new(ErrorKind::LoneSurrogate, self.offset))?;
        self.advance(6);
        Ok(character)
    }
}

impl Iterator for Unescape<'_> {
    type Item = Result<char, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let decoded = self.step()?;
        if decoded.is_err() {
            // Stop after one report of a failure. The tokenizer accepts any
            // four hexadecimal digits after `\u` and leaves the check of the
            // surrogate pair for this code. Malformed input can thus cause a
            // failure here. A repetition of the error without an end would make
            // `for character in text.unescape()` an infinite loop.
            self.rest = "";
        }
        Some(decoded)
    }
}

impl core::iter::FusedIterator for Unescape<'_> {}

impl Unescape<'_> {
    /// Decodes the next character, and does not stop the iterator after an
    /// error as `next` does.
    fn step(&mut self) -> Option<Result<char, Error>> {
        let character = self.rest.chars().next()?;

        if character != '\\' {
            self.advance(character.len_utf8());
            return Some(Ok(character));
        }

        let Some(escape) = self.rest.as_bytes().get(1).copied() else {
            return Some(Err(Error::new(ErrorKind::InvalidEscape, self.offset)));
        };

        let simple = match escape {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{0008}',
            b'f' => '\u{000C}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => return Some(self.decode_unicode_escape()),
            _ => return Some(Err(Error::new(ErrorKind::InvalidEscape, self.offset))),
        };

        self.advance(2);
        Some(Ok(simple))
    }
}
