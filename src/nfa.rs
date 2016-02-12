// Copyright 2014-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// FIXME: Currently, the VM simulates an NFA. It would be nice to have another
// VM that simulates a DFA.
//
// According to Russ Cox[1], a DFA performs better than an NFA, principally
// because it reuses states previously computed by the machine *and* doesn't
// keep track of capture groups. The drawback of a DFA (aside from its
// complexity) is that it can't accurately return the locations of submatches.
// The NFA *can* do that. (This is my understanding anyway.)
//
// Cox suggests that a DFA ought to be used to answer "does this match" and
// "where does it match" questions. (In the latter, the starting position of
// the match is computed by executing the regex backwards.) Cox also suggests
// that a DFA should be run when asking "where are the submatches", which can
// 1) quickly answer "no" is there's no match and 2) discover the substring
// that matches, which means running the NFA on smaller input.
//
// Currently, the NFA simulation implemented below does some dirty tricks to
// avoid tracking capture groups when they aren't needed (which only works
// for 'is_match', not 'find'). This is a half-measure, but does provide some
// perf improvement.
//
// AFAIK, the DFA/NFA approach is implemented in RE2/C++ but *not* in RE2/Go.
//
// UPDATE: We now have a backtracking matching engine and a DFA for prefix
// matching. The prefix DFA is used in both the NFA simulation below and the
// backtracking engine to skip along the input quickly.
//
// [1] - http://swtch.com/~rsc/regex/regex3.html

use input::{Input, InputAt};
use program::Program;
use re::CaptureIdxs;

/// An NFA simulation matching engine.
#[derive(Debug)]
pub struct Nfa<'r, I> {
    prog: &'r Program,
    input: I,
}

/// A cached allocation that can be reused on each execution.
#[derive(Debug)]
pub struct NfaCache {
    threads: NfaThreads,
}

impl NfaCache {
    /// Create a new allocation used by the NFA machine to record execution
    /// and captures.
    pub fn new() -> Self {
        NfaCache { threads: NfaThreads::new() }
    }
}

impl<'r, I: Input> Nfa<'r, I> {
    /// Execute the NFA matching engine.
    ///
    /// If there's a match, `exec` returns `true` and populates the given
    /// captures accordingly.
    pub fn exec(
        prog: &'r Program,
        mut caps: &mut CaptureIdxs,
        input: I,
        start: usize,
    ) -> bool {
        let mut cache = prog.cache_nfa();
        cache.threads.resize(prog.insts.len(), prog.num_captures());
        let at = input.at(start);
        Nfa {
            prog: prog,
            input: input,
        }.exec_(&mut cache.threads, &mut caps, at)
    }

    fn exec_(
        &mut self,
        mut q: &mut NfaThreads,
        mut caps: &mut CaptureIdxs,
        mut at: InputAt,
    ) -> bool {
        let mut matched = false;
        q.clist.clear(); q.nlist.clear();
'LOOP:  loop {
            if q.clist.size == 0 {
                // Three ways to bail out when our current set of threads is
                // empty.
                //
                // 1. We have a match---so we're done exploring any possible
                //    alternatives.  Time to quit.
                //
                // 2. If the expression starts with a '^' we can terminate as
                //    soon as the last thread dies.
                if matched
                   || (!at.is_beginning() && self.prog.anchored_begin) {
                    break;
                }

                // 3. If there's a literal prefix for the program, try to
                //    jump ahead quickly. If it can't be found, then we can
                //    bail out early.
                if !self.prog.prefixes.is_empty() {
                    at = match self.input.prefix_at(&self.prog.prefixes, at) {
                        None => break,
                        Some(at) => at,
                    };
                }
            }

            // This simulates a preceding '.*?' for every regex by adding
            // a state starting at the current position in the input for the
            // beginning of the program only if we don't already have a match.
            if q.clist.size == 0 || (!self.prog.anchored_begin && !matched) {
                self.add(&mut q.clist, &mut caps, 0, at)
            }
            // The previous call to "add" actually inspects the position just
            // before the current character. For stepping through the machine,
            // we can to look at the current character, so we advance the
            // input.
            let at_next = self.input.at(at.next_pos());
            for i in 0..q.clist.size {
                let pc = q.clist.pc(i);
                let tcaps = q.clist.caps(i);
                if self.step(&mut q.nlist, caps, tcaps, pc, at, at_next) {
                    matched = true;
                    if caps.len() == 0 {
                        // If we only care if a match occurs (not its
                        // position), then we can quit right now.
                        break 'LOOP;
                    }
                    // We don't need to check the rest of the threads in this
                    // set because we've matched something ("leftmost-first").
                    // However, we still need to check threads in the next set
                    // to support things like greedy matching.
                    break;
                }
            }
            if at.is_end() {
                break;
            }
            at = at_next;
            q.swap();
            q.nlist.clear();
        }
        matched
    }

    fn step(
        &self,
        nlist: &mut Threads,
        caps: &mut [Option<usize>],
        thread_caps: &mut [Option<usize>],
        pc: usize,
        at: InputAt,
        at_next: InputAt,
    ) -> bool {
        use inst::Inst::*;
        match self.prog.insts[pc] {
            Match => {
                for (slot, val) in caps.iter_mut().zip(thread_caps.iter()) {
                    *slot = *val;
                }
                true
            }
            Char(ref inst) => {
                if inst.c == at.char() {
                    self.add(nlist, thread_caps, inst.goto, at_next);
                }
                false
            }
            Ranges(ref inst) => {
                if inst.matches(at.char()) {
                    self.add(nlist, thread_caps, inst.goto, at_next);
                }
                false
            }
            Bytes(ref inst) => {
                if let Some(b) = at.byte() {
                    if inst.matches(b) {
                        self.add(nlist, thread_caps, inst.goto, at_next);
                    }
                }
                false
            }
            EmptyLook(_) | Save(_) | Split(_) => false,
        }
    }

    fn add(
        &self,
        nlist: &mut Threads,
        thread_caps: &mut [Option<usize>],
        pc: usize,
        at: InputAt,
    ) {
        use inst::Inst::*;

        if nlist.contains(pc) {
            return
        }
        let ti = nlist.add(pc);
        match self.prog.insts[pc] {
            EmptyLook(ref inst) => {
                let prev = self.input.previous_char(at);
                let next = self.input.next_char(at);
                if inst.matches(prev, next) {
                    self.add(nlist, thread_caps, inst.goto, at);
                }
            }
            Save(ref inst) => {
                if inst.slot >= thread_caps.len() {
                    self.add(nlist, thread_caps, inst.goto, at);
                } else {
                    let old = thread_caps[inst.slot];
                    thread_caps[inst.slot] = Some(at.pos());
                    self.add(nlist, thread_caps, inst.goto, at);
                    thread_caps[inst.slot] = old;
                }
            }
            Split(ref inst) => {
                self.add(nlist, thread_caps, inst.goto1, at);
                self.add(nlist, thread_caps, inst.goto2, at);
            }
            Match | Char(_) | Ranges(_) | Bytes(_) => {
                let mut t = &mut nlist.thread(ti);
                for (slot, val) in t.caps.iter_mut().zip(thread_caps.iter()) {
                    *slot = *val;
                }
            }
        }
    }
}

/// Shared cached state between multiple invocations of a NFA engine
/// in the same thread.
///
/// It is exported so that it can be cached by `program::Program`.
#[derive(Debug)]
struct NfaThreads {
    clist: Threads,
    nlist: Threads,
}

#[derive(Debug)]
struct Threads {
    dense: Vec<Thread>,
    sparse: Vec<usize>,
    size: usize,
}

#[derive(Clone, Debug)]
struct Thread {
    pc: usize,
    caps: Vec<Option<usize>>,
}

impl NfaThreads {
    fn new() -> NfaThreads {
        NfaThreads { clist: Threads::new(), nlist: Threads::new(), }
    }

    fn resize(&mut self, num_insts: usize, ncaps: usize) {
        self.clist.resize(num_insts, ncaps);
        self.nlist.resize(num_insts, ncaps);
    }

    fn swap(&mut self) {
        ::std::mem::swap(&mut self.clist, &mut self.nlist);
    }
}

impl Threads {
    fn new() -> Threads {
        Threads { dense: vec![], sparse: vec![], size: 0 }
    }

    fn resize(&mut self, num_insts: usize, ncaps: usize) {
        let old_slots = self.dense.get(0).map_or(0, |t| t.caps.len());
        let new_slots = ncaps * 2;
        if num_insts != self.dense.len() || old_slots != new_slots {
            let t = Thread { pc: 0, caps: vec![None; ncaps * 2] };
            *self = Threads {
                dense: vec![t; num_insts],
                sparse: vec![0; num_insts],
                size: 0,
            }
        }
    }

    fn add(&mut self, pc: usize) -> usize {
        let i = self.size;
        self.dense[i].pc = pc;
        self.sparse[pc] = i;
        self.size += 1;
        i
    }

    fn thread(&mut self, i: usize) -> &mut Thread {
        &mut self.dense[i]
    }

    fn contains(&self, pc: usize) -> bool {
        let s = self.sparse[pc];
        s < self.size && self.dense[s].pc == pc
    }

    fn clear(&mut self) {
        self.size = 0;
    }

    fn pc(&self, i: usize) -> usize {
        self.dense[i].pc
    }

    fn caps(&mut self, i: usize) -> &mut [Option<usize>] {
        &mut self.dense[i].caps
    }
}
