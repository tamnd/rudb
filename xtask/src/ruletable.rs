//! Compiling the parsed grammar into a flat table the matcher walks.
//!
//! Upstream builds a tree of `Matcher` objects, one virtual call per node, allocated in an arena
//! at startup and chased by pointer at match time. We build the same shape as two parallel arrays
//! and index into them. Every node is twelve bytes in `NODES` and its FIRST set is eight bytes at
//! the same index in `FIRST`, so the filter that decides whether an alternative can possibly match
//! touches one cache line of `FIRST` and never loads the node at all.
//!
//! What the compiler is doing, in the order it does it:
//!
//! 1. Resolve what a rule name means. Twenty five of them are not grammar at all, they are matcher
//!    overrides, and their written bodies are dead. Five more are the keyword class rules that
//!    upstream's build script appends to the grammar text from the `.list` files, and they compile
//!    to a mask test rather than to the two hundred way ordered choice they are written as.
//! 2. Walk from `Program` and keep only what is reachable. That is what makes the dead bodies
//!    provably dead rather than merely unused.
//! 3. Monomorphize `List(D)` and `Parens(D)` at each call site, which is what upstream does too.
//! 4. Solve nullability and FIRST to a fixpoint over the whole table.
//! 5. Refuse to emit anything we cannot run: a reachable capture, character class or negative
//!    lookahead, or a nullable body under `+`, which upstream would loop on forever.
//!
//! `spec/20-the-grammar.md` sections 3 and 5 are the design and the reasoning.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::grammar::Expr;

/// The rule the whole thing hangs off. Upstream validates that it exists before compiling.
const ROOT: &str = "Program";
/// The other root upstream requires, used when parsing one statement rather than a script.
const STATEMENT_ROOT: &str = "TopLevelStatement";

/// The suggestion each identifier matcher was built with, in the order the codes are assigned.
///
/// This is not decoration. Upstream derives two things from it that decide whether a token
/// matches: which keyword class is allowed as a bare word, and whether a single quoted string is
/// accepted where a name is expected. Carrying the suggestion rather than the two derived answers
/// means the derivation lives in one place in `rudb-parse` and reads like `identifier_matcher.hpp`
/// does.
const SUGGESTIONS: [&str; 11] = [
    "SUGGEST_VARIABLE",
    "SUGGEST_CATALOG_NAME",
    "SUGGEST_SCHEMA_NAME",
    "SUGGEST_TABLE_NAME",
    "SUGGEST_COLUMN_NAME",
    "SUGGEST_SCALAR_FUNCTION_NAME",
    "SUGGEST_TABLE_FUNCTION_NAME",
    "SUGGEST_TYPE_NAME",
    "SUGGEST_PRAGMA_NAME",
    "SUGGEST_SETTING_NAME",
    "SUGGEST_FILE_NAME",
];

/// The five keyword class rule names, in the bit order `generated::keywords` assigns.
const CLASS_RULES: [&str; 5] = [
    "ReservedKeyword",
    "UnreservedKeyword",
    "ColumnNameKeyword",
    "FuncNameKeyword",
    "TypeNameKeyword",
];

/// How many bits of a FIRST set are spent on token kinds before the keyword buckets start.
pub(crate) const KIND_BITS: u32 = 6;
/// The keyword buckets. 58 of them, being the 64 bits of a FIRST set less the six kind bits.
pub(crate) const BUCKETS: u32 = 64 - KIND_BITS;

/// A FIRST bit per token kind. Mirrored by `Kind::first_bit` in `rudb-parse`, and the test at the
/// bottom of this file is what keeps the two in step.
const FIRST_IDENT: u64 = 1 << 0;
const FIRST_NUMBER: u64 = 1 << 1;
const FIRST_STRING: u64 = 1 << 2;
const FIRST_OPERATOR: u64 = 1 << 3;
const FIRST_TERMINATOR: u64 = 1 << 4;
const FIRST_END: u64 = 1 << 5;
/// Every keyword bucket at once.
const FIRST_ANY_KEYWORD: u64 = !((1 << KIND_BITS) - 1);

/// What a node is. The discriminants are written into the generated table, so `rudb-parse`'s `Op`
/// has to agree with this one value for value, which `the_ops_agree_with_the_parser` checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Op {
    /// `a` is an index into `generated::keywords::KEYWORDS`.
    Keyword = 0,
    /// `a` is an index into `SYMBOLS`. Punctuation and operator literals.
    Symbol = 1,
    /// `a` is a rule index. The only node that is not expanded in place.
    Rule = 2,
    /// `a` is a start index into `CHILDREN`, `b` is how many.
    Sequence = 3,
    /// Same layout as a sequence. Ordered, first match wins.
    Choice = 4,
    /// `a` is a node index.
    Optional = 5,
    /// `a` is a node index. One or more, never zero or more: `X*` is compiled as
    /// `Optional(Repeat(X))`, which is exactly what `matcher_factory.cpp` builds.
    Repeat = 6,
    /// `a` is a suggestion code, `flags` bit 0 says the keyword check is dropped.
    Identifier = 7,
    Number = 8,
    String = 9,
    Operator = 10,
    EndOfInput = 11,
    /// `a` is a keyword class mask.
    KeywordClass = 12,
}

/// Twelve bytes, laid out the way the generated table writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Node {
    pub(crate) op: Op,
    pub(crate) flags: u8,
    pub(crate) a: u32,
    pub(crate) b: u32,
}

/// One rule that survived reachability.
#[derive(Debug, Clone)]
pub(crate) struct Compiled {
    pub(crate) name: String,
    pub(crate) root: u32,
    /// Whether upstream memoizes it. Twenty two rules, listed in `memoized_rules.list`.
    pub(crate) memoized: bool,
}

/// Everything the generator emits.
pub(crate) struct Table {
    pub(crate) nodes: Vec<Node>,
    pub(crate) children: Vec<u32>,
    pub(crate) first: Vec<u64>,
    pub(crate) nullable: Vec<bool>,
    pub(crate) rules: Vec<Compiled>,
    pub(crate) symbols: Vec<String>,
    pub(crate) program: u32,
    pub(crate) top_level: u32,
}

/// What a rule name resolves to before anything is compiled.
enum Resolved<'a> {
    /// An ordinary grammar rule, with its body.
    Body(&'a Expr),
    /// A matcher override. The written body, if there is one, is dead.
    Override(Node),
    /// One of the five keyword class rules, as a mask test.
    Class(u8),
}

/// Compiles the grammar.
///
/// `keywords` is the table `codegen::keyword_table` built, and the indices into it are what a
/// `Keyword` node carries, so the two files are generated from one run and cannot drift.
/// `overrides` and `memoized` are the two vendored `.list` files.
pub(crate) fn compile(
    parsed: &[crate::grammar::Rule],
    keywords: &[(String, u8)],
    overrides: &[(String, String, String)],
    memoized: &BTreeSet<String>,
) -> Result<Table, String> {
    let mut bodies: BTreeMap<&str, &crate::grammar::Rule> = BTreeMap::new();
    for rule in parsed {
        // A duplicate name is upstream's last definition winning, which is how a grammar extension
        // replaces a rule. Nothing in the vendored tree does it, so say so rather than pick.
        if let Some(first) = bodies.insert(rule.name.as_str(), rule) {
            return Err(format!(
                "{} defines {} and so does {}, and this compiler has no rule for which one wins",
                first.origin, rule.name, rule.origin
            ));
        }
    }

    let mut resolved: BTreeMap<String, Resolved<'_>> = BTreeMap::new();
    for (name, class, suggestion) in overrides {
        resolved.insert(name.clone(), Resolved::Override(terminal(name, class, suggestion)?));
    }
    // Upstream adds this one outside the generated block in `compiled_grammar.cpp`, so it is not
    // in the vendored list. `TopLevelStatement <- Statement? (';'+ / EndOfInput)` needs it and no
    // grammar file defines it.
    resolved.entry("EndOfInput".to_string()).or_insert(Resolved::Override(Node {
        op: Op::EndOfInput,
        flags: 0,
        a: 0,
        b: 0,
    }));
    for (index, name) in CLASS_RULES.iter().enumerate() {
        // An override wins. `ReservedKeyword` is in both lists, and upstream's override turns it
        // into a matcher that accepts any word at all, so `ColLabel <- ReservedKeyword / ...` is
        // not the reserved word test it reads as.
        resolved.entry((*name).to_string()).or_insert(Resolved::Class(1 << index));
    }
    for (name, rule) in &bodies {
        resolved.entry((*name).to_string()).or_insert(Resolved::Body(&rule.body));
    }

    let reachable = reach(&resolved, &bodies)?;
    let ids: BTreeMap<&str, u32> =
        reachable.iter().enumerate().map(|(index, name)| (name.as_str(), index as u32)).collect();

    let mut compiler = Compiler {
        resolved: &resolved,
        bodies: &bodies,
        ids: &ids,
        keywords: keywords
            .iter()
            .enumerate()
            .map(|(index, (word, _))| (word.as_str(), index as u32))
            .collect(),
        nodes: Vec::new(),
        children: Vec::new(),
        symbols: Vec::new(),
    };

    let mut rules = Vec::with_capacity(reachable.len());
    for name in &reachable {
        let root = match compiler.resolved.get(name.as_str()) {
            Some(Resolved::Body(body)) => compiler.expr(body, &BTreeMap::new(), name)?,
            Some(Resolved::Override(node)) => compiler.push(*node),
            Some(Resolved::Class(mask)) => {
                compiler.push(Node { op: Op::KeywordClass, flags: 0, a: u32::from(*mask), b: 0 })
            }
            None => return Err(format!("{name} is reachable and nothing defines it")),
        };
        rules.push(Compiled { name: name.clone(), root, memoized: memoized.contains(name) });
    }

    for name in memoized {
        if !ids.contains_key(name.as_str()) {
            return Err(format!(
                "memoized_rules.list names {name}, which is not a rule reachable from {ROOT}"
            ));
        }
    }

    let Compiler { nodes, children, symbols, .. } = compiler;
    let class_masks = class_masks(keywords);
    let (nullable, first) =
        solve(&nodes, &children, &rules, &symbols, &compiler_keywords(keywords), &class_masks);
    check(&nodes, &children, &rules, &nullable)?;

    let program =
        *ids.get(ROOT).ok_or_else(|| format!("the grammar has no {ROOT} rule to start from"))?;
    let top_level = *ids
        .get(STATEMENT_ROOT)
        .ok_or_else(|| format!("the grammar has no {STATEMENT_ROOT} rule"))?;

    Ok(Table { nodes, children, first, nullable, rules, symbols, program, top_level })
}

/// The terminal node one line of `matcher_overrides.list` describes.
fn terminal(name: &str, class: &str, suggestion: &str) -> Result<Node, String> {
    match class {
        "IdentifierMatcher" | "ReservedIdentifierMatcher" => {
            let code =
                SUGGESTIONS.iter().position(|known| *known == suggestion).ok_or_else(|| {
                    format!("{name} was built with an unknown suggestion {suggestion}")
                })?;
            // The whole difference between the two classes: the reserved one skips the keyword
            // check, so any word matches. `identifier_matcher.hpp` is four lines on this.
            let flags = u8::from(class == "ReservedIdentifierMatcher");
            Ok(Node { op: Op::Identifier, flags, a: code as u32, b: 0 })
        }
        "NumberLiteralMatcher" => Ok(Node { op: Op::Number, flags: 0, a: 0, b: 0 }),
        "StringLiteralMatcher" => Ok(Node { op: Op::String, flags: 0, a: 0, b: 0 }),
        "OperatorMatcher" => Ok(Node { op: Op::Operator, flags: 0, a: 0, b: 0 }),
        "EndOfInputMatcher" => Ok(Node { op: Op::EndOfInput, flags: 0, a: 0, b: 0 }),
        other => {
            Err(format!("{name} is overridden with {other}, which this compiler does not know"))
        }
    }
}

/// Every rule reachable from `Program`, sorted by name.
///
/// Sorted rather than in discovery order so that the generated file depends on the grammar and not
/// on the order this happens to walk it. Reachability is the thing that makes the overridden rules'
/// written bodies provably dead: `Identifier <- QuotedIdentifier / PlainIdentifier` is in the
/// grammar, the matcher never looks at it, and so `PlainIdentifier` and its `!ReservedKeyword`
/// lookahead are unreachable and this compiler never has to have an answer for them.
fn reach(
    resolved: &BTreeMap<String, Resolved<'_>>,
    bodies: &BTreeMap<&str, &crate::grammar::Rule>,
) -> Result<Vec<String>, String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    for root in [ROOT, STATEMENT_ROOT] {
        seen.insert(root.to_string());
        queue.push_back(root.to_string());
    }

    while let Some(name) = queue.pop_front() {
        let Some(entry) = resolved.get(&name) else {
            return Err(format!("{name} is referenced and nothing defines it"));
        };
        // An override or a class rule has no body to walk, which is the point of both.
        let Resolved::Body(body) = entry else { continue };
        let origin = bodies.get(name.as_str()).map_or("?", |rule| rule.origin.as_str());

        let mut referenced = BTreeSet::new();
        let mut called = BTreeSet::new();
        references(body, &mut referenced, &mut called);

        // A call is expanded at the call site, so the template never becomes a rule of its own and
        // its parameter never has to resolve to anything. What the template references, though, is
        // reachable through it, and `List(D) <- D (',' D)* ','?` referencing nothing is a fact
        // about today's grammar rather than a rule of the meta-language.
        for template in called {
            let rule = bodies.get(template.as_str()).ok_or_else(|| {
                format!("{origin}: {name} calls {template}, which nothing defines")
            })?;
            let parameter = rule.parameter.as_ref().ok_or_else(|| {
                format!("{origin}: {name} calls {template}, which takes no parameter")
            })?;
            let mut inner = BTreeSet::new();
            let mut nested = BTreeSet::new();
            references(&rule.body, &mut inner, &mut nested);
            inner.remove(parameter);
            referenced.extend(inner);
            referenced.extend(nested);
        }

        for next in referenced {
            if !resolved.contains_key(&next) {
                return Err(format!("{origin}: {name} references {next}, which nothing defines"));
            }
            if seen.insert(next.clone()) {
                queue.push_back(next);
            }
        }
    }
    Ok(seen.into_iter().collect())
}

/// Every rule an expression mentions, split into plain references and call targets.
///
/// They have to be kept apart because a call target is not a rule the table holds. It is a
/// template that gets copied into every call site, which is what upstream's `MatcherFactory` does
/// with a parameterized rule and why `List` never appears as a matcher of its own.
fn references(expr: &Expr, into: &mut BTreeSet<String>, calls: &mut BTreeSet<String>) {
    match expr {
        Expr::Reference(name) => {
            into.insert(name.clone());
        }
        Expr::Call(name, argument) => {
            calls.insert(name.clone());
            references(argument, into, calls);
        }
        Expr::Sequence(items) | Expr::Choice(items) => {
            for item in items {
                references(item, into, calls);
            }
        }
        Expr::Optional(inner) | Expr::Repeat(inner) | Expr::Not(inner) | Expr::Capture(inner) => {
            references(inner, into, calls);
        }
        Expr::Literal(_) | Expr::CharClass(_) => {}
    }
}

struct Compiler<'a> {
    resolved: &'a BTreeMap<String, Resolved<'a>>,
    bodies: &'a BTreeMap<&'a str, &'a crate::grammar::Rule>,
    ids: &'a BTreeMap<&'a str, u32>,
    keywords: BTreeMap<&'a str, u32>,
    nodes: Vec<Node>,
    children: Vec<u32>,
    symbols: Vec<String>,
}

impl Compiler<'_> {
    fn push(&mut self, node: Node) -> u32 {
        self.nodes.push(node);
        (self.nodes.len() - 1) as u32
    }

    /// Compiles one expression. `bound` carries the argument of a parameterized rule, already
    /// compiled, so that `D` inside `List(D)` resolves to the node the call site built.
    fn expr(
        &mut self,
        expr: &Expr,
        bound: &BTreeMap<String, u32>,
        owner: &str,
    ) -> Result<u32, String> {
        match expr {
            Expr::Literal(text) => self.literal(text, owner),
            Expr::Reference(name) => {
                if let Some(node) = bound.get(name) {
                    return Ok(*node);
                }
                let id = self.ids.get(name.as_str()).ok_or_else(|| {
                    format!("{owner} references {name}, which did not survive reachability")
                })?;
                Ok(self.push(Node { op: Op::Rule, flags: 0, a: *id, b: 0 }))
            }
            Expr::Call(name, argument) => self.call(name, argument, bound, owner),
            Expr::Sequence(items) => self.list(Op::Sequence, items, bound, owner),
            Expr::Choice(items) => self.list(Op::Choice, items, bound, owner),
            Expr::Optional(inner) => {
                let child = self.expr(inner, bound, owner)?;
                Ok(self.push(Node { op: Op::Optional, flags: 0, a: child, b: 0 }))
            }
            Expr::Repeat(inner) => {
                let child = self.expr(inner, bound, owner)?;
                Ok(self.push(Node { op: Op::Repeat, flags: 0, a: child, b: 0 }))
            }
            Expr::Not(_) => Err(format!(
                "{owner} has a negative lookahead in a reachable position. Upstream parses `!` and \
                 then ignores it, so there is no behaviour here to copy and guessing one would \
                 change what the dialect accepts"
            )),
            Expr::Capture(_) => Err(format!(
                "{owner} has a capture in a reachable position, which means a rule that used to be \
                 a matcher override now has to be walked, and the tokenizer has already decided \
                 what its text is"
            )),
            Expr::CharClass(class) => Err(format!(
                "{owner} has the character class {class} in a reachable position. Scanning bytes is \
                 the tokenizer's job and the matcher never sees them"
            )),
        }
    }

    fn list(
        &mut self,
        op: Op,
        items: &[Expr],
        bound: &BTreeMap<String, u32>,
        owner: &str,
    ) -> Result<u32, String> {
        let compiled: Vec<u32> =
            items.iter().map(|item| self.expr(item, bound, owner)).collect::<Result<_, _>>()?;
        let start = self.children.len() as u32;
        self.children.extend_from_slice(&compiled);
        Ok(self.push(Node { op, flags: 0, a: start, b: compiled.len() as u32 }))
    }

    /// Monomorphizes `List(D)` or `Parens(D)` at this call site.
    ///
    /// Upstream does the same and does not cache, so two call sites with the same argument build
    /// two matcher trees. Copying that is not laziness: the instantiation keeps the rule name of
    /// the template, so a parse result from `List(Expression)` is named `List` and the transformer
    /// reads it positionally. Sharing them would be fine for matching and wrong for anything that
    /// later wants a node identity per call site.
    fn call(
        &mut self,
        name: &str,
        argument: &Expr,
        bound: &BTreeMap<String, u32>,
        owner: &str,
    ) -> Result<u32, String> {
        let rule = self
            .bodies
            .get(name)
            .ok_or_else(|| format!("{owner} calls {name}, which is not a parameterized rule"))?;
        let parameter = rule.parameter.as_ref().ok_or_else(|| {
            format!("{owner} calls {name} with an argument and {name} takes none")
        })?;
        let compiled = self.expr(argument, bound, owner)?;
        let mut inner = BTreeMap::new();
        inner.insert(parameter.clone(), compiled);
        // The template's body is compiled fresh here, so a nested call inside it monomorphizes
        // again with the right binding.
        let body = match self.resolved.get(name) {
            Some(Resolved::Body(body)) => *body,
            _ => return Err(format!("{name} is called and is not an ordinary rule")),
        };
        self.expr(body, &inner, name)
    }

    fn literal(&mut self, text: &str, owner: &str) -> Result<u32, String> {
        if text.is_empty() {
            return Err(format!("{owner} has an empty literal, which matches nothing"));
        }
        if text.bytes().all(|byte| byte.is_ascii_alphabetic() || byte == b'_') {
            let folded = text.to_ascii_lowercase();
            let index = *self.keywords.get(folded.as_str()).ok_or_else(|| {
                format!(
                    "{owner} spells {text}, which is not in the generated keyword table. The table \
                     is built from the same grammar, so this means the two scans disagree"
                )
            })?;
            return Ok(self.push(Node { op: Op::Keyword, flags: 0, a: index, b: 0 }));
        }
        let index = match self.symbols.iter().position(|known| known == text) {
            Some(index) => index as u32,
            None => {
                self.symbols.push(text.to_string());
                (self.symbols.len() - 1) as u32
            }
        };
        Ok(self.push(Node { op: Op::Symbol, flags: 0, a: index, b: 0 }))
    }
}

/// The keyword table as a lookup from index to class mask, for the FIRST solver.
fn compiler_keywords(keywords: &[(String, u8)]) -> Vec<u8> {
    keywords.iter().map(|(_, mask)| *mask).collect()
}

/// For each of the five classes, the union of the buckets of every word in it.
fn class_masks(keywords: &[(String, u8)]) -> [u64; 5] {
    let mut masks = [0u64; 5];
    for (index, (_, classes)) in keywords.iter().enumerate() {
        for (bit, mask) in masks.iter_mut().enumerate() {
            if classes & (1 << bit) != 0 {
                *mask |= bucket(index as u32);
            }
        }
    }
    masks
}

/// Which FIRST bit a keyword falls in.
///
/// Buckets rather than one bit per word, because there are 514 words and 58 bits. A bucket
/// collision costs a node visit that would have been skipped, and never accepts anything, because
/// the filter only ever decides not to try an alternative that could not have matched.
pub(crate) fn bucket(index: u32) -> u64 {
    1 << (KIND_BITS + index % BUCKETS)
}

/// Nullability and FIRST for every node, to a fixpoint.
///
/// Both are mutually recursive through rule references, so this iterates until nothing changes
/// rather than trying to find an order. The grammar is small enough that it settles in a handful
/// of passes and this runs once, at generation time, never at parse time.
fn solve(
    nodes: &[Node],
    children: &[u32],
    rules: &[Compiled],
    symbols: &[String],
    keyword_classes: &[u8],
    class_masks: &[u64; 5],
) -> (Vec<bool>, Vec<u64>) {
    let mut nullable = vec![false; nodes.len()];
    let mut first = vec![0u64; nodes.len()];

    loop {
        let mut changed = false;
        for (index, node) in nodes.iter().enumerate() {
            let (is_nullable, set) = match node.op {
                // Which bit a keyword node waits for is decided by the tokenizer, not by this
                // node. A word in at least one class arrives as a keyword token and carries its
                // bucket. A word in no class arrives as an identifier token, because that is what
                // makes `SELECT ascending FROM t` a column reference, and its bucket bit is
                // therefore never set on anything. Fifteen words are in that state and every one
                // of them is spelled by some rule, so a bucket here would filter out the only
                // token that could ever match, and `ORDER BY x ASCENDING` would stop parsing.
                Op::Keyword => (
                    false,
                    if keyword_classes[node.a as usize] == 0 {
                        FIRST_IDENT
                    } else {
                        bucket(node.a)
                    },
                ),
                // Which kind of token a piece of punctuation arrives as is the tokenizer's
                // business and it is not always the obvious one. A `;` is its own kind, because a
                // statement boundary is decided before the grammar sees it. A `.` is a number,
                // because `.5` is a number and the scan cannot know which it has until it has read
                // the next byte, so `a.b` hands the matcher a number token whose text is `.`, and
                // upstream's own `NumberLiteralMatcher` carries a rule rejecting a lone dot for
                // exactly that reason. Both bits are set for it rather than just the number one, so
                // that a tokenizer that later decides a trailing dot is punctuation does not
                // silently take `DotColLabel` out of the grammar.
                Op::Symbol => (
                    false,
                    match symbols[node.a as usize].as_str() {
                        ";" => FIRST_TERMINATOR,
                        "." => FIRST_NUMBER | FIRST_OPERATOR,
                        _ => FIRST_OPERATOR,
                    },
                ),
                Op::Rule => {
                    let root = rules[node.a as usize].root as usize;
                    (nullable[root], first[root])
                }
                Op::Sequence => {
                    let span = &children[node.a as usize..(node.a + node.b) as usize];
                    let mut set = 0;
                    let mut all_nullable = true;
                    for child in span {
                        set |= first[*child as usize];
                        if !nullable[*child as usize] {
                            all_nullable = false;
                            break;
                        }
                    }
                    (all_nullable, set)
                }
                Op::Choice => {
                    let span = &children[node.a as usize..(node.a + node.b) as usize];
                    let mut set = 0;
                    let mut any_nullable = false;
                    for child in span {
                        set |= first[*child as usize];
                        any_nullable |= nullable[*child as usize];
                    }
                    (any_nullable, set)
                }
                Op::Optional => (true, first[node.a as usize]),
                Op::Repeat => (nullable[node.a as usize], first[node.a as usize]),
                // An identifier matcher takes a bare word, and which words it takes depends on the
                // class of the keyword and on the position, so every keyword bucket is in. That is
                // a superset and a superset is all the filter is allowed to be. A quoted name is an
                // identifier token, and a single quoted string only where the suggestion supports
                // one, which is the table and file name cases.
                Op::Identifier => {
                    let supports_string = matches!(
                        SUGGESTIONS[node.a as usize],
                        "SUGGEST_TABLE_NAME" | "SUGGEST_FILE_NAME"
                    );
                    let mut set = FIRST_IDENT | FIRST_ANY_KEYWORD;
                    if supports_string {
                        set |= FIRST_STRING;
                    }
                    (false, set)
                }
                Op::Number => (false, FIRST_NUMBER),
                Op::String => (false, FIRST_STRING),
                Op::Operator => (false, FIRST_OPERATOR),
                Op::EndOfInput => (false, FIRST_END),
                Op::KeywordClass => {
                    let mut set = 0;
                    for (bit, mask) in class_masks.iter().enumerate() {
                        if node.a & (1 << bit) != 0 {
                            set |= *mask;
                        }
                    }
                    (false, set)
                }
            };
            if nullable[index] != is_nullable || first[index] != set {
                nullable[index] = is_nullable;
                first[index] = set;
                changed = true;
            }
        }
        if !changed {
            return (nullable, first);
        }
    }
}

/// The things we refuse to emit.
///
/// A nullable body under `+` is the one worth spelling out. `RepeatMatchProcess` in
/// `matcher_process.cpp` loops while the child succeeds and has no guard for a child that succeeds
/// without consuming a token, so upstream would hang. There is no such rule today and this is a
/// static guarantee that there is not one tomorrow, which is cheaper than the runtime guard it
/// replaces.
fn check(
    nodes: &[Node],
    children: &[u32],
    rules: &[Compiled],
    nullable: &[bool],
) -> Result<(), String> {
    let mut owner: Vec<&str> = vec![""; nodes.len()];
    for rule in rules {
        mark(nodes, children, rule.root, &rule.name, &mut owner);
    }
    for (index, node) in nodes.iter().enumerate() {
        if node.op == Op::Repeat && nullable[node.a as usize] {
            return Err(format!(
                "{}: a repeat whose body can match nothing, which loops forever",
                owner[index]
            ));
        }
    }
    Ok(())
}

/// Attributes each node to the rule it was compiled for, for error messages only.
fn mark<'a>(nodes: &[Node], children: &[u32], at: u32, name: &'a str, owner: &mut Vec<&'a str>) {
    let index = at as usize;
    if !owner[index].is_empty() {
        return;
    }
    owner[index] = name;
    match nodes[index].op {
        Op::Sequence | Op::Choice => {
            let node = nodes[index];
            for child in &children[node.a as usize..(node.a + node.b) as usize] {
                mark(nodes, children, *child, name, owner);
            }
        }
        Op::Optional | Op::Repeat => mark(nodes, children, nodes[index].a, name, owner),
        _ => {}
    }
}

/// Every op, in discriminant order. Only the tests walk it, and they are what it is for: a Rust
/// enum cannot be enumerated without either a derive macro or a list like this one, and the list
/// is only useful if something checks that it is complete, which is what
/// `the_discriminants_are_dense_and_in_order` does.
#[cfg(test)]
const OPS: [Op; 13] = [
    Op::Keyword,
    Op::Symbol,
    Op::Rule,
    Op::Sequence,
    Op::Choice,
    Op::Optional,
    Op::Repeat,
    Op::Identifier,
    Op::Number,
    Op::String,
    Op::Operator,
    Op::EndOfInput,
    Op::KeywordClass,
];

/// The name of an op as `rudb-parse` spells it.
pub(crate) fn op_name(op: Op) -> &'static str {
    match op {
        Op::Keyword => "Keyword",
        Op::Symbol => "Symbol",
        Op::Rule => "Rule",
        Op::Sequence => "Sequence",
        Op::Choice => "Choice",
        Op::Optional => "Optional",
        Op::Repeat => "Repeat",
        Op::Identifier => "Identifier",
        Op::Number => "Number",
        Op::String => "String",
        Op::Operator => "Operator",
        Op::EndOfInput => "EndOfInput",
        Op::KeywordClass => "KeywordClass",
    }
}

/// The suggestion name for a code, as `rudb-parse` spells it.
///
/// Same order as `SUGGESTIONS`, which is upstream's spelling. Kept as two lists rather than one
/// derived from the other, because the mapping is the thing a reader checks and a transformation
/// that turns `SUGGEST_SCALAR_FUNCTION_NAME` into `ScalarFunctionName` would be more code and less
/// obvious than writing both down.
pub(crate) fn suggestion_name(code: u32) -> &'static str {
    const VARIANTS: [&str; SUGGESTIONS.len()] = [
        "Variable",
        "CatalogName",
        "SchemaName",
        "TableName",
        "ColumnName",
        "ScalarFunctionName",
        "TableFunctionName",
        "TypeName",
        "PragmaName",
        "SettingName",
        "FileName",
    ];
    VARIANTS[code as usize]
}

#[cfg(test)]
mod tests {
    use super::{OPS, Op, SUGGESTIONS, op_name, suggestion_name};

    /// The variant names an enum declares, in order, read out of a Rust source file.
    ///
    /// Crude on purpose. `xtask` does not depend on `rudb-parse` and should not start to, because
    /// the generator has to be able to run when `rudb-parse` does not compile, which is exactly
    /// the state the tree is in halfway through a grammar bump. Reading the text is enough to
    /// catch the failure that matters, which is the generator emitting a variant name the parser
    /// crate does not define, or the two disagreeing about the order of the discriminants.
    fn variants(file: &str, name: &str) -> Vec<String> {
        let path = crate::root().join("crates/rudb-parse/src").join(file);
        let text = std::fs::read_to_string(&path).expect("the parser crate is not there");
        let start = text.find(&format!("pub enum {name} {{")).expect("the enum is not there");
        let body = &text[start..];
        let end = body.find("\n}").expect("the enum does not end");
        body[..end]
            .lines()
            .filter_map(|line| line.trim().split_once(" = "))
            .map(|(variant, _)| variant.to_string())
            .collect()
    }

    #[test]
    fn the_ops_agree_with_the_parser() {
        let ours: Vec<String> = OPS.iter().map(|op| op_name(*op).to_string()).collect();
        assert_eq!(ours, variants("rules.rs", "Op"));
    }

    #[test]
    fn the_suggestions_agree_with_the_parser() {
        let ours: Vec<String> =
            (0..SUGGESTIONS.len() as u32).map(|code| suggestion_name(code).to_string()).collect();
        assert_eq!(ours, variants("rules.rs", "Suggestion"));
    }

    #[test]
    fn the_discriminants_are_dense_and_in_order() {
        for (index, op) in OPS.iter().enumerate() {
            assert_eq!(*op as u8 as usize, index, "{} is out of order", op_name(*op));
        }
    }

    /// The real grammar, compiled, checked for the things a person would otherwise have to notice
    /// by reading four thousand generated lines.
    #[test]
    fn the_vendored_grammar_compiles() {
        let grammar = crate::root().join(crate::vendor::DEST);
        let keywords = crate::codegen::keyword_table(&grammar).expect("the keyword table");
        let parsed =
            crate::grammar::parse_dir(&grammar.join("statements")).expect("the grammar parses");
        let overrides = crate::codegen::overrides(&grammar).expect("the override list");
        let memoized = crate::codegen::memoized(&grammar).expect("the memoized list");
        let table = super::compile(&parsed, &keywords, &overrides, &memoized).expect("it compiles");

        let names: Vec<&str> = table.rules.iter().map(|rule| rule.name.as_str()).collect();
        assert!(names.contains(&"Program"));
        assert!(names.contains(&"TopLevelStatement"));
        // Overridden, so the rule is a terminal and its written body is dead. The body's only
        // other reference is what proves it: nothing else reaches PlainIdentifier.
        assert!(names.contains(&"Identifier"));
        assert!(!names.contains(&"PlainIdentifier"));
        assert!(!names.contains(&"QuotedIdentifier"));
        // Expanded at every call site, the way MatcherFactory monomorphizes them, so neither is a
        // rule of its own.
        assert!(!names.contains(&"List"));
        assert!(!names.contains(&"Parens"));
        // Appended to the grammar text by upstream's build script from the .list files, and
        // compiled here to a mask test rather than to a two hundred way choice.
        assert!(names.contains(&"UnreservedKeyword"));
        assert!(names.contains(&"ColumnNameKeyword"));
        // Referenced by TopLevelStatement and defined by no grammar file. Upstream adds it outside
        // the generated block, which is why it is not in matcher_overrides.list.
        assert!(names.contains(&"EndOfInput"));

        assert_eq!(
            table.rules.iter().filter(|rule| rule.memoized).count(),
            memoized.len(),
            "every memoized rule should have survived reachability"
        );

        // A node that can start with nothing can never match, which would mean the FIRST solver
        // never reached it or the grammar has an alternative that is dead.
        for (index, set) in table.first.iter().enumerate() {
            assert_ne!(
                *set, 0,
                "node {index} ({:?}) can start with nothing",
                table.nodes[index].op
            );
        }

        // Sorted by name, because the generated file is compared byte for byte and the walk order
        // must not leak into it.
        for pair in table.rules.windows(2) {
            assert!(pair[0].name < pair[1].name, "{} then {}", pair[0].name, pair[1].name);
        }

        // Both roots resolve to themselves.
        assert_eq!(table.rules[table.program as usize].name, "Program");
        assert_eq!(table.rules[table.top_level as usize].name, "TopLevelStatement");
    }

    /// The class rules are the reason the table is worth generating rather than interpreting.
    #[test]
    fn a_keyword_class_is_a_mask_and_not_two_hundred_alternatives() {
        let grammar = crate::root().join(crate::vendor::DEST);
        let keywords = crate::codegen::keyword_table(&grammar).expect("the keyword table");
        let parsed =
            crate::grammar::parse_dir(&grammar.join("statements")).expect("the grammar parses");
        let overrides = crate::codegen::overrides(&grammar).expect("the override list");
        let memoized = crate::codegen::memoized(&grammar).expect("the memoized list");
        let table = super::compile(&parsed, &keywords, &overrides, &memoized).expect("it compiles");

        let rule = table
            .rules
            .iter()
            .find(|rule| rule.name == "UnreservedKeyword")
            .expect("UnreservedKeyword is a rule");
        assert_eq!(table.nodes[rule.root as usize].op, Op::KeywordClass);

        // ReservedKeyword is in both the class list and the override list, and the override wins.
        // Upstream builds it with ReservedIdentifierMatcher, which drops the keyword check, so the
        // rule that reads as "a reserved word" accepts any word at all. Getting this backwards
        // would silently narrow ColLabel and DefArgKeyword.
        let reserved = table
            .rules
            .iter()
            .find(|rule| rule.name == "ReservedKeyword")
            .expect("ReservedKeyword is a rule");
        let node = table.nodes[reserved.root as usize];
        assert_eq!(node.op, Op::Identifier);
        assert_eq!(node.flags & 1, 1, "the keyword check should be dropped");
    }
}
