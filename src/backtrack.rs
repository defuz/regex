// Copyright 2014-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// This is the backtracking matching engine. It has the same exact capability
// as the full NFA simulation, except it is artificially restricted to small
// regexes on small inputs because of its memory requirements.
//
// In particular, this is a *bounded* backtracking engine. It retains worst
// case linear time by keeping track of the states that is has visited (using a
// bitmap). Namely, once a state is visited, it is never visited again. Since a
// state is keyed by `(instruction index, input index)`, we have that its time
// complexity is `O(mn)`.
//
// The backtracking engine can beat out the NFA simulation on small
// regexes/inputs because it doesn't have to keep track of multiple copies of
// the capture groups. In benchmarks, the backtracking engine is roughly twice
// as fast as the full NFA simulation.

use input::{Input, ByteInput};
use inst::InstIdx;
use program::Program;
use re::CaptureIdxs;

type Bits = u32;
const BIT_SIZE: usize = 32;
const MAX_PROG_SIZE: usize = 100;
const MAX_INPUT_SIZE: usize = 256 * (1 << 10);

// Total memory usage in bytes is determined by:
//
//   ((len(insts) * (len(input) + 1) + bits - 1) / bits) / (bits / 8)
//
// With the above settings, this comes out to ~3.2MB. Mostly these numbers
// were picked empirically with suspicious benchmarks.

/// A backtracking matching engine.
#[derive(Debug)]
pub struct Backtrack<'a, 'r, 't, 'c, I: 't> {
    prog: &'r Program,
    input: I,
    caps: &'c mut CaptureIdxs,
    m: &'a mut BackMachine<I>,
}

/// Shared cached state between multiple invocations of a backtracking engine
/// in the same thread.
///
/// It is exported so that it can be cached by `program::Program`.
#[derive(Debug)]
pub struct BackMachine<I> {
    jobs: Vec<Job<I>>,
    visited: Vec<Bits>,
}

impl<I: Input> BackMachine<I> {
    /// Create new empty state for the backtracking engine.
    pub fn new() -> BackMachine<I> {
        BackMachine {
            jobs: vec![],
            visited: vec![],
        }
    }
}

/// A job is an explicit unit of stack space in the backtracking engine.
///
/// The "normal" representation is a single state transition, which corresponds
/// to an NFA state and a character in the input. However, the backtracking
/// engine must keep track of old capture group values. We use the explicit
/// stack to do it.
#[derive(Clone, Copy, Debug)]
enum Job<I: Input> {
    Inst { pc: InstIdx, at: I::At },
    SaveRestore { slot: usize, old_pos: Option<usize> },
}

impl<'a, 'r, 't, 'c, I: 't + Input> Backtrack<'a, 'r, 't, 'c, I> {
    /// Execute the backtracking matching engine.
    ///
    /// If there's a match, `exec` returns `true` and populates the given
    /// captures accordingly.
    pub fn exec<I: Input>(
        prog: &'r Program,
        mut caps: &mut CaptureIdxs,
        input: I,
        start: usize,
    ) -> bool {
        let start = input.at(start);
        let mut m = prog.backtrack.get();
        let mut b = Backtrack {
            prog: prog,
            input: input,
            caps: caps,
            m: &mut m,
        };
        b.exec_(start)
    }

    /// Returns true iff the given regex and input can be executed by this
    /// engine with reasonable memory usage.
    pub fn should_exec(prog: &'r Program, input: &[u8]) -> bool {
        prog.insts.len() <= MAX_PROG_SIZE && input.len() <= MAX_INPUT_SIZE
    }

    fn clear(&mut self) {
        // Reset the job memory so that we start fresh.
        self.m.jobs.truncate(0);

        // Now we need to clear the bit state set.
        // We do this by figuring out how much space we need to keep track
        // of the states we've visited.
        // Then we reset all existing allocated space to 0.
        // Finally, we request more space if we need it.
        //
        // This is all a little circuitous, but doing this unsafely
        // doesn't seem to have a measurable impact on performance.
        // (Probably because backtracking is limited to such small
        // inputs/regexes in the first place.)
        let visited_len =
            (self.prog.insts.len() * (self.input.len() + 1) + BIT_SIZE - 1)
            /
            BIT_SIZE;
        self.m.visited.truncate(visited_len);
        for v in &mut self.m.visited {
            *v = 0;
        }
        if visited_len > self.m.visited.len() {
            let len = self.m.visited.len();
            self.m.visited.reserve_exact(visited_len - len);
            for _ in 0..(visited_len - len) {
                self.m.visited.push(0);
            }
        }
    }

    fn exec_(&mut self, mut at: I::At) -> bool {
        self.clear();
        if self.prog.anchored_begin && !at.is_beginning() {
            return false;
        }
        /*
        if self.prog.anchored_begin {
            return if at > 0 {
                false
            } else {
                match self.input.prefix_at(&self.prog.prefixes, at) {
                    None => false,
                    Some(at) => self.backtrack(at),
                }
            };
        }
        */
        loop {
            /*
            if !self.prog.prefixes.is_empty() {
                at = match self.input.prefix_at(&self.prog.prefixes, at) {
                    None => return false,
                    Some(at) => at,
                };
            }
            */
            if self.backtrack(at) {
                return true;
            }
            if at >= self.input.len() {
                return false;
            }
            at += 1;
        }
    }

    // This `inline(always)` seems to result in about a 10-15% increase in
    // throughput on the `hard` benchmarks (over a standard `inline`). ---AG
    #[inline(always)]
    fn backtrack(&mut self, start: usize) -> bool {
        self.push(0, start);
        while let Some(job) = self.m.jobs.pop() {
            match job {
                Job::Inst { pc, at } => {
                    if self.step(pc, at) {
                        return true;
                    }
                }
                Job::SaveRestore { slot, old_pos } => {
                    self.caps[slot] = old_pos;
                }
            }
        }
        false
    }

    fn step(&mut self, mut pc: InstIdx, mut at: usize) -> bool {
        use inst::Inst::*;
        loop {
            // This loop is an optimization to avoid constantly pushing/popping
            // from the stack. Namely, if we're pushing a job only to run it
            // next, avoid the push and just mutate `pc` (and possibly `at`)
            // in place.
            match self.prog.insts[pc] {
                Match => return true,
                Save(ref inst) => {
                    if inst.slot < self.caps.len() {
                        // If this path doesn't work out, then we save the old
                        // capture index (if one exists) in an alternate
                        // job. If the next path fails, then the alternate
                        // job is popped and the old capture index is restored.
                        let old_pos = self.caps[inst.slot];
                        self.push_save_restore(inst.slot, old_pos);
                        self.caps[inst.slot] = Some(at);
                    }
                    pc = inst.goto;
                }
                Split(ref inst) => {
                    self.push(inst.goto2, at);
                    pc = inst.goto1;
                }
                EmptyLook(ref inst) => {
                    let prev = self.input.previous_char(at);
                    let next = self.input.next_char(at);
                    if inst.matches(prev, next) {
                        pc = inst.goto;
                    } else {
                        return false;
                    }
                }
                Bytes(ref inst) => {
                    if let Some(&b) = self.input.get(at) {
                        if inst.start <= b && b <= inst.end {
                            pc = inst.goto;
                            at += 1;
                            continue;
                        }
                    }
                    return false;
                }
            }
            if self.has_visited(pc, at) {
                return false;
            }
        }
    }

    fn push(&mut self, pc: InstIdx, at: usize) {
        self.m.jobs.push(Job::Inst { pc: pc, at: at });
    }

    fn push_save_restore(&mut self, slot: usize, old_pos: Option<usize>) {
        self.m.jobs.push(Job::SaveRestore { slot: slot, old_pos: old_pos });
    }

    fn has_visited(&mut self, pc: InstIdx, at: usize) -> bool {
        let k = pc * (self.input.len() + 1) + at;
        let k1 = k / BIT_SIZE;
        let k2 = (1 << (k & (BIT_SIZE - 1))) as Bits;
        if self.m.visited[k1] & k2 == 0 {
            self.m.visited[k1] |= k2;
            false
        } else {
            true
        }
    }
}
