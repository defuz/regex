// Copyright 2014-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ops;
use std::str;

use char::Char;
use prefix::Prefix;

pub trait InputReader {
    type Reader: Input;
}

pub struct StringInputReader;
pub struct BytesInputReader;

impl<'t> InputReader for StringInputReader {
    type Reader = CharInput<'t>;
}

impl<'t> InputReader for BytesInputReader {
    type Reader = ByteInput<'t>;
}

/// An abstraction over input used in the matching engines.
pub trait Input {
    /// A representation of a position in the input.
    type At: InputAt;

    /// Return an encoding of the position at byte offset `i`.
    fn at(&self, i: usize) -> Self::At;

    /// Return the Unicode character occurring next to at.
    ///
    /// If no such character could be decoded, then Char should be absent.
    fn next_char(&self, at: Self::At) -> Char;

    /// Return the Unicode character occurring previous to at.
    ///
    /// If no such character could be decoded, then Char should be absent.
    fn previous_char(&self, at: Self::At) -> Char;

    /// Scan the input for a matching prefix.
    fn prefix_at(&self, prefixes: &Prefix, at: Self::At) -> Option<Self::At>;
}

pub trait InputAt {
    /// Returns true iff this position is at the beginning of the input.
    fn is_beginning(&self) -> bool;

    /// Returns the character at this position.
    ///
    /// If this position is just before or after the input, then an absent
    /// character is returned.
    ///
    /// If the underlying input reader is not character based, then this
    /// method may panic because it is illegal to mix byte based matching
    /// and Unicode based matching.
    fn char(&self) -> Char;

    /// Return the byte at this position.
    ///
    /// If this position is after the end of the input, then `None` is
    /// returned.
    ///
    /// If the underlying input reader is not byte based, then this method may
    /// panic because it is illegal to mix byte based matching and Unicode
    /// based matching.
    fn byte(&self) -> Option<u8>;

    /// Returns the width of the character at this position.
    ///
    /// If the underlying input reader is byte based, then this method should
    /// always return `1`. Otherwise, it should return the number of bytes
    /// required to encode the current character in UTF-8.
    fn len(&self) -> usize;

    /// Returns the byte offset of this position.
    fn pos(&self) -> usize;

    /// Returns the byte offset of the next position in the input.
    fn next_pos(&self) -> usize;
}

/// An input reader over Unicode scalar values.
///
/// This reader advances by codepoint.
#[derive(Debug)]
pub struct CharInput<'t>(&'t str);

impl<'t> CharInput<'t> {
    /// Return a new character input reader for the given string.
    pub fn new(s: &'t str) -> CharInput<'t> {
        CharInput(s)
    }
}

impl<'t> ops::Deref for CharInput<'t> {
    type Target = str;

    fn deref(&self) -> &str {
        self.0
    }
}

impl<'t> Input for CharInput<'t> {
    type At = CharInputAt;

    // This `inline(always)` increases throughput by almost 25% on the `hard`
    // benchmarks over a normal `inline` annotation.
    //
    // I'm not sure why `#[inline]` isn't enough to convince LLVM, but it is
    // used *a lot* in the guts of the matching engines.
    #[inline(always)]
    fn at(&self, i: usize) -> Self::At {
        let c = self[i..].chars().next().into();
        CharInputAt {
            pos: i,
            c: c,
            len: c.len_utf8(),
        }
    }

    fn next_char(&self, at: Self::At) -> Char {
        at.char()
    }

    fn previous_char(&self, at: Self::At) -> Self::At {
        let c: Char = self[..at.pos()].chars().rev().next().into();
        let len = c.len_utf8();
        CharInputAt {
            pos: at.pos() - len,
            c: c,
            len: len,
        }
    }

    fn prefix_at(&self, prefixes: &Prefix, at: Self::At) -> Option<Self::At> {
        prefixes.find(&self[at.pos()..]).map(|(s, _)| self.at(at.pos() + s))
    }
}

/// Represents a location in the input.
#[derive(Clone, Copy, Debug)]
pub struct CharInputAt {
    pos: usize,
    c: Char,
    len: usize,
}

impl InputAt for CharInputAt {
    fn is_beginning(&self) -> bool {
        self.pos == 0
    }

    fn char(&self) -> Char {
        self.c
    }

    fn byte(&self) -> Option<u8> {
        unreachable!("Unicode program cannot use byte matching functions")
    }

    fn len(&self) -> usize {
        self.len
    }

    fn pos(&self) -> usize {
        self.pos
    }

    fn next_pos(&self) -> usize {
        self.pos + self.len
    }
}

#[derive(Debug)]
pub struct ByteInput<'t>(&'t [u8]);

impl<'t> ByteInput<'t> {
    pub fn new(s: &'t str) -> ByteInput<'t> {
        ByteInput(s.as_bytes())
    }
}

impl<'t> ops::Deref for ByteInput<'t> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.0
    }
}

impl<'t> Input for ByteInput<'t> {
    type At = ByteInputAt;

    fn at(&self, i: usize) -> Self::At {
        ByteInputAt { pos: i, byte: self.get(i) }
    }

    fn next_char(&self, at: Self::At) -> Char {
        let s = unsafe { str::from_utf8_unchecked(&self[at.pos()..]) };
        s.chars().next().into()
    }

    fn previous_char(&self, at: Self::At) -> Char {
        let s = unsafe { str::from_utf8_unchecked(&self[..at.pos()]) };
        s.chars().rev().next().into()
    }

    fn prefix_at(&self, prefixes: &Prefix, at: Self::At) -> Option<Self::At> {
        unimplemented!()
    }
}

pub struct ByteInputAt {
    pos: usize,
    byte: Option<u8>,
}

impl InputAt for ByteInputAt {
    fn is_beginning(&self) -> bool {
        self.pos == 0
    }

    fn char(&self) -> Char {
        unreachable!("byte program cannot use Unicode matching functions")
    }

    fn byte(&self) -> Option<u8> {
        self.byte
    }

    fn len(&self) -> usize {
        1
    }

    fn pos(&self) -> usize {
        self.pos
    }

    fn next_pos(&self) -> usize {
        self.pos + 1
    }
}
