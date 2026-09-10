//! Walking the rule table over a token vector, producing a parse tree.
//!
//! This is a PEG matcher and nothing more. It decides where every rule in the grammar started and
//! stopped, and it does not know what any of them mean. Turning the tree into an AST is the
//! transformer's job, and keeping the two apart is what lets the grammar be vendored: a grammar
//! bump changes the table and this file does not move.
//!
//! Three things about it are worth knowing before reading it.
//!
//! It has no Rust stack recursion. A PEG over a grammar with a thousand rules nests as deep as the
//! query does, and `a + (b + (c + ...))` nests as deep as the user cares to type. A recursive
//! matcher blows the thread stack on input that is merely rude rather than adversarial, and it does
//! it with a segfault rather than an error, so the recursion is an explicit `Vec` of frames with a
//! cap on it and the cap reports a parser error like any other.
//!
//! Failure does not truncate the arena. A choice that tries thirty alternatives builds and
//! abandons tree nodes for twenty nine of them, and the obvious cleanup is to roll the arena back
//! to where the alternative started. That is wrong here, because a memoized rule that succeeded
//! inside a failed alternative keeps its memo entry, and the entry points at nodes in the arena. So
//! abandoned nodes stay, unreferenced, and the arena is a bump allocator that is freed all at once.
//! For a query that parses, the waste is small; for one that does not, it does not matter.
//!
//! The FIRST filter is a superset test and only its negative answer is used. `Statement` is a
//! choice of thirty six alternatives and upstream descends into each one far enough to fail. Here
//! an alternative whose FIRST set does not contain the token in hand is skipped on one AND. A
//! nullable node is never skipped, because it can match without looking at the token at all, which
//! is why the guard tests the nullable bit before it tests the set. Both live in the node, so the
//! guard and the work it guards read the same twenty four bytes.
//!
//! `spec/20-the-grammar.md` sections 3, 5 and 6.

use rudb_common::{Error, Result};

use crate::generated::keywords::{KEYWORDS, UNRESERVED};
use crate::generated::rules::{CHILDREN, NODES, PROGRAM, RULES, SYMBOLS};
use crate::rules::{Node, Op, Suggestion};
use crate::token::{Flags, Kind, Token};
use crate::tokenize::tokenize;

/// No node.
///
/// `u32::MAX` rather than an `Option<u32>`, so that a `ParseNode` is twenty bytes and a tree of a
/// hundred thousand nodes is two megabytes rather than four.
pub const NONE: u32 = u32::MAX;

/// How deep the frame stack may go before the parse is called a runaway.
///
/// Two hundred and sixty two thousand frames is far past anything a person writes and far short of
/// anything that takes noticeable time or memory to reach. It exists because a PEG has no other
/// bound: `(((((...)))))` nests one frame per paren and the grammar is happy to keep going. The
/// number is a power of two for no reason other than that a round one invites being tuned.
const MAX_DEPTH: usize = 262_144;

/// An empty memo slot, meaning this rule has not been tried at this position.
const MEMO_EMPTY: u32 = u32::MAX;
/// A memo slot holding a failure, meaning this rule was tried here and did not match.
const MEMO_FAILED: u32 = u32::MAX - 1;

/// One node of the parse tree. Twenty bytes.
///
/// Children are a linked list rather than a slice, because a node's children are discovered one at
/// a time and interleaved with the children of every other node being built at the same moment, so
/// a contiguous list would need either a second pass or a per node vector. The list is built in
/// order and read in order, which is the only access pattern the transformer has.
///
/// Terminals get no node. A keyword, a symbol and a literal are all recoverable from the token
/// span of the rule that contains them, and giving each one a node would roughly triple the tree
/// for information that is already in the token vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseNode {
    /// Which rule this is, as an index into `RULES`.
    pub rule: u32,
    /// The first token it covers.
    pub start: u32,
    /// One past the last token it covers.
    pub end: u32,
    /// Its first child, or `NONE`.
    pub first_child: u32,
    /// The next child of this node's parent, or `NONE`.
    pub next_sibling: u32,
}

/// A parsed query.
#[derive(Debug, Clone)]
pub struct Tree {
    nodes: Vec<ParseNode>,
    root: u32,
    steps: u64,
}

impl Tree {
    /// The root node, which is the rule the parse was started from.
    pub fn root(&self) -> u32 {
        self.root
    }

    /// How many nodes the tree has, abandoned ones included.
    ///
    /// Not the size of the tree that is reachable from the root. It is the size of the arena, which
    /// is what the parse cost, and telling the two apart is what the ratio between them is for.
    pub fn arena_len(&self) -> usize {
        self.nodes.len()
    }

    /// How many nodes of the rule table the matcher went into to produce this.
    ///
    /// The one number that says what a parse cost, and the one to watch when the grammar or the
    /// filter changes. A parse that is linear in the query does a roughly constant number of these
    /// per token; one that is backtracking badly does thousands.
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// One node.
    pub fn node(&self, index: u32) -> ParseNode {
        self.nodes[index as usize]
    }

    /// The name of the rule a node is.
    pub fn name(&self, index: u32) -> &'static str {
        RULES[self.node(index).rule as usize].name
    }

    /// The children of a node, in order.
    pub fn children(&self, index: u32) -> Children<'_> {
        Children { tree: self, next: self.node(index).first_child }
    }

    /// The text a node covers, given the query and its tokens.
    ///
    /// A node that covers no tokens, which is any rule whose body matched nothing, gets the empty
    /// string at the point it started rather than a span running backwards.
    pub fn text<'a>(&self, index: u32, query: &'a str, tokens: &[Token]) -> &'a str {
        let node = self.node(index);
        if node.end <= node.start {
            let at = tokens.get(node.start as usize).map_or(query.len(), |t| t.start as usize);
            return &query[at..at];
        }
        let start = tokens[node.start as usize].start as usize;
        let end = tokens[node.end as usize - 1].end as usize;
        &query[start..end]
    }
}

/// The children of one node.
#[derive(Debug)]
pub struct Children<'a> {
    tree: &'a Tree,
    next: u32,
}

impl Iterator for Children<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.next == NONE {
            return None;
        }
        let current = self.next;
        self.next = self.tree.node(current).next_sibling;
        Some(current)
    }
}

/// Parse a whole script.
pub fn parse(query: &str) -> Result<Tree> {
    let tokens = tokenize(query)?;
    parse_tokens(query, &tokens, PROGRAM, true)
}

/// Parse from a named rule, for tests and for the differential harness.
///
/// `filter` off runs the same walk with the FIRST filter disabled, which is how the harness checks
/// that the filter is the superset it claims to be: the two modes have to accept the same queries
/// and build the same trees, and if they ever do not, the filter is wrong and not the grammar.
pub fn parse_from(query: &str, rule_name: &str, filter: bool) -> Result<Tree> {
    let index = RULES
        .binary_search_by(|candidate| candidate.name.cmp(rule_name))
        .map_err(|_| Error::parser(format!("no rule named {rule_name}")))?;
    let tokens = tokenize(query)?;
    parse_tokens(query, &tokens, index as u32, filter)
}

/// Parse tokens that have already been produced.
pub fn parse_tokens(query: &str, tokens: &[Token], root: u32, filter: bool) -> Result<Tree> {
    Matcher::new(query, tokens, filter).run(root)
}

/// Which frame this is, decided once when it is pushed rather than read back off the node.
///
/// The five composite ops are the five kinds of frame. Terminals never get one, because they match
/// or they do not and there is nothing to come back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameOp {
    Rule,
    Sequence,
    Choice,
    Optional,
    Repeat,
}

/// One suspended node.
///
/// `a` and `b` mean what they mean on the node this came from: the rule index for a rule, the child
/// node for an optional or a repeat, and the start and length of the child list for a sequence or a
/// choice. Copying them in is what keeps the loop from touching `NODES` on the way back up.
#[derive(Debug, Clone, Copy)]
struct Frame {
    op: FrameOp,
    a: u32,
    b: u32,
    /// Where the token position was on entry, which is where a failure puts it back.
    start: u32,
    /// Which child a sequence or a choice is on, or how many times a repeat has gone round.
    step: u32,
    /// Where a repeat's last successful iteration ended.
    mark: u32,
    /// The children collected so far, as a list.
    head: u32,
    tail: u32,
}

/// What the loop does next.
enum Action {
    /// Go into this node.
    Enter(u32),
    /// The thing that just ran matched, and contributed this list of children.
    Succeed(u32, u32),
    /// The thing that just ran did not match.
    Fail,
    /// The stack is empty. `Some` is the root's node, `None` is a parse that failed.
    Done(Option<u32>),
}

struct Matcher<'a> {
    query: &'a str,
    tokens: &'a [Token],
    /// One FIRST key per token, computed once. The filter asks for the key of the token at the
    /// current position on every node it enters, and the same token is entered on many times.
    keys: Vec<u64>,
    arena: Vec<ParseNode>,
    stack: Vec<Frame>,
    /// One slot per memoized rule per token position, holding an arena index, `MEMO_FAILED` or
    /// `MEMO_EMPTY`. A flat array rather than a map: twenty two rules against the token count is a
    /// few tens of kilobytes for a normal query, and the lookup is an index rather than a hash.
    memo: Vec<u32>,
    /// Which memo row a rule uses, or `NONE`.
    slot_of: &'static [u32],
    filter: bool,
    pos: u32,
    /// How many nodes have been entered. Diagnostic only, and free next to the work it counts.
    steps: u64,
    /// The furthest token any terminal was tried at, which is where the error goes. The furthest
    /// failure is what a person reads as the place the query went wrong, and the place the matcher
    /// finally gives up is usually the start of the statement.
    furthest: u32,
}

/// The memo row each rule uses, built once for the process.
///
/// Twenty two rules memoize, out of one thousand and eighty eight, so a row per rule would be a
/// table forty nine times bigger than it needs to be and the memo is sized per token on top of
/// that.
fn slots() -> &'static (Box<[u32]>, usize) {
    use std::sync::OnceLock;
    static SLOTS: OnceLock<(Box<[u32]>, usize)> = OnceLock::new();
    SLOTS.get_or_init(build_slots)
}

fn build_slots() -> (Box<[u32]>, usize) {
    let mut slots = vec![NONE; RULES.len()];
    let mut next = 0;
    for (index, rule) in RULES.iter().enumerate() {
        if rule.memoized {
            slots[index] = next;
            next += 1;
        }
    }
    (slots.into_boxed_slice(), next as usize)
}

impl<'a> Matcher<'a> {
    fn new(query: &'a str, tokens: &'a [Token], filter: bool) -> Self {
        let keys = tokens.iter().map(|token| crate::rules::token_key(*token)).collect();
        // One row per memoized rule, one column per token plus one for the position past the end.
        let memo = vec![MEMO_EMPTY; slots().1 * (tokens.len() + 1)];
        Self {
            query,
            tokens,
            keys,
            // The arena grows as the tree does. A guess here saves a handful of reallocations on
            // anything but the smallest query, and a token is worth about a node in practice.
            arena: Vec::with_capacity(tokens.len()),
            stack: Vec::with_capacity(64),
            memo,
            slot_of: &slots().0,
            filter,
            pos: 0,
            steps: 0,
            furthest: 0,
        }
    }

    fn run(mut self, root: u32) -> Result<Tree> {
        self.push(Frame {
            op: FrameOp::Rule,
            a: root,
            b: 0,
            start: 0,
            step: 0,
            mark: 0,
            head: NONE,
            tail: NONE,
        })?;

        let mut action = Action::Enter(RULES[root as usize].root);
        let node = loop {
            action = match action {
                Action::Enter(node) => self.enter(node)?,
                Action::Succeed(head, tail) => self.settle_ok(head, tail),
                Action::Fail => self.settle_fail(),
                Action::Done(result) => match result {
                    Some(node) => break node,
                    None => return Err(self.syntax_error(self.furthest)),
                },
            };
        };

        // Everything has to be consumed. `Program <- TopLevelStatement*` stops at the first token
        // it cannot start a statement with and calls that a successful parse of the part it read,
        // so without this `SELECT 1 rubbish here` parses as `SELECT 1` and the rest is silently
        // dropped. The token vector always ends with an end of input token, so a parse that
        // reached the end is at `len`, and one that stopped short is pointing at the offender.
        if (self.pos as usize) < self.tokens.len()
            && self.tokens[self.pos as usize].kind != Kind::EndOfInput
        {
            return Err(self.syntax_error(self.pos.max(self.furthest)));
        }

        Ok(Tree { nodes: self.arena, root: node, steps: self.steps })
    }

    /// The token at a position, or the end of input past the end.
    ///
    /// Only the FIRST filter asks past the end. The terminals all check the bound themselves,
    /// because `EndOfInputMatcher` advancing over a synthetic token would let
    /// `TopLevelStatement <- Statement? (';'+ / EndOfInput)` match forever at the end of a script.
    fn token(&self, pos: u32) -> Token {
        self.tokens.get(pos as usize).copied().unwrap_or(Token {
            kind: Kind::EndOfInput,
            flags: Flags::default(),
            keyword: crate::token::NOT_A_KEYWORD,
            start: self.query.len() as u32,
            end: self.query.len() as u32,
        })
    }

    fn key(&self, pos: u32) -> u64 {
        self.keys.get(pos as usize).copied().unwrap_or(crate::rules::FIRST_END)
    }

    fn push(&mut self, frame: Frame) -> Result<()> {
        if self.stack.len() >= MAX_DEPTH {
            let token = self.token(self.pos);
            return Err(Error::parser(format!(
                "memory exhausted at or near \"{}\"",
                token.text(self.query)
            ))
            .with_span(token.span()));
        }
        self.stack.push(frame);
        Ok(())
    }

    fn alloc(&mut self, node: ParseNode) -> u32 {
        self.arena.push(node);
        (self.arena.len() - 1) as u32
    }

    /// Record that something was tried here, for the error message.
    fn reached(&mut self, pos: u32) {
        if pos > self.furthest {
            self.furthest = pos;
        }
    }

    fn syntax_error(&self, pos: u32) -> Error {
        let token = self.token(pos);
        if token.kind == Kind::EndOfInput {
            return Error::parser("syntax error at end of input").with_span(token.span());
        }
        Error::parser(format!("syntax error at or near \"{}\"", token.text(self.query)))
            .with_span(token.span())
    }

    /// Handle one node.
    fn enter(&mut self, index: u32) -> Result<Action> {
        self.steps += 1;
        let node = NODES[index as usize];
        // The superset test, and only its no. A nullable node can match without reading a token at
        // all, so its FIRST set says nothing about whether it applies and asking would reject the
        // empty match that is the whole point of it.
        if self.filter && !node.can_start(self.key(self.pos)) {
            self.reached(self.pos);
            return Ok(Action::Fail);
        }

        match node.op {
            Op::Rule => self.enter_rule(node.a, node.b),
            Op::Sequence => {
                self.push(self.frame(FrameOp::Sequence, node.a, node.b))?;
                Ok(Action::Enter(CHILDREN[node.a as usize]))
            }
            Op::Choice => {
                let step = self.viable(node.a, node.b, 0);
                if step == node.b {
                    self.reached(self.pos);
                    return Ok(Action::Fail);
                }
                let mut frame = self.frame(FrameOp::Choice, node.a, node.b);
                frame.step = step;
                self.push(frame)?;
                Ok(Action::Enter(CHILDREN[(node.a + step) as usize]))
            }
            Op::Optional => {
                self.push(self.frame(FrameOp::Optional, node.a, 0))?;
                Ok(Action::Enter(node.a))
            }
            Op::Repeat => {
                self.push(self.frame(FrameOp::Repeat, node.a, 0))?;
                Ok(Action::Enter(node.a))
            }
            _ => Ok(self.terminal(node)),
        }
    }

    /// The first alternative at or after `step` that could match the token in hand.
    ///
    /// A choice used to enter every alternative in turn and let the guard at the top of `enter`
    /// reject it, and `Statement` has thirty six of them. That costs a step, a stack push and a
    /// stack pop per rejection, for a test that is a load and an AND. Doing the test here means an
    /// alternative that cannot match never becomes a step at all, which is why the step counts in
    /// the bench moved and not only the times.
    fn viable(&self, a: u32, b: u32, mut step: u32) -> u32 {
        if !self.filter {
            return step;
        }
        let key = self.key(self.pos);
        while step < b && !NODES[CHILDREN[(a + step) as usize] as usize].can_start(key) {
            step += 1;
        }
        step
    }

    fn frame(&self, op: FrameOp, a: u32, b: u32) -> Frame {
        Frame { op, a, b, start: self.pos, step: 0, mark: self.pos, head: NONE, tail: NONE }
    }

    /// A reference to a rule, which is the only thing that makes a tree node.
    fn enter_rule(&mut self, rule: u32, root: u32) -> Result<Action> {
        let slot = self.slot_of[rule as usize];
        if slot != NONE {
            match self.memo[self.memo_index(slot)] {
                MEMO_EMPTY => {}
                MEMO_FAILED => return Ok(Action::Fail),
                stored => {
                    // The stored node is shared by every parent that adopts it, and `next_sibling`
                    // is written by whichever one that is, so the node itself is copied and only
                    // its children are shared. The children are safe to share because nothing ever
                    // rewrites a link inside a finished list, only the link out of its head.
                    let source = self.arena[stored as usize];
                    self.pos = source.end;
                    let copy = self.alloc(ParseNode { next_sibling: NONE, ..source });
                    return Ok(Action::Succeed(copy, copy));
                }
            }
        }
        self.push(self.frame(FrameOp::Rule, rule, 0))?;
        Ok(Action::Enter(root))
    }

    fn memo_index(&self, slot: u32) -> usize {
        slot as usize * (self.tokens.len() + 1) + self.pos as usize
    }

    /// Something matched. Give its children to the frame above and decide what that frame does now.
    fn settle_ok(&mut self, head: u32, tail: u32) -> Action {
        // Popped rather than looked at, and pushed back by the two cases that carry on. A frame is
        // thirty two bytes of `Copy`, so this is a couple of moves, and the alternative is holding
        // a mutable borrow of the stack across every write to the arena.
        let Some(mut frame) = self.stack.pop() else {
            return Action::Done(Some(head));
        };

        if head != NONE {
            if frame.head == NONE {
                frame.head = head;
            } else {
                self.arena[frame.tail as usize].next_sibling = head;
            }
            frame.tail = tail;
        }

        match frame.op {
            FrameOp::Rule => {
                let node = self.alloc(ParseNode {
                    rule: frame.a,
                    start: frame.start,
                    end: self.pos,
                    first_child: frame.head,
                    next_sibling: NONE,
                });
                self.remember(frame.a, frame.start, node);
                Action::Succeed(node, node)
            }
            FrameOp::Sequence => {
                frame.step += 1;
                if frame.step == frame.b {
                    Action::Succeed(frame.head, frame.tail)
                } else {
                    let next = CHILDREN[(frame.a + frame.step) as usize];
                    self.stack.push(frame);
                    Action::Enter(next)
                }
            }
            FrameOp::Choice | FrameOp::Optional => Action::Succeed(frame.head, frame.tail),
            FrameOp::Repeat => {
                // A repeat wraps something that cannot match nothing, which the generator checks
                // and `a_repeat_never_wraps_something_that_matches_nothing` asserts, so this always
                // moves. The guard is here because the alternative to a wrong answer would be a
                // hang, and a hang in a parser is the failure nobody can diagnose from a bug
                // report.
                debug_assert!(self.pos != frame.mark, "a repeat went round without consuming");
                if self.pos == frame.mark {
                    return Action::Succeed(frame.head, frame.tail);
                }
                frame.mark = self.pos;
                frame.step += 1;
                let child = frame.a;
                self.stack.push(frame);
                Action::Enter(child)
            }
        }
    }

    /// Something did not match. Put the position back and decide what the frame above does now.
    fn settle_fail(&mut self) -> Action {
        let Some(mut frame) = self.stack.pop() else {
            return Action::Done(None);
        };

        match frame.op {
            FrameOp::Rule => {
                self.pos = frame.start;
                // A failure is worth remembering for the same reason a success is. The rules that
                // memoize are the ones an expression re-enters at the same position from every
                // alternative in turn, and most of those re-entries fail.
                self.remember(frame.a, frame.start, MEMO_FAILED);
                Action::Fail
            }
            FrameOp::Sequence => {
                self.pos = frame.start;
                Action::Fail
            }
            FrameOp::Choice => {
                self.pos = frame.start;
                frame.step = self.viable(frame.a, frame.b, frame.step + 1);
                if frame.step == frame.b {
                    Action::Fail
                } else {
                    // The children of a failed alternative are dropped by not being spliced. The
                    // nodes stay in the arena, unreferenced, which is the trade this file's header
                    // is about.
                    frame.head = NONE;
                    frame.tail = NONE;
                    let next = CHILDREN[(frame.a + frame.step) as usize];
                    self.stack.push(frame);
                    Action::Enter(next)
                }
            }
            FrameOp::Optional => {
                self.pos = frame.start;
                Action::Succeed(NONE, NONE)
            }
            FrameOp::Repeat => {
                self.pos = frame.mark;
                if frame.step == 0 { Action::Fail } else { Action::Succeed(frame.head, frame.tail) }
            }
        }
    }

    /// Write a memo entry, if this rule is one of the twenty two that get one.
    fn remember(&mut self, rule: u32, start: u32, entry: u32) {
        let slot = self.slot_of[rule as usize];
        if slot != NONE {
            let index = slot as usize * (self.tokens.len() + 1) + start as usize;
            self.memo[index] = entry;
        }
    }

    /// A node that matches tokens directly, or does not.
    fn terminal(&mut self, node: Node) -> Action {
        self.reached(self.pos);
        if self.pos as usize >= self.tokens.len() {
            return Action::Fail;
        }
        let token = self.tokens[self.pos as usize];
        let matched = match node.op {
            // An index compare, not a text compare. The tokenizer already folded the word and
            // looked it up, and everything that is not a word carries `NOT_A_KEYWORD`, which is
            // larger than any index, so the compare rejects them without asking what they are.
            Op::Keyword => u32::from(token.keyword) == node.a,
            Op::KeywordClass => {
                token.kind == Kind::Keyword && u32::from(class_of(token)) & node.a != 0
            }
            // A text compare and nothing else, which is upstream's, and it matters. A `.` between
            // two names arrives as a number token, because the tokenizer cannot tell `a.b` from
            // `.5` until it has read past the dot, so a check that the token is an operator would
            // make `DottedIdentifier` unmatchable. Nothing is lost by dropping it: every symbol is
            // punctuation, no word or literal has punctuation for its whole text, and a quoted or
            // string token carries its quotes in its text and so cannot collide either.
            Op::Symbol => token.text(self.query) == SYMBOLS[node.a as usize],
            // The other half of the same fact. Upstream rejects a lone dot here, and this is why:
            // without it `a.b` would parse `.` as a numeric literal and `SELECT a.b` would come
            // out as three expressions rather than one qualified name.
            Op::Number => token.kind == Kind::Number && token.text(self.query) != ".",
            Op::Operator => {
                token.kind == Kind::Operator && is_bare_operator(token.text(self.query))
            }
            Op::EndOfInput => token.kind == Kind::EndOfInput,
            Op::String => return self.string(token),
            Op::Identifier => self.identifier(token, node),
            other => unreachable!("{other:?} is a composite and never reaches here"),
        };
        if matched {
            self.pos += 1;
            Action::Succeed(NONE, NONE)
        } else {
            Action::Fail
        }
    }

    /// A string literal and the literals that continue it.
    ///
    /// `'a'` on one line and `'b'` on the next is one string in SQL, and the rule for when it is
    /// comes from PostgreSQL: the pieces have to be plain single quoted literals, there has to be a
    /// line break between them, and a block comment in the gap stops the run. `'a' 'b'` on one line
    /// is not a continuation and neither is `E'a'` followed by anything, so a prefixed or dollar
    /// quoted literal matches alone.
    fn string(&mut self, token: Token) -> Action {
        if token.kind != Kind::String {
            return Action::Fail;
        }
        self.pos += 1;
        if !is_plain_string(token.text(self.query)) {
            return Action::Succeed(NONE, NONE);
        }
        while let Some(next) = self.tokens.get(self.pos as usize) {
            if next.kind != Kind::String
                || !next.flags.has(Flags::NEWLINE)
                || next.flags.has(Flags::BLOCK_COMMENT)
                || !is_plain_string(next.text(self.query))
            {
                break;
            }
            self.pos += 1;
        }
        Action::Succeed(NONE, NONE)
    }

    /// A name, in whichever of the eleven positions the grammar is at.
    ///
    /// Two questions, in upstream's order. Is this the shape of a name at all, and if it is a
    /// keyword, is this a position that lets that keyword through. The second is where the keyword
    /// classes earn their existence: `SELECT * FROM binary` is an error and `SELECT binary(x)` is
    /// not, and the only difference between them is which suggestion the matcher was built with.
    fn identifier(&mut self, token: Token, node: Node) -> bool {
        let suggestion = SUGGESTIONS[node.a as usize];
        let shaped = match token.kind {
            Kind::QuotedIdentifier => true,
            Kind::Identifier | Kind::Keyword => true,
            // `FROM 'file.parquet'` and `COPY t TO 'out.csv'`, and nowhere else. Anywhere else a
            // single quoted string has to stay a string, or `SELECT 'x' FROM t` becomes a column.
            Kind::String => {
                suggestion.supports_string_literal() && is_plain_string(token.text(self.query))
            }
            _ => false,
        };
        if !shaped {
            return false;
        }
        // The whole of `ReservedIdentifierMatcher`, which is what the rule named `ReservedKeyword`
        // is overridden with. It skips the class check entirely, so it takes any word at all rather
        // than the seventy five reserved ones. See `Node::RESERVED`.
        if node.flags & Node::RESERVED != 0 {
            return true;
        }
        if token.kind != Kind::Keyword {
            return true;
        }
        let class = class_of(token);
        class & UNRESERVED != 0 || class & suggestion.allowed_class() != 0
    }
}

/// Which classes a token's word is in.
fn class_of(token: Token) -> u8 {
    KEYWORDS[token.keyword as usize].1
}

/// Whether a string literal is the plain single quoted kind.
///
/// Prefixed forms (`E'a'`, `x'ff'`) and dollar quoting start with something else, and the two
/// places this is asked both care about the same distinction.
fn is_plain_string(text: &str) -> bool {
    text.starts_with('\'')
}

/// The characters `OperatorMatcher` will accept a token made entirely of.
const OPERATOR_CHARACTERS: &[u8] = b"+-*/%^<>=~!@&|";

/// The tokens that look like operators and are not, because the grammar spells them itself.
///
/// Upstream lists these out in `OperatorMatcher` and the reason is the same for all of them: a rule
/// somewhere writes the token as a literal and means something specific by it, so letting the
/// generic operator node take it first would make that rule unreachable. `->` is JSON extraction,
/// the comparisons are comparisons, and the tilde family is the pattern matching operators.
const NOT_OPERATORS: [&str; 15] = [
    "->", "->>", "<=", ">=", "!=", "==", "<>", "~~", "~~*", "~~~", "~*", "!~~", "!~~*", "!~", "!~*",
];

/// Whether this text is an operator in the sense the `Operator` node means.
///
/// A single character is never one, which is not an oversight: every single character operator in
/// the language is spelled by a rule, so the generic node is only ever for the multi character ones
/// a user might define.
fn is_bare_operator(text: &str) -> bool {
    if text.len() < 2 || NOT_OPERATORS.contains(&text) {
        return false;
    }
    text.bytes().all(|byte| OPERATOR_CHARACTERS.contains(&byte))
}

/// The eleven suggestions by discriminant, so that a node's `a` can be turned back into one.
///
/// A table rather than a `match`, because the discriminants are dense and written by the generator
/// and the table is checked against them by `the_suggestions_are_dense_and_in_order`.
const SUGGESTIONS: [Suggestion; 11] = [
    Suggestion::Variable,
    Suggestion::CatalogName,
    Suggestion::SchemaName,
    Suggestion::TableName,
    Suggestion::ColumnName,
    Suggestion::ScalarFunctionName,
    Suggestion::TableFunctionName,
    Suggestion::TypeName,
    Suggestion::PragmaName,
    Suggestion::SettingName,
    Suggestion::FileName,
];

#[cfg(test)]
mod tests {
    use super::{
        NONE, SUGGESTIONS, Tree, is_bare_operator, is_plain_string, parse, parse_from, parse_tokens,
    };
    use crate::corpus::CORPUS;
    use crate::generated::rules::PROGRAM;
    use crate::tokenize::tokenize;

    /// The rules a tree has, outermost first, for asserting on shape without writing out the whole
    /// thing.
    fn names(tree: &Tree, node: u32, into: &mut Vec<&'static str>) {
        into.push(tree.name(node));
        for child in tree.children(node) {
            names(tree, child, into);
        }
    }

    /// The first node with this rule name, depth first.
    fn find(tree: &Tree, node: u32, name: &str) -> Option<u32> {
        if tree.name(node) == name {
            return Some(node);
        }
        tree.children(node).find_map(|child| find(tree, child, name))
    }

    fn shape(query: &str) -> Vec<&'static str> {
        let tree = parse(query).expect("parses");
        let mut out = Vec::new();
        names(&tree, tree.root(), &mut out);
        out
    }

    #[test]
    fn the_suggestions_are_dense_and_in_order() {
        for (index, suggestion) in SUGGESTIONS.iter().enumerate() {
            assert_eq!(*suggestion as usize, index);
        }
    }

    #[test]
    fn an_empty_script_parses() {
        let tree = parse("").expect("an empty script is a script with no statements");
        assert_eq!(tree.name(tree.root()), "Program");
    }

    #[test]
    fn a_select_parses_and_the_root_is_the_program() {
        let tree = parse("SELECT 1").expect("parses");
        assert_eq!(tree.name(tree.root()), "Program");
        let statements: Vec<_> = tree.children(tree.root()).collect();
        assert_eq!(statements.len(), 1);
        assert_eq!(tree.name(statements[0]), "TopLevelStatement");
    }

    #[test]
    fn the_shape_has_the_rules_the_grammar_names() {
        let shape = shape("SELECT 1");
        assert!(shape.contains(&"SelectStatement"), "{shape:?}");
    }

    #[test]
    fn a_statement_covers_the_text_it_came_from() {
        let query = "  SELECT 1  ";
        let tokens = tokenize(query).expect("tokenizes");
        let tree = parse_tokens(query, &tokens, PROGRAM, true).expect("parses");
        // The statement and not the `TopLevelStatement` that wraps it. `TopLevelStatement` covers
        // the terminator too, and at the end of a script the terminator is the end of input token,
        // whose span is the end of the query, so its text runs out to the trailing whitespace.
        let statement = find(&tree, tree.root(), "SelectStatement").expect("there is one");
        assert_eq!(tree.text(statement, query, &tokens), "SELECT 1");
    }

    #[test]
    fn several_statements_parse_as_several() {
        let tree = parse("SELECT 1; SELECT 2; SELECT 3").expect("parses");
        let shape = shape("SELECT 1; SELECT 2; SELECT 3");
        assert_eq!(shape.iter().filter(|name| **name == "SelectStatement").count(), 3);
        assert!(tree.children(tree.root()).count() >= 3);
    }

    #[test]
    fn a_trailing_semicolon_makes_an_empty_statement() {
        // Not a bug and not worth working around here. `TopLevelStatement <- Statement? (';'+ /
        // EndOfInput)` has both halves optional in effect, so at the end of `SELECT 1;` the
        // repetition goes round once more, matches no statement and the end of input, and stops.
        // The extra node has an `EndOfInput` child and no `Statement` one, which is how the
        // transformer tells it apart, and upstream drops it in the same place for the same reason.
        let one = parse("SELECT 1").expect("parses");
        let two = parse("SELECT 1;").expect("parses");
        assert_eq!(one.children(one.root()).count(), 1);
        assert_eq!(two.children(two.root()).count(), 2);
        let last = two.children(two.root()).last().expect("there is a last one");
        let inside: Vec<_> = two.children(last).map(|child| two.name(child)).collect();
        assert_eq!(inside, ["EndOfInput"], "the extra one holds no statement");
    }

    #[test]
    fn rubbish_after_a_statement_is_an_error() {
        // Without the consumed-everything check this parses as `SELECT 1` and drops the rest,
        // because `Program <- TopLevelStatement*` is allowed to stop early.
        let error = parse("SELECT 1 rubbish here").expect_err("not a query");
        assert!(error.message().starts_with("syntax error at or near"), "{}", error.message());
    }

    #[test]
    fn a_word_that_is_not_a_statement_is_an_error() {
        let error = parse("SELCT 1").expect_err("not a query");
        assert!(error.message().contains("syntax error"), "{}", error.message());
    }

    #[test]
    fn the_error_points_at_the_furthest_token_reached() {
        // The parse gives up at the start of the statement, having tried every alternative. The
        // place worth reporting is the furthest one any of them got to, which is the `from`.
        let error = parse("SELECT 1 FROM").expect_err("not a query");
        assert!(error.span().is_some(), "an error about a place should say which place");
    }

    #[test]
    fn a_soft_keyword_is_a_column_name_and_also_a_keyword() {
        // `ascending` is spelled by a rule and is in no class, so it is both of these and the
        // FIRST set for the literal has to be the identifier bit rather than a keyword bucket.
        parse("SELECT ascending FROM t").expect("a soft word is a name");
        parse("SELECT x FROM t ORDER BY x ASCENDING").expect("a soft word is also a literal");
    }

    #[test]
    fn a_reserved_word_is_not_a_column_name() {
        parse("SELECT x FROM t").expect("an ordinary name is fine");
        parse("SELECT * FROM t WHERE all").expect_err("`all` is reserved");
    }

    #[test]
    fn an_unreserved_word_is_a_column_name_everywhere() {
        parse("SELECT abort FROM t").expect("`abort` is unreserved");
    }

    #[test]
    fn a_function_name_keyword_is_a_function_and_not_a_column() {
        // The whole point of the classes. `binary` is in the function name class and nowhere else,
        // so the two positions disagree about it.
        parse("SELECT binary(x) FROM t").expect("a function name position takes it");
        parse("SELECT binary FROM t").expect_err("a column name position does not");
    }

    #[test]
    fn a_quoted_name_is_a_name_whatever_it_spells() {
        parse(r#"SELECT "all" FROM t"#).expect("quoting takes a word out of every class");
    }

    #[test]
    fn adjacent_strings_across_a_line_are_one_literal() {
        parse("SELECT 'a'\n'b'").expect("a continuation");
        parse("SELECT 'a' 'b'").expect_err("on one line they are two strings and a syntax error");
    }

    #[test]
    fn deep_nesting_is_an_error_and_not_a_crash() {
        // A thread stack would be gone long before this. The number is well past the cap.
        let query = format!("SELECT {}1{}", "(".repeat(200_000), ")".repeat(200_000));
        let error = parse(&query).expect_err("too deep to parse");
        assert!(error.message().contains("memory exhausted"), "{}", error.message());
    }

    #[test]
    fn nesting_that_is_merely_rude_still_parses() {
        let query = format!("SELECT {}1{}", "(".repeat(500), ")".repeat(500));
        parse(&query).expect("five hundred deep is fine");
    }

    #[test]
    fn a_named_rule_can_be_parsed_on_its_own() {
        let tree = parse_from("SELECT 1", "SelectStatement", true).expect("parses");
        assert_eq!(tree.name(tree.root()), "SelectStatement");
    }

    #[test]
    fn asking_for_a_rule_that_does_not_exist_says_so() {
        let error = parse_from("SELECT 1", "NoSuchRule", true).expect_err("no such rule");
        assert!(error.message().contains("NoSuchRule"));
    }

    #[test]
    fn the_children_of_a_leaf_rule_are_none() {
        let tree = parse("SELECT 1").expect("parses");
        let mut leaves = 0;
        for index in 0..tree.arena_len() as u32 {
            if tree.node(index).first_child == NONE {
                leaves += 1;
            }
        }
        assert!(leaves > 0, "every tree has leaves");
    }

    #[test]
    fn what_counts_as_a_bare_operator() {
        // The ones a user can define, which is the only thing the generic node is for.
        for text in ["&&", "@>", "<@", "||", "^@", "<<", ">>", "//", "**", "<<=", ">>="] {
            assert!(is_bare_operator(text), "{text} should be an operator");
        }
        // Spelled by a rule, so the generic node has to leave them alone.
        for text in ["->", "->>", "<=", ">=", "!=", "==", "<>", "~~", "!~~*"] {
            assert!(!is_bare_operator(text), "{text} is spelled by a rule");
        }
        // A colon is not an operator character, so neither of these is one.
        for text in ["::", ":=", "+", "(", ","] {
            assert!(!is_bare_operator(text), "{text} is not an operator");
        }
    }

    #[test]
    fn the_corpus_parses() {
        for query in CORPUS {
            parse(query).unwrap_or_else(|error| panic!("{query}\n  {}", error.message()));
        }
    }

    #[test]
    fn the_corpus_parses_the_same_with_the_filter_off() {
        for query in CORPUS {
            let filtered = parse_from(query, "Program", true).expect("parses");
            let plain = parse_from(query, "Program", false).expect("parses unfiltered");
            let mut a = Vec::new();
            let mut b = Vec::new();
            names(&filtered, filtered.root(), &mut a);
            names(&plain, plain.root(), &mut b);
            assert_eq!(a, b, "{query} parsed differently with the filter on");
        }
    }

    #[test]
    fn the_work_stays_proportional_to_the_query() {
        // A guard against the kind of regression that does not fail a test: a grammar or filter
        // change that leaves every query still parsing and quietly triples what it costs. The
        // numbers are what the table does today with a little room, not a target. The expression
        // grammar is about twenty rules deep from `Expression` down to `BaseExpression` and every
        // operand walks all of them, which is where most of these go.
        for query in CORPUS {
            let tree = parse(query).expect("parses");
            let tokens = tokenize(query).expect("tokenizes").len() as u64;
            let per_token = tree.steps() / tokens;
            assert!(per_token < 200, "{query} took {per_token} steps a token");
        }
    }

    #[test]
    fn what_counts_as_a_plain_string() {
        assert!(is_plain_string("'a'"));
        assert!(!is_plain_string("E'a'"));
        assert!(!is_plain_string("$$a$$"));
        assert!(!is_plain_string(r#""a""#));
    }
}
