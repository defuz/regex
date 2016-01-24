// Copyright 2014-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use syntax;

use backtrack::{Backtrack, BackMachine};
use compile::Compiler;
use input::{Input, ByteInput, CharInput};
use inst::{EmptyLook, Inst};
use nfa::{Nfa, NfaThreads};
use pool::Pool;
use literals::{BuildPrefixes, Literals};
use re::CaptureIdxs;
use Error;

const NUM_PREFIX_LIMIT: usize = 30;
const PREFIX_LENGTH_LIMIT: usize = 15;

/// The matching engines offered by this regex implementation.
///
/// N.B. This is exported for use in testing.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub enum MatchEngine {
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

/// Program represents a compiled regular expression. Once an expression is
/// compiled, its representation is immutable and will never change.
/// (Well, almost. In fact, the matching engines cache state that can be
/// reused on subsequent searches. But this is interior mutability that
/// shouldn't be observable by the caller.)
#[derive(Debug)]
pub struct Program {
    /// The original regular expression string.
    pub original: String,
    /// A sequence of instructions.
    pub insts: Vec<Inst>,
    /// The sequence of capture group names. There is an entry for each capture
    /// group index and a name exists only if the capture group is named.
    pub cap_names: Vec<Option<String>>,
    /// If the regular expression requires a literal prefix in order to have a
    /// match, that prefix is stored here as a DFA.
    pub prefixes: Literals,
    /// True iff program is anchored at the beginning.
    pub anchored_begin: bool,
    /// True iff program is anchored at the end.
    pub anchored_end: bool,
    /// True iff program should use byte based matching.
    pub bytes: bool,
    /// The type of matching engine to use.
    /// When `None` (the default), pick an engine automatically.
    pub engine: Option<MatchEngine>,
    /// Cached NFA threads.
    pub nfa_threads: Pool<NfaThreads>,
    /// Cached backtracking memory.
    pub backtrack: Pool<BackMachine>,
}

impl Program {
    /// Compiles a Regex.
    pub fn new(
        engine: Option<MatchEngine>,
        bytes: bool,
        size_limit: usize,
        re: &str,
    ) -> Result<Program, Error> {
        let expr = try!(syntax::Expr::parse(re));
        let compiler = Compiler::new(size_limit, bytes);
        let (insts, cap_names) = try!(compiler.compile(&expr));
        let (insts_len, ncaps) = (insts.len(), num_captures(&insts));
        let create_threads = move || NfaThreads::new(insts_len, ncaps);
        let create_backtrack = move || BackMachine::new();
        let prefixes = BuildPrefixes::new(&insts).literals().into_matcher();
        let mut prog = Program {
            original: re.into(),
            insts: insts,
            cap_names: cap_names,
            prefixes: prefixes,
            anchored_begin: false,
            anchored_end: false,
            bytes: bytes,
            engine: engine,
            nfa_threads: Pool::new(Box::new(create_threads)),
            backtrack: Pool::new(Box::new(create_backtrack)),
        };
        prog.anchored_begin = match prog.insts[1] {
            Inst::EmptyLook(ref inst) => inst.look == EmptyLook::StartText,
            _ => false,
        };
        prog.anchored_end = match prog.insts[prog.insts.len() - 3] {
            Inst::EmptyLook(ref inst) => inst.look == EmptyLook::EndText,
            _ => false,
        };
        Ok(prog)
    }

    /// Executes a compiled regex program.
    pub fn exec(
        &self,
        caps: &mut CaptureIdxs,
        text: &str,
        start: usize,
    ) -> bool {
        if self.bytes {
            self.exec_input(caps, ByteInput::new(text), start)
        } else {
            self.exec_input(caps, CharInput::new(text), start)
        }
    }

    fn exec_input<I: Input>(
        &self,
        caps: &mut CaptureIdxs,
        input: I,
        start: usize,
    ) -> bool {
        match self.choose_engine(caps.len(), &input) {
            MatchEngine::Backtrack => {
                Backtrack::exec(self, caps, &input, start)
            }
            MatchEngine::Nfa => {
                Nfa::exec(self, caps, input, start)
            }
            MatchEngine::Literals => {
                match self.prefixes.find(&input.as_bytes()[start..]) {
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
        }
    }

    fn choose_engine<I: Input>(
        &self,
        cap_len: usize,
        input: I,
    ) -> MatchEngine {
        // If the engine is already chosen, then we use it.
        // But that might not be a good idea. e.g., What if `Literals` is
        // chosen and it can't work? I guess we should probably check whether
        // the chosen engine is appropriate or not.
        self.engine.unwrap_or_else(|| {
            if cap_len <= 2
               && self.prefixes.at_match()
               && self.prefixes.preserves_priority() {
                MatchEngine::Literals
            } else if Backtrack::should_exec(self, input) {
                // We're only here if the input and regex combined are small.
                MatchEngine::Backtrack
            } else {
                MatchEngine::Nfa
            }
        })
    }

    /// Returns the total number of capture groups in the regular expression.
    /// This includes the zeroth capture.
    pub fn num_captures(&self) -> usize {
        num_captures(&self.insts)
    }

    /// Allocate new capture groups.
    pub fn alloc_captures(&self) -> Vec<Option<usize>> {
        vec![None; 2 * self.num_captures()]
    }
}

impl Clone for Program {
    fn clone(&self) -> Program {
        let (insts_len, ncaps) = (self.insts.len(), self.num_captures());
        let create_threads = move || NfaThreads::new(insts_len, ncaps);
        let create_backtrack = move || BackMachine::new();
        Program {
            original: self.original.clone(),
            insts: self.insts.clone(),
            cap_names: self.cap_names.clone(),
            prefixes: self.prefixes.clone(),
            anchored_begin: self.anchored_begin,
            anchored_end: self.anchored_end,
            bytes: self.bytes,
            engine: self.engine,
            nfa_threads: Pool::new(Box::new(create_threads)),
            backtrack: Pool::new(Box::new(create_backtrack)),
        }
    }
}

/// Return the number of captures in the given sequence of instructions.
fn num_captures(insts: &[Inst]) -> usize {
    let mut n = 0;
    for inst in insts {
        if let Inst::Save(ref inst) = *inst {
            n = ::std::cmp::max(n, inst.slot + 1)
        }
    }
    // There's exactly 2 Save slots for every capture.
    n / 2
}

/// Count the number of characters in the given range.
///
/// This is useful for pre-emptively limiting the number of prefix literals
/// we extract from a regex program.
fn num_chars_in_ranges(ranges: &[(char, char)]) -> usize {
    ranges.iter()
          .map(|&(s, e)| 1 + (e as u32) - (s as u32))
          .fold(0, |acc, len| acc + len) as usize
}
