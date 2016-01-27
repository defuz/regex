use std::cmp::Ordering;
use std::ops::Deref;

use char::Char;
use literals::{BuildPrefixes, Literals};

/// InstIdx represents the index of an instruction in a regex program.
pub type InstIdx = usize;

/// Insts is a sequence of instructions.
#[derive(Clone, Debug)]
pub struct Insts {
    insts: Vec<Inst>,
    bytes: bool,
}

impl Insts {
    /// Create a new instruction sequence.
    ///
    /// If `bytes` is true, then this instruction sequence must run on raw
    /// bytes. Otherwise, it is executed on Unicode codepoints.
    ///
    /// A Vec<Inst> can be created with the compiler.
    pub fn new(insts: Vec<Inst>, bytes: bool) -> Self {
        Insts { insts: insts, bytes: bytes }
    }

    /// Returns true if and only if this instruction sequence must be executed
    /// on byte strings.
    pub fn is_bytes(&self) -> bool {
        self.bytes
    }

    /// If pc is an index to a no-op instruction (like Save), then return the
    /// next pc that is not a no-op instruction.
    pub fn skip(&self, mut pc: usize) -> usize {
        loop {
            match self[pc] {
                Inst::Save(ref i) => pc = i.goto,
                _ => return pc,
            }
        }
    }

    /// Return true if and only if an execution engine at instruction `pc` will
    /// always lead to a match.
    pub fn leads_to_match(&self, pc: usize) -> bool {
        match self[self.skip(pc)] {
            Inst::Match => true,
            _ => false,
        }
    }

    /// Return true if and only if the regex is anchored at the start of
    /// search text.
    pub fn anchored_begin(&self) -> bool {
        match self.get(1) {
            Some(&Inst::EmptyLook(ref inst)) => {
                inst.look == EmptyLook::StartText
            }
            _ => false,
        }
    }

    /// Return true if and only if the regex is anchored at the end of
    /// search text.
    pub fn anchored_end(&self) -> bool {
        match self.get(self.len() - 3) {
            Some(&Inst::EmptyLook(ref inst)) => {
                inst.look == EmptyLook::EndText
            }
            _ => false,
        }
    }

    /// Build a matching engine for all prefix literals in this instruction
    /// sequence.
    ///
    /// If there are no prefix literals (or there are too many), then a
    /// matching engine that never matches is returned.
    pub fn prefix_matcher(&self) -> Literals {
        BuildPrefixes::new(self).literals().into_matcher()
    }
}

impl Deref for Insts {
    type Target = [Inst];

    fn deref(&self) -> &Self::Target {
        &*self.insts
    }
}

/// Inst is an instruction code in a Regex program.
#[derive(Clone, Debug)]
pub enum Inst {
    /// Match indicates that the program has reached a match state.
    Match,
    /// Save causes the program to save the current location of the input in
    /// the slot indicated by InstSave.
    Save(InstSave),
    /// Split causes the program to diverge to one of two paths in the
    /// program, preferring goto1 in InstSplit.
    Split(InstSplit),
    /// EmptyLook represents a zero-width assertion in a regex program. A
    /// zero-width assertion does not consume any of the input text.
    EmptyLook(InstEmptyLook),
    /// Char requires the regex program to match the character in InstChar at
    /// the current position in the input.
    Char(InstChar),
    /// Ranges requires the regex program to match the character at the current
    /// position in the input with one of the ranges specified in InstRanges.
    Ranges(InstRanges),
    Bytes(InstBytes),
}

/// Representation of the Save instruction.
#[derive(Clone, Debug)]
pub struct InstSave {
    /// The next location to execute in the program.
    pub goto: InstIdx,
    /// The capture slot (there are two slots for every capture in a regex,
    /// including the zeroth capture for the entire match).
    pub slot: usize,
}

/// Representation of the Split instruction.
#[derive(Clone, Debug)]
pub struct InstSplit {
    /// The first instruction to try. A match resulting from following goto1
    /// has precedence over a match resulting from following goto2.
    pub goto1: InstIdx,
    /// The second instruction to try. A match resulting from following goto1
    /// has precedence over a match resulting from following goto2.
    pub goto2: InstIdx,
}

/// Representation of the EmptyLook instruction.
#[derive(Clone, Debug)]
pub struct InstEmptyLook {
    /// The next location to execute in the program if this instruction
    /// succeeds.
    pub goto: InstIdx,
    /// The type of zero-width assertion to check.
    pub look: EmptyLook,
}

/// The set of zero-width match instructions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmptyLook {
    /// Start of line or input.
    StartLine,
    /// End of line or input.
    EndLine,
    /// Start of input.
    StartText,
    /// End of input.
    EndText,
    /// Word character on one side and non-word character on other.
    WordBoundary,
    /// Word character on both sides or non-word character on both sides.
    NotWordBoundary,
}

impl InstEmptyLook {
    /// Tests whether the pair of characters matches this zero-width
    /// instruction.
    pub fn matches(&self, c1: Char, c2: Char) -> bool {
        use self::EmptyLook::*;
        match self.look {
            StartLine => c1.is_none() || c1 == '\n',
            EndLine => c2.is_none() || c2 == '\n',
            StartText => c1.is_none(),
            EndText => c2.is_none(),
            ref wbty => {
                let (w1, w2) = (c1.is_word_char(), c2.is_word_char());
                (*wbty == WordBoundary && w1 ^ w2)
                || (*wbty == NotWordBoundary && !(w1 ^ w2))
            }
        }
    }
}

/// Representation of the Char instruction.
#[derive(Clone, Debug)]
pub struct InstChar {
    /// The next location to execute in the program if this instruction
    /// succeeds.
    pub goto: InstIdx,
    /// The character to test.
    pub c: char,
}

/// Representation of the Ranges instruction.
#[derive(Clone, Debug)]
pub struct InstRanges {
    /// The next location to execute in the program if this instruction
    /// succeeds.
    pub goto: InstIdx,
    /// The set of Unicode scalar value ranges to test.
    pub ranges: Vec<(char, char)>,
}

impl InstRanges {
    /// Tests whether the given input character matches this instruction.
    #[inline(always)] // About ~5-15% more throughput then `#[inline]`
    pub fn matches(&self, c: Char) -> bool {
        // This speeds up the `match_class_unicode` benchmark by checking
        // some common cases quickly without binary search. e.g., Matching
        // a Unicode class on predominantly ASCII text.
        for r in self.ranges.iter().take(4) {
            if c < r.0 {
                return false;
            }
            if c <= r.1 {
                return true;
            }
        }
        self.ranges.binary_search_by(|r| {
            if r.1 < c {
                Ordering::Less
            } else if r.0 > c {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }).is_ok()
    }

    pub fn num_chars(&self) -> usize {
        self.ranges.iter()
            .map(|&(s, e)| 1 + (e as u32) - (s as u32))
            .fold(0, |acc, len| acc + len)
            as usize
    }
}

#[derive(Clone, Debug)]
pub struct InstBytes {
    pub goto: InstIdx,
    pub start: u8,
    pub end: u8,
}

impl InstBytes {
    pub fn matches(&self, byte: u8) -> bool {
        self.start <= byte && byte <= self.end
    }
}
