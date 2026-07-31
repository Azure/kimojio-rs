// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Errors reported by the tokenizer.

use core::fmt;

/// The reason a JSON input was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The input ended in the middle of a token or container.
    UnexpectedEof,
    /// A byte appeared where the JSON grammar does not allow it.
    UnexpectedByte,
    /// Non-whitespace bytes followed the top-level value.
    TrailingData,
    /// A number did not match the JSON number grammar.
    InvalidNumber,
    /// A string had no closing quote.
    UnterminatedString,
    /// A raw control byte below `0x20` appeared inside a string.
    ControlCharacter,
    /// A backslash was not followed by a valid escape sequence.
    InvalidEscape,
    /// An object member name was not a string.
    ExpectedKey,
    /// An object member name was not followed by `:`.
    ExpectedColon,
    /// Container nesting exceeded [`MAX_DEPTH`](crate::MAX_DEPTH).
    DepthExceeded,
    /// A `\u` escape sequence named one half of a surrogate pair, and there is
    /// no valid partner for it.
    ///
    /// Only an expansion of the escape sequences causes this error. The
    /// tokenizer accepts any four hexadecimal digits.
    LoneSurrogate,
    /// The caller's buffer was too small to hold the unescaped string.
    BufferTooSmall,
}

impl ErrorKind {
    /// Returns a short, stable description of this error kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnexpectedEof => "input ended unexpectedly",
            Self::UnexpectedByte => "unexpected byte",
            Self::TrailingData => "trailing data after the top-level value",
            Self::InvalidNumber => "invalid number",
            Self::UnterminatedString => "unterminated string",
            Self::ControlCharacter => "unescaped control character in a string",
            Self::InvalidEscape => "invalid escape sequence",
            Self::ExpectedKey => "expected an object member name",
            Self::ExpectedColon => "expected `:` after an object member name",
            Self::DepthExceeded => "maximum nesting depth exceeded",
            Self::LoneSurrogate => "lone surrogate in a `\\u` escape",
            Self::BufferTooSmall => "buffer too small",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An error from the tokenizer, with the byte offset of the rejected input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    kind: ErrorKind,
    offset: usize,
}

impl Error {
    /// Creates an error of `kind` positioned at `offset`.
    #[must_use]
    pub const fn new(kind: ErrorKind, offset: usize) -> Self {
        Self { kind, offset }
    }

    /// Returns the reason the input was rejected.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Returns the byte offset at which the input was rejected.
    ///
    /// An offset from an expansion of escape sequences is relative to the
    /// string body and not to the full document.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at byte {}", self.kind, self.offset)
    }
}

impl core::error::Error for Error {}
