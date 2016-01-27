// Copyright 2014-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use backtrack::{self, Backtrack};
use input::{ByteInput, CharInput};
use nfa::Nfa;
use program::Program;
use re::CaptureIdxs;
use Error;

/// Executor manages the execution of a regular expression.
///
/// In particular, this manages the various compiled forms of a single regular
/// expression and the choice of which matching engine to use to execute a
/// regular expression.
#[derive(Clone, Debug)]
pub struct Executor {
    /// A compiled program that executes a regex on Unicode codepoints.
    /// This can be Unicode based, byte based or have both.
    prog: Prog,
    /// A preference for matching engine selection.
    ///
    /// This defaults to Automatic, which means the matching engine is selected
    /// based on heuristics (such as the nature and size of the compiled
    /// program, in addition to the size of the search text).
    ///
    /// If either Nfa or Backtrack is set, then it is always used because
    /// either is capable of executing every compiled program on any input
    /// size.
    ///
    /// If anything else is set, the behavior is currently identical to
    /// Automatic.
    match_engine: MatchEngine,
}

impl Executor {
    pub fn new(
        re: &str,
        match_engine: MatchEngine,
        size_limit: usize,
        bytes: bool,
    ) -> Result<Executor, Error> {
        let prog = if bytes {
            Prog::Bytes(try!(Program::bytes(re, size_limit)))
        } else {
            // TODO: Check if the program can be executed by a DFA.
            // If so, compile the bytes program too.
            Prog::Unicode(try!(Program::unicode(re, size_limit)))
        };
        Ok(Executor {
            prog: prog,
            match_engine: match_engine,
        })
    }

    pub fn regex_str(&self) -> &str {
        match self.prog {
            Prog::Unicode(ref p) => &p.original,
            Prog::Bytes(ref p) => &p.original,
            Prog::Both { ref unicode, .. } => &unicode.original,
        }
    }

    pub fn capture_names(&self) -> &[Option<String>] {
        match self.prog {
            Prog::Unicode(ref p) => &p.cap_names,
            Prog::Bytes(ref p) => &p.cap_names,
            Prog::Both { ref unicode, .. } => &unicode.cap_names,
        }
    }

    pub fn alloc_captures(&self) -> Vec<Option<usize>> {
        match self.prog {
            Prog::Unicode(ref p) => p.alloc_captures(),
            Prog::Bytes(ref p) => p.alloc_captures(),
            Prog::Both { ref unicode, .. } => unicode.alloc_captures(),
        }
    }

    pub fn exec(
        &self,
        caps: &mut CaptureIdxs,
        text: &str,
        start: usize,
    ) -> bool {
        match self.match_engine {
            MatchEngine::Nfa => self.exec_nfa(caps, text, start),
            MatchEngine::Backtrack => self.exec_backtrack(caps, text, start),
            MatchEngine::Literals => self.exec_literals(caps, text, start),
            MatchEngine::Automatic => self.exec_auto(caps, text, start),
        }
    }

    fn exec_auto(
        &self,
        caps: &mut CaptureIdxs,
        text: &str,
        start: usize,
    ) -> bool {
        if self.can_exec_literals(caps.len()) {
            return self.exec_literals(caps, text, start);
        } else if backtrack::should_exec(self.prog.num_insts(), text.len()) {
            self.exec_backtrack(caps, text, start)
        } else {
            self.exec_nfa(caps, text, start)
        }
    }

    fn exec_nfa(
        &self,
        caps: &mut CaptureIdxs,
        text: &str,
        start: usize,
    ) -> bool {
        match self.prog {
            Prog::Unicode(ref p) => {
                Nfa::exec(p, caps, CharInput::new(text), start)
            }
            Prog::Bytes(ref p) => {
                Nfa::exec(p, caps, ByteInput::new(text), start)
            }
            Prog::Both { ref unicode, .. } => {
                Nfa::exec(unicode, caps, CharInput::new(text), start)
            }
        }
    }

    fn exec_backtrack(
        &self,
        caps: &mut CaptureIdxs,
        text: &str,
        start: usize,
    ) -> bool {
        match self.prog {
            Prog::Unicode(ref p) => {
                Backtrack::exec(p, caps, CharInput::new(text), start)
            }
            Prog::Bytes(ref p) => {
                Backtrack::exec(p, caps, ByteInput::new(text), start)
            }
            Prog::Both { ref unicode, .. } => {
                Backtrack::exec(unicode, caps, CharInput::new(text), start)
            }
        }
    }

    fn exec_literals(
        &self,
        caps: &mut CaptureIdxs,
        text: &str,
        start: usize,
    ) -> bool {
        if !self.can_exec_literals(caps.len()) {
            return self.exec_auto(caps, text, start);
        }
        let pos = match self.prog {
            Prog::Unicode(ref p) => {
                p.prefixes.find(&text.as_bytes()[start..])
            }
            Prog::Bytes(ref p) => {
                p.prefixes.find(&text.as_bytes()[start..])
            }
            Prog::Both { ref unicode, .. } => {
                assert!(unicode.is_prefix_match());
                unicode.prefixes.find(&text.as_bytes()[start..])
            }
        };
        match pos {
            None => false,
            Some((s, e)) => {
                if caps.len() == 2 {
                    caps[0] = Some(start + s);
                    caps[1] = Some(start + e);
                }
                true
            }
        }
    }

    fn can_exec_literals(&self, cap_len: usize) -> bool {
        cap_len <= 2 && self.prog.is_prefix_match()
    }
}

#[derive(Clone, Debug)]
enum Prog {
    Unicode(Program),
    Bytes(Program),
    Both { unicode: Program, bytes: Program },
}

impl Prog {
    fn is_prefix_match(&self) -> bool {
        match *self {
            Prog::Unicode(ref p) => p.is_prefix_match(),
            Prog::Bytes(ref p) => p.is_prefix_match(),
            Prog::Both { ref unicode, .. } => unicode.is_prefix_match(),
        }
    }

    fn num_insts(&self) -> usize {
        match *self {
            Prog::Unicode(ref p) => p.insts.len(),
            Prog::Bytes(ref p) => p.insts.len(),
            Prog::Both { ref unicode, .. } => unicode.insts.len()
        }
    }
}

/// The matching engines offered by this regex implementation.
///
/// N.B. This is exported for use in testing.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub enum MatchEngine {
    /// Automatically choose the best matching engine based on heuristics.
    Automatic,
    /// A bounded backtracking implementation. About twice as fast as the
    /// NFA, but can only work on small regexes and small input.
    Backtrack,
    /// A full NFA simulation. Can always be employed but almost always the
    /// slowest choice.
    Nfa,
    /// If the entire regex is a literal and no capture groups have been
    /// requested, then we can degrade to a simple substring match.
    Literals,
}
