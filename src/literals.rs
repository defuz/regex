// Copyright 2014-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// For a full AC automaton, limit to ~250 bytes. This uses somewhere
// around 250KB.
//
// For a less memory intensive AC automaton, limit to ~3000 bytes. This also
// uses somewhere around 250KB.

use std::char;
use std::collections::HashSet;
use std::fmt;
use std::mem;

use aho_corasick::{Automaton, AcAutomaton, FullAcAutomaton};
use memchr::memchr;

use char_utf8::encode_utf8;
use inst::{Insts, Inst, InstBytes, InstRanges};

pub struct AlternateLiterals {
    at_match: bool,
    literals: Vec<Vec<u8>>,
}

impl AlternateLiterals {
    pub fn into_matcher(self) -> Literals {
        if self.literals.is_empty() {
            Literals::empty()
        } else {
            let at_match = self.at_match;
            Literals {
                at_match: at_match,
                matcher: LiteralMatcher::new(self),
            }
        }
    }

    fn empty() -> AlternateLiterals {
        AlternateLiterals { at_match: false, literals: vec![] }
    }

    fn is_empty(&self) -> bool {
        self.literals.is_empty()
    }

    fn is_single_byte(&self) -> bool {
        self.literals.len() == 1 && self.literals[0].len() == 1
    }

    fn all_single_bytes(&self) -> bool {
        self.literals.len() >= 1 && self.literals.iter().all(|s| s.len() == 1)
    }

    fn is_one_literal(&self) -> bool {
        self.literals.len() == 1
    }

    fn num_bytes(&self) -> usize {
        self.literals.iter().map(|lit| lit.len()).fold(0, |acc, len| acc + len)
    }

    fn add_alternates(&mut self, alts: AlternateLiterals) {
        self.at_match = self.at_match && alts.at_match;
        self.literals.extend(alts.literals);
    }

    fn add_literal_char(&mut self, c: char) {
        let scratch = &mut [0; 4];
        let n = encode_utf8(c, scratch).unwrap();
        for alt in &mut self.literals {
            alt.extend(&scratch[0..n]);
        }
    }

    fn add_literal_char_ranges(&mut self, inst: &InstRanges) {
        // This is tricky. We need to think of each range as its own set of
        // alternations. For example, `[a-cx-z]` is comprised of two ranges:
        // `a-c` and `x-z`. This is equivalent to the regex `a|b|c|x|y|z`. If
        // we've already found two prefixes, e.g., `foo|bar`, then we need to
        // extend all such prefixes with all alternates here. For e.g., `fooa`,
        // ..., `fooz`, `bara`, ..., `barz`.
        //
        // To achieve this, we copy our existing literals for every char!
        let scratch = &mut [0; 4];
        let nlits = self.literals.len();
        let orig = mem::replace(&mut self.literals, Vec::with_capacity(nlits));
        for &(s, e) in &inst.ranges {
            for c in (s as u32)..(e as u32 + 1) {
                for alt in &orig {
                    let mut alt = alt.clone();
                    let ch = char::from_u32(c).unwrap();
                    let n = encode_utf8(ch, scratch).unwrap();

                    alt.extend(&scratch[0..n]);
                    self.literals.push(alt);
                }
            }
        }
    }

    fn add_literal_byte_range(&mut self, inst: &InstBytes) {
        // Pretty much the same process as for literal char ranges, but we
        // only have one range.
        let nlits = self.literals.len();
        let orig = mem::replace(&mut self.literals, Vec::with_capacity(nlits));
        for b in inst.start..(inst.end + 1) {
            for alt in &orig {
                let mut alt = alt.clone();
                alt.push(b);
                self.literals.push(alt);
            }
        }
    }
}

pub struct BuildPrefixes<'a> {
    insts: &'a Insts,
    limit: usize,
    alts: AlternateLiterals,
}

impl<'a> BuildPrefixes<'a> {
    pub fn new(insts: &'a Insts) -> Self {
        BuildPrefixes {
            insts: insts,
            limit: 3000,
            alts: AlternateLiterals { at_match: true, literals: vec![] },
        }
    }

    pub fn literals(mut self) -> AlternateLiterals {
        let mut stack = vec![self.insts.skip(1)];
        let mut seen = HashSet::new();
        while let Some(mut pc) = stack.pop() {
            seen.insert(pc);
            pc = self.insts.skip(pc);
            if let Inst::Split(ref inst) = self.insts[pc] {
                if !seen.contains(&inst.goto2) {
                    stack.push(inst.goto2);
                }
                if !seen.contains(&inst.goto1) {
                    stack.push(inst.goto1);
                }
                continue;
            }
            // When searching for required literals, set the local limit to
            // something a bit less than our real limit. This prevents a single
            // alternation from blowing our budget in most cases. (If a single
            // alt blows the budget, then we can't consume literals from other
            // alts, which means we end up with nothing to show for it.)
            //
            // For example, consider `a?[0-9]{3}`. This splits into two regexes
            // `a[0-9]{3}` and `[0-9]{3}`. The latter regex can be expanded
            // completely into a set of alternate literals that consumes
            // exactly 3000 bytes. This is our entire budget by default.
            // Therefore, we're left with no room to add the second branch
            // (`a[0-9]{3}`) to our set of literals. If we can't represent
            // all required alternates, then we have to give up. Therefore, as
            // a heuristic, limit what each alternate is allowed to use. In
            // this case, `[0-9]{3}` will only gather literals for `[0-9]{2}`,
            // which leaves more than enough room for our second branch.
            let alts = BuildRequiredLiterals::new(self.insts)
                                             .set_limit(self.limit / 10)
                                             .literals(pc);
            if alts.is_empty() {
                // If we couldn't find any literals required in this path
                // through the program, then we can't conclude anything about
                // prefix literals for this program. For example, if the regex
                // is `a|b*`, then the second alternate has no prefix to search
                // for. (`b*` matches the empty string!)
                return AlternateLiterals::empty();
            }
            if self.alts.num_bytes() + alts.num_bytes() > self.limit {
                // We've blown our budget. Give up.
                // We could do something a little smarter here and try to trim
                // the literals we've got here. (e.g., If every literal is two
                // characters, then it would be legal to remove the second char
                // from every literal.)
                return AlternateLiterals::empty();
            }
            self.alts.add_alternates(alts);
        }
        self.alts
    }
}

pub struct BuildRequiredLiterals<'a> {
    insts: &'a Insts,
    limit: usize,
    alts: AlternateLiterals,
}

impl<'a> BuildRequiredLiterals<'a> {
    pub fn new(insts: &'a Insts) -> Self {
        BuildRequiredLiterals {
            insts: insts,
            limit: 3000,
            alts: AlternateLiterals { at_match: true, literals: vec![vec![]] },
        }
    }

    pub fn set_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    fn literals(mut self, mut pc: usize) -> AlternateLiterals {
        use inst::Inst::*;
        loop {
            let inst = &self.insts[pc];
            match *inst {
                Save(ref inst) => pc = inst.goto,
                Char(ref inst) => {
                    if !self.add_literal_char(inst.c) {
                        self.alts.at_match = false;
                        break;
                    }
                    pc = inst.goto;
                }
                Ranges(ref inst) => {
                    if !self.add_literal_char_ranges(inst) {
                        self.alts.at_match = false;
                        break;
                    }
                    pc = inst.goto;
                }
                Bytes(ref inst) => {
                    if !self.add_literal_byte_range(inst) {
                        self.alts.at_match = false;
                        break;
                    }
                    pc = inst.goto;
                }
                Split(_) | EmptyLook(_) | Match => {
                    self.alts.at_match = self.insts.leads_to_match(pc);
                    break;
                }
            }
        }
        if self.alts.literals.len() == 1 && self.alts.literals[0].is_empty() {
            AlternateLiterals::empty()
        } else {
            self.alts
        }
    }

    fn add_literal_char(&mut self, c: char) -> bool {
        if self.alts.num_bytes() + 1 > self.limit {
            return false;
        }
        self.alts.add_literal_char(c);
        true
    }

    fn add_literal_char_ranges(&mut self, inst: &InstRanges) -> bool {
        // Compute roughly how many bytes will be in our literals following
        // the addition of the given ranges. If we blow our limit, then we
        // can't add *any* of them.
        let nchars = inst.num_chars();
        let new_byte_count = (self.alts.num_bytes() * nchars)
                             + (self.alts.literals.len() * nchars);
        if new_byte_count > self.limit {
            return false;
        }
        self.alts.add_literal_char_ranges(inst);
        true
    }

    fn add_literal_byte_range(&mut self, inst: &InstBytes) -> bool {
        // Compute roughly how many bytes will be in our literals following
        // the addition of the given range. If we blow our limit, then we
        // can't add anything.
        let nbytes = (inst.end - inst.start + 1) as usize;
        let new_byte_count = (self.alts.num_bytes() * nbytes)
                             + (self.alts.literals.len() * nbytes);
        if new_byte_count > self.limit {
            return false;
        }
        self.alts.add_literal_byte_range(inst);
        true
    }
}

/// A prefix extracted from a compiled regular expression.
///
/// A regex prefix is a set of literal strings that *must* be matched at the
/// beginning of a regex in order for the entire regex to match.
///
/// There are a variety of ways to efficiently scan the search text for a
/// prefix. Currently, there are three implemented:
///
/// 1. The prefix is a single byte. Just use memchr.
/// 2. If the prefix is a set of two or more single byte prefixes, then
///    a single sparse map is created. Checking if there is a match is a lookup
///    in this map for each byte in the search text.
/// 3. In all other cases, build an Aho-Corasick automaton.
///
/// It's possible that there's room here for other substring algorithms,
/// such as Boyer-Moore for single-set prefixes greater than 1, or Rabin-Karp
/// for small sets of same-length prefixes.
#[derive(Clone)]
pub struct Literals {
    at_match: bool,
    matcher: LiteralMatcher,
}

#[derive(Clone)]
enum LiteralMatcher {
    /// No prefixes. (Never advances through the input.)
    Empty,
    /// A single byte prefix.
    Byte(u8),
    /// A set of two or more single byte prefixes.
    /// This could be reduced to a bitset, which would use only 8 bytes,
    /// but I don't think we care.
    Bytes {
        chars: Vec<u8>,
        sparse: Vec<bool>,
    },
    Single(SingleSearch),
    /// A full Aho-Corasick DFA. A "full" DFA in this case means that all of
    /// the failure transitions have been expanded and the entire DFA is
    /// represented by a memory inefficient sparse matrix. This makes matching
    /// extremely fast. We only use this "full" DFA when the number of bytes
    /// in our literals does not exceed 250. This generally leads to a DFA that
    /// consumes 250KB of memory.
    FullAutomaton(FullAcAutomaton<Vec<u8>>),
    /// An Aho-Corasick DFA.
    ///
    /// This is more memory efficient than a "full" AC DFA, but is slower at
    /// matching. Therefore, we use it catch all other cases of alternate
    /// literals. (3000 bytes in the alternating literals leads to about 250KB
    /// of memory usage for an Aho-Corasick DFA.)
    Automaton(AcAutomaton<Vec<u8>>),
}

impl Literals {
    /// Returns a matcher that never matches and never advances the input.
    fn empty() -> Self {
        Literals { at_match: false, matcher: LiteralMatcher::Empty }
    }

    /// Returns true if and only if a literal match corresponds to a match
    /// in the regex from which the literal was extracted.
    pub fn at_match(&self) -> bool {
        self.at_match
    }

    /// Find the position of a prefix in `haystack` if it exists.
    ///
    /// In the matching engines, we only actually need the starting index
    /// because the prefix is used to only skip ahead---the matching engine
    /// still needs to run over the prefix input. However, we return the ending
    /// location as well in case the prefix corresponds to the entire regex,
    /// in which case, you need the end of the match.
    pub fn find(&self, haystack: &[u8]) -> Option<(usize, usize)> {
        use self::LiteralMatcher::*;
        match self.matcher {
            Empty => Some((0, 0)),
            Byte(b) => memchr(b, haystack).map(|i| (i, i+1)),
            Bytes { ref sparse, .. } => {
                find_singles(sparse, haystack)
            }
            Single(ref searcher) => {
                searcher.find(haystack).map(|i| (i, i + searcher.pat.len()))
            }
            FullAutomaton(ref aut) => {
                aut.find(haystack).next().map(|m| (m.start, m.end))
            }
            Automaton(ref aut) => {
                aut.find(haystack).next().map(|m| (m.start, m.end))
            }
        }
    }

    /// Returns true iff this prefix is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the number of prefixes in this machine.
    pub fn len(&self) -> usize {
        use self::LiteralMatcher::*;
        match self.matcher {
            Empty => 0,
            Byte(_) => 1,
            Bytes { ref chars, .. } => chars.len(),
            Single(_) => 1,
            FullAutomaton(ref aut) => aut.len(),
            Automaton(ref aut) => aut.len(),
        }
    }

    /// Returns true iff the prefix match preserves priority.
    ///
    /// For example, given the alternation `ab|a` and the target string `ab`,
    /// does the prefix machine guarantee that `ab` will match? (A full
    /// Aho-Corasick automaton does not!)
    pub fn preserves_priority(&self) -> bool {
        use self::LiteralMatcher::*;
        match self.matcher {
            Empty => true,
            Byte(_) => true,
            Bytes{..} => true,
            Single(_) => true,
            FullAutomaton(ref aut) => {
                // See comments for 'Automaton' branch.
                aut.patterns().iter().all(|p| p.len() == aut.pattern(0).len())
            }
            Automaton(ref aut) => {
                // Okay, so the automaton can respect priority in one
                // particular case: when every pattern is of the same length.
                // The trick is that the automaton will report the leftmost
                // match, which in this case, corresponds to the correct
                // match for the regex engine. If any other alternate matches
                // at the same position, then they must be exactly equivalent.

                // Guaranteed at least one prefix by construction, so use
                // that for the length.
                aut.patterns().iter().all(|p| p.len() == aut.pattern(0).len())
            }
        }
    }

    /// Returns all of the prefixes participating in this machine.
    ///
    /// For debug/testing only! (It allocates.)
    #[allow(dead_code)]
    pub fn prefixes(&self) -> Vec<String> {
        use self::LiteralMatcher::*;
        match self.matcher {
            Empty => vec![],
            Byte(b) => vec![format!("{}", b as char)],
            Bytes { ref chars, .. } => {
                chars.iter().map(|&b| format!("{}", b as char)).collect()
            }
            Single(ref searcher) => {
                let pat = String::from_utf8(searcher.pat.clone()).unwrap();
                vec![pat]
            }
            FullAutomaton(ref aut) => {
                aut
                .patterns()
                .iter()
                .map(|p| String::from_utf8(p.clone()).unwrap())
                .collect()
            }
            Automaton(ref aut) => {
                aut
                .patterns()
                .iter()
                .map(|p| String::from_utf8(p.clone()).unwrap())
                .collect()
            }
        }
    }
}

impl LiteralMatcher {
    /// Create a new prefix matching machine.
    fn new(mut alts: AlternateLiterals) -> Self {
        use self::LiteralMatcher::*;

        if alts.is_empty() {
            Empty
        } else if alts.is_single_byte() {
            Byte(alts.literals[0][0])
        } else if alts.all_single_bytes() {
            let mut set = vec![false; 256];
            let mut bytes = vec![];
            for lit in alts.literals {
                bytes.push(lit[0]);
                set[lit[0] as usize] = true;
            }
            Bytes { chars: bytes, sparse: set }
        } else if alts.is_one_literal() {
            Single(SingleSearch::new(alts.literals.pop().unwrap()))
        } else if alts.num_bytes() <= 250 {
            FullAutomaton(AcAutomaton::new(alts.literals).into_full())
        } else {
            Automaton(AcAutomaton::new(alts.literals))
        }
    }
}

/// Provides an implementation of fast subtring search.
///
/// In particular, this uses Boyer-Moore-Horspool with Tim Raita's twist:
/// https://en.wikipedia.org/wiki/Raita_Algorithm
///
/// I'm skeptical of the utility here, because benchmarks suggest that it is
/// difficult to beat Aho-Corasick on random text. Namely, both algorithms are
/// dominated by the performance of `memchr` for the leading byte prefix.
/// With that said, BMH does seem to surpass AC when the search text gets
/// longer (see the `easy0_1MB` vs. `easy1_1MB` benchmarks).
///
/// More analysis needs to be done to test this on different search texts.
#[derive(Clone, Debug)]
pub struct SingleSearch {
    pat: Vec<u8>,
    shift: Vec<usize>,
}

impl SingleSearch {
    fn new(pat: Vec<u8>) -> SingleSearch {
        assert!(pat.len() >= 1);
        let mut shift = vec![pat.len(); 256];
        for i in 0..(pat.len() - 1) {
            shift[pat[i] as usize] = pat.len() - i - 1;
        }
        SingleSearch {
            pat: pat,
            shift: shift,
        }
    }

    fn find(&self, haystack: &[u8]) -> Option<usize> {
        let pat = &*self.pat;
        if haystack.len() < pat.len() {
            return None;
        }
        let mut i = match memchr(pat[0], haystack) {
            None => return None,
            Some(i) => i,
        };
        while i <= haystack.len() - pat.len() {
            let b = haystack[i + pat.len() - 1];
            if b == pat[pat.len() - 1]
               && haystack[i] == pat[0]
               && haystack[i + (pat.len() / 2)] == pat[pat.len() / 2]
               && pat == &haystack[i..i + pat.len()] {
                return Some(i);
            }
            i += self.shift[b as usize];
            i += match memchr(pat[0], &haystack[i..]) {
                None => return None,
                Some(i) => i,
            };
        }
        None
    }
}

/// A quick scan for multiple single byte prefixes using a sparse map.
fn find_singles(sparse: &[bool], haystack: &[u8]) -> Option<(usize, usize)> {
    // TODO: Improve this with ideas found in jetscii crate.
    for (hi, &b) in haystack.iter().enumerate() {
        if sparse[b as usize] {
            return Some((hi, hi+1));
        }
    }
    None
}

impl fmt::Debug for Literals {
    #[allow(deprecated)] // connect => join in 1.3
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::LiteralMatcher::*;
        try!(write!(f, "complete? {}, matcher: ", self.at_match));
        match self.matcher {
            Empty => write!(f, "Empty"),
            Byte(b) => write!(f, "{:?}", b as char),
            Bytes { ref chars, .. } => {
                let chars: Vec<String> =
                    chars.iter()
                         .map(|&c| format!("{:?}", c as char))
                         .collect();
                write!(f, "{}", chars.connect(", "))
            }
            Single(ref searcher) => write!(f, "{:?}", searcher),
            FullAutomaton(ref aut) => write!(f, "{:?}", aut),
            Automaton(ref aut) => write!(f, "{:?}", aut),
        }
    }
}

#[cfg(test)]
mod tests {
    use program::Program;

    macro_rules! prog {
        ($re:expr) => { Program::unicode($re, 1 << 30).unwrap() }
    }

    macro_rules! prefixes {
        ($re:expr) => {{
            let p = prog!($re);
            assert!(!p.prefixes.at_match());
            p.prefixes.prefixes()
        }}
    }
    macro_rules! prefixes_complete {
        ($re:expr) => {{
            let p = prog!($re);
            assert!(p.prefixes.at_match());
            p.prefixes.prefixes()
        }}
    }

    #[test]
    fn single() {
        assert_eq!(prefixes_complete!("a"), vec!["a"]);
        assert_eq!(prefixes_complete!("[a]"), vec!["a"]);
        assert_eq!(prefixes!("a+"), vec!["a"]);
        assert_eq!(prefixes!("(?:a)+"), vec!["a"]);
        assert_eq!(prefixes!("(a)+"), vec!["a"]);
    }

    #[test]
    fn single_alt() {
        assert_eq!(prefixes_complete!("a|b"), vec!["a", "b"]);
        assert_eq!(prefixes_complete!("b|a"), vec!["b", "a"]);
        assert_eq!(prefixes_complete!("[a]|[b]"), vec!["a", "b"]);
        assert_eq!(prefixes!("a+|b"), vec!["a", "b"]);
        assert_eq!(prefixes!("a|b+"), vec!["a", "b"]);
        assert_eq!(prefixes!("(?:a+)|b"), vec!["a", "b"]);
        assert_eq!(prefixes!("(a+)|b"), vec!["a", "b"]);
    }

    #[test]
    fn many() {
        assert_eq!(prefixes_complete!("abcdef"), vec!["abcdef"]);
        assert_eq!(prefixes!("abcdef+"), vec!["abcdef"]);
        assert_eq!(prefixes!("(?:abcdef)+"), vec!["abcdef"]);
        assert_eq!(prefixes!("(abcdef)+"), vec!["abcdef"]);
    }

    #[test]
    fn many_alt() {
        assert_eq!(prefixes_complete!("abc|def"), vec!["abc", "def"]);
        assert_eq!(prefixes_complete!("def|abc"), vec!["def", "abc"]);
        assert_eq!(prefixes!("abc+|def"), vec!["abc", "def"]);
        assert_eq!(prefixes!("abc|def+"), vec!["abc", "def"]);
        assert_eq!(prefixes!("(?:abc)+|def"), vec!["abc", "def"]);
        assert_eq!(prefixes!("(abc)+|def"), vec!["abc", "def"]);
    }

    #[test]
    fn class() {
        assert_eq!(prefixes_complete!("[0-9]"), vec![
            "0", "1", "2", "3", "4", "5", "6", "7", "8", "9",
        ]);
        assert_eq!(prefixes!("[0-9]+"), vec![
            "0", "1", "2", "3", "4", "5", "6", "7", "8", "9",
        ]);
    }

    #[test]
    fn preceding_alt() {
        assert_eq!(prefixes!("(?:a|b).+"), vec!["a", "b"]);
        assert_eq!(prefixes!("(a|b).+"), vec!["a", "b"]);
    }

    #[test]
    fn nested_alt() {
        assert_eq!(prefixes_complete!("(a|b|c|d)"),
                   vec!["a", "b", "c", "d"]);
        assert_eq!(prefixes_complete!("((a|b)|(c|d))"),
                   vec!["a", "b", "c", "d"]);
    }
}
