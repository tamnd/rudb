//! The match, which is `find` and the dissect functions of `regexec.c`.
//!
//! The whole match comes first: the leftmost place a match begins, and from there the longest end,
//! or the shortest where the pattern as a whole prefers short. Then the dissect step walks the tree
//! and splits the match between the parts of each node, asking each part's program how far it can
//! reach. A concatenation gives its left part the longest piece after which the right part still
//! reaches the end, or the shortest when the left part prefers short. An iteration splits its piece
//! into as few long iterations as reach the end. A capture records the piece it was given.
//!
//! The programs are asked one question, how far a match from a position can end, and the answer
//! comes from stepping the set of instructions a match could be at, the way a DFA would, without
//! building the DFA. The constraints look at the whole text and not at the piece, which is what
//! PostgreSQL's DFA does by reading one character past the end of the piece.

use rudb_common::Result;

use super::tree::{Node, Op, SHORTER};
use crate::compile::{self, Inst, Program};
use crate::vm::{holds, look};

/// A node of the tree with its program built.
#[derive(Debug)]
struct Sub {
    op: Op,
    flags: u8,
    capno: usize,
    children: Vec<Sub>,
    program: Program,
}

/// A compiled pattern of PostgreSQL's flavour.
#[derive(Debug)]
pub(crate) struct Tree {
    root: Sub,
    /// How many groups the pattern captures, not counting the whole match.
    pub(crate) groups: usize,
}

impl Tree {
    /// The program of the whole pattern, with no groups in it.
    pub(crate) fn program(&self) -> &Program {
        &self.root.program
    }
}

pub(super) fn build(node: &Node, groups: usize) -> Result<Tree> {
    Ok(Tree { root: sub(node)?, groups })
}

fn sub(node: &Node) -> Result<Sub> {
    Ok(Sub {
        op: node.op,
        flags: node.flags,
        capno: node.capno,
        children: node.children.iter().map(sub).collect::<Result<_>>()?,
        program: compile::compile(&node.ast, 0)?,
    })
}

/// The first match at or after `start`, as capture slots: the whole match and then two per group.
pub(crate) fn find(tree: &Tree, text: &str, start: usize) -> Option<Vec<Option<usize>>> {
    let mut slots = Vec::new();
    if !crate::run(&tree.root.program, text, start, false, &mut slots) {
        return None;
    }
    let begin = slots.first().copied().flatten()?;
    let mut machine = Machine::new(text);
    let end = if tree.root.flags & SHORTER != 0 {
        machine.shortest(&tree.root.program, begin, begin, text.len())
    } else {
        machine.longest(&tree.root.program, begin, text.len())
    }?;
    let mut found = vec![None; 2 * (tree.groups + 1)];
    found[0] = Some(begin);
    found[1] = Some(end);
    machine.dissect(&tree.root, begin, end, &mut found);
    Some(found)
}

/// The set simulation of a program over the text, with the work lists kept between calls.
struct Machine<'t> {
    text: &'t str,
    current: Vec<usize>,
    next: Vec<usize>,
    stack: Vec<usize>,
    seen: Vec<u32>,
    generation: u32,
}

impl<'t> Machine<'t> {
    fn new(text: &'t str) -> Self {
        Self {
            text,
            current: Vec::new(),
            next: Vec::new(),
            stack: Vec::new(),
            seen: Vec::new(),
            generation: 0,
        }
    }

    fn fresh(&mut self, size: usize) {
        if self.seen.len() < size {
            self.seen.resize(size, 0);
        }
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.seen.iter_mut().for_each(|cell| *cell = 0);
            self.generation = 1;
        }
    }

    /// Follows everything that reads nothing from `pc` at `at`, and adds what reads to `next`.
    /// Returns whether `Match` was reached.
    fn follow(&mut self, program: &Program, pc: usize, at: usize) -> bool {
        let mut matched = false;
        self.stack.push(pc);
        while let Some(pc) = self.stack.pop() {
            if self.seen[pc] == self.generation {
                continue;
            }
            self.seen[pc] = self.generation;
            match program.insts[pc] {
                Inst::Jump(to) => self.stack.push(to),
                Inst::Split(one, other) => {
                    self.stack.push(other);
                    self.stack.push(one);
                }
                Inst::Save(_) => self.stack.push(pc + 1),
                Inst::Assert(assertion) => {
                    if holds(assertion, self.text, at) {
                        self.stack.push(pc + 1);
                    }
                }
                Inst::Look(id) => {
                    if look(program, id, self.text, at) {
                        self.stack.push(pc + 1);
                    }
                }
                Inst::Match => matched = true,
                Inst::Char(_) | Inst::Set(_) | Inst::Any(_) => self.next.push(pc),
            }
        }
        matched
    }

    /// Steps the program from `begin`, calling `stop` with each position a match ends at until it
    /// says to stop, and never reading past `limit`.
    fn scan(
        &mut self,
        program: &Program,
        begin: usize,
        limit: usize,
        mut stop: impl FnMut(usize) -> bool,
    ) {
        self.current.clear();
        self.next.clear();
        self.fresh(program.insts.len());
        let mut at = begin;
        if self.follow(program, 0, at) && stop(at) {
            return;
        }
        loop {
            std::mem::swap(&mut self.current, &mut self.next);
            self.next.clear();
            if self.current.is_empty() || at >= limit {
                return;
            }
            let Some(ch) = self.text[at..].chars().next() else { return };
            let after = at + ch.len_utf8();
            self.fresh(program.insts.len());
            let mut matched = false;
            for index in 0..self.current.len() {
                let pc = self.current[index];
                let reads = match program.insts[pc] {
                    Inst::Char(want) => ch == want,
                    Inst::Set(id) => program.sets[id].contains(ch),
                    Inst::Any(newline) => newline || ch != '\n',
                    _ => false,
                };
                if reads {
                    matched |= self.follow(program, pc + 1, after);
                }
            }
            at = after;
            if matched && stop(at) {
                return;
            }
        }
    }

    /// `longest`: the furthest a match from `begin` can end, at `limit` or before.
    fn longest(&mut self, program: &Program, begin: usize, limit: usize) -> Option<usize> {
        let mut end = None;
        self.scan(program, begin, limit, |at| {
            end = Some(at);
            false
        });
        end
    }

    /// `shortest`: the nearest a match from `begin` can end, at `least` or after and at `limit`
    /// or before.
    fn shortest(
        &mut self,
        program: &Program,
        begin: usize,
        least: usize,
        limit: usize,
    ) -> Option<usize> {
        let mut end = None;
        self.scan(program, begin, limit, |at| {
            if at >= least {
                end = Some(at);
                return true;
            }
            false
        });
        end
    }

    fn before(&self, at: usize) -> usize {
        self.text[..at].chars().next_back().map_or(at, |ch| at - ch.len_utf8())
    }

    fn after(&self, at: usize) -> usize {
        self.text[at..].chars().next().map_or(at, |ch| at + ch.len_utf8())
    }

    fn count(&self, from: usize, to: usize) -> usize {
        self.text[from..to].chars().count()
    }

    /// `cdissect`. Without back references a node whose program matched the piece always
    /// dissects, so there is no failure to report and the groups are written as they are found.
    fn dissect(&mut self, sub: &Sub, begin: usize, end: usize, slots: &mut [Option<usize>]) {
        match sub.op {
            Op::Leaf => {}
            Op::Concat => {
                if sub.children[0].flags & SHORTER != 0 {
                    self.concat_shortest(sub, begin, end, slots);
                } else {
                    self.concat_longest(sub, begin, end, slots);
                }
            }
            Op::Alt => self.alternation(sub, begin, end, slots),
            Op::Iter { min, max } => {
                if sub.children[0].flags & SHORTER != 0 {
                    self.iterate_shortest(sub, min, max, begin, end, slots);
                } else {
                    self.iterate_longest(sub, min, max, begin, end, slots);
                }
            }
            Op::Capture => self.dissect(&sub.children[0], begin, end, slots),
        }
        if sub.capno > 0 {
            slots[2 * sub.capno] = Some(begin);
            slots[2 * sub.capno + 1] = Some(end);
        }
    }

    /// `ccondissect`: the longest left part after which the right part reaches the end.
    fn concat_longest(&mut self, sub: &Sub, begin: usize, end: usize, slots: &mut [Option<usize>]) {
        let (left, right) = (&sub.children[0], &sub.children[1]);
        let Some(mut mid) = self.longest(&left.program, begin, end) else { return };
        loop {
            if self.longest(&right.program, mid, end) == Some(end) {
                self.dissect(left, begin, mid, slots);
                self.dissect(right, mid, end, slots);
                return;
            }
            if mid == begin {
                return;
            }
            let limit = self.before(mid);
            let Some(next) = self.longest(&left.program, begin, limit) else { return };
            mid = next;
        }
    }

    /// `crevcondissect`: the shortest left part after which the right part reaches the end.
    fn concat_shortest(
        &mut self,
        sub: &Sub,
        begin: usize,
        end: usize,
        slots: &mut [Option<usize>],
    ) {
        let (left, right) = (&sub.children[0], &sub.children[1]);
        let Some(mut mid) = self.shortest(&left.program, begin, begin, end) else { return };
        loop {
            if self.longest(&right.program, mid, end) == Some(end) {
                self.dissect(left, begin, mid, slots);
                self.dissect(right, mid, end, slots);
                return;
            }
            if mid == end {
                return;
            }
            let least = self.after(mid);
            let Some(next) = self.shortest(&left.program, begin, least, end) else { return };
            mid = next;
        }
    }

    /// `caltdissect`: the first branch that matches the whole piece.
    fn alternation(&mut self, sub: &Sub, begin: usize, end: usize, slots: &mut [Option<usize>]) {
        for branch in &sub.children {
            if self.longest(&branch.program, begin, end) == Some(end) {
                self.dissect(branch, begin, end, slots);
                return;
            }
        }
    }

    /// `citerdissect`: the piece split into iterations, each as long as it can be while the rest
    /// still splits, with an empty iteration only where the minimum needs one.
    fn iterate_longest(
        &mut self,
        sub: &Sub,
        min: u32,
        max: Option<u32>,
        begin: usize,
        end: usize,
        slots: &mut [Option<usize>],
    ) {
        let child = &sub.children[0];
        let min_matches = (min as usize).max(1);
        let length = self.count(begin, end);
        let mut max_matches = match max {
            Some(max) => length.min(max as usize),
            None => length,
        };
        max_matches = max_matches.max(min_matches);
        let mut endpts = vec![begin; max_matches + 1];
        let mut k = 1;
        let mut limit = end;
        'outer: while k > 0 {
            let found = self.longest(&child.program, endpts[k - 1], limit);
            let mut shorten = found.is_none();
            if let Some(found) = found {
                endpts[k] = found;
                if found != end {
                    if k >= max_matches {
                        shorten = true;
                    } else if found == endpts[k - 1]
                        && (k >= min_matches || min_matches - k < self.count(found, end))
                    {
                        // A zero length iteration is rejected unless the minimum needs it, and
                        // the same iteration is tried shorter.
                    } else {
                        k += 1;
                        limit = end;
                        continue;
                    }
                } else if k >= min_matches {
                    // Without back references every iteration verifies, so the last one's groups
                    // are the answer.
                    self.dissect(child, endpts[k - 1], endpts[k], slots);
                    return;
                }
            }
            if shorten {
                k -= 1;
            }
            while k > 0 {
                let previous = endpts[k - 1];
                if endpts[k] > previous {
                    limit = self.before(endpts[k]);
                    if limit > previous
                        || (k < min_matches && min_matches - k >= self.count(previous, end))
                    {
                        continue 'outer;
                    }
                }
                k -= 1;
            }
        }
        // Zero iterations, which is what is left for an empty piece.
    }

    /// `creviterdissect`: the piece split into iterations, each as short as it can be while the
    /// rest still splits.
    fn iterate_shortest(
        &mut self,
        sub: &Sub,
        min: u32,
        max: Option<u32>,
        begin: usize,
        end: usize,
        slots: &mut [Option<usize>],
    ) {
        let child = &sub.children[0];
        if min == 0 && begin == end {
            return;
        }
        let min_matches = (min as usize).max(1);
        let length = self.count(begin, end);
        let mut max_matches = match max {
            Some(max) => length.min(max as usize),
            None => length,
        };
        max_matches = max_matches.max(min_matches);
        let mut endpts = vec![begin; max_matches + 1];
        let mut k = 1;
        let mut limit = begin;
        'outer: while k > 0 {
            if limit == endpts[k - 1]
                && limit != end
                && (k >= min_matches || min_matches - k < self.count(limit, end))
            {
                limit = self.after(limit);
            }
            if k >= max_matches {
                limit = end;
            }
            let found = self.shortest(&child.program, endpts[k - 1], limit, end);
            let mut lengthen = found.is_none();
            if let Some(found) = found {
                endpts[k] = found;
                if found != end {
                    if k >= max_matches {
                        lengthen = true;
                    } else {
                        k += 1;
                        limit = endpts[k - 1];
                        continue;
                    }
                } else if k >= min_matches {
                    self.dissect(child, endpts[k - 1], endpts[k], slots);
                    return;
                }
            }
            if lengthen {
                k -= 1;
            }
            while k > 0 {
                if endpts[k] < end {
                    limit = self.after(endpts[k]);
                    continue 'outer;
                }
                k -= 1;
            }
        }
    }
}
