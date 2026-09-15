//! Writing statements out of the rule table, which is the matcher run in the other direction.
//!
//! The matcher walks the table over a token vector and decides where each rule started and stopped.
//! This walks the same table with no tokens in hand and makes them up: at a choice it picks an
//! alternative, at a repeat it picks a count, at an identifier it asks a catalog for a name. What
//! comes out is a statement the grammar can produce, which is a much larger set than the statements
//! anybody has written down.
//!
//! It lives in the library rather than in a test because two things outside this crate need it. The
//! compatibility harness generates statements and runs them through both engines, and it depends on
//! `rudb` and on nothing else, so anything it cannot reach through the facade is a hole in the
//! facade. `spec/sql/duckdb/10-generation-and-fuzzing.md` section 10.2 is the argument for building
//! it at all.
//!
//! Three things keep it from producing either a novel or the same four tokens forever.
//!
//! Every node has a cost, which is the smallest number of tokens it can be written down in, and it
//! is a fixpoint over the table computed once for the process. A choice weights its alternatives by
//! that cost, so the cheap ones are picked more often and the expression grammar does not run away
//! down its own left hand side. It is also what guarantees termination: once a statement is over
//! budget or a rule has come round too often, every choice takes its cheapest alternative, every
//! optional is skipped and every repeat goes round once, and the cheapest expansion of anything is
//! finite by construction.
//!
//! The recursion bound counts how many times one rule is on the path rather than how long the path
//! is, which is not the same thing in this grammar and the difference is not subtle. The expression
//! rules are a chain of about twenty, one per precedence level, and a plain `a + b` walks the whole
//! chain, so a depth bound tight enough to stop nesting is spent before it reaches a leaf and every
//! leaf comes out as the cheapest literal there is. The first version of this had one and wrote
//! several hundred expressions without a single column reference in any of them.
//!
//! Names come from a catalog rather than from a pool of letters, and one statement draws its
//! columns from one table. Neither of those makes the statement mean anything, since a PEG walk has
//! no idea what a scope is, but both of them mean a generated statement has a real chance of
//! binding rather than dying on the first name, and a statement that dies on the first name tests
//! the tokenizer and nothing else.
//!
//! What it does not promise is that everything it writes parses. Ordered choice is the reason: this
//! can pick the fifth alternative of a choice and write text the matcher settles on the second
//! alternative of, and then the rest of the sequence has nothing to match against. That is a
//! property of every PEG generator and not a bug here. The share that parses is measured rather
//! than assumed, by `the_generator_writes_statements_that_parse`, and the interesting inputs are
//! the ones that do not, because our answer and DuckDB's answer on those is exactly the level two
//! statement number the harness reports.

use std::sync::OnceLock;

use rudb_common::{Error, Result};

use crate::generated::keywords::KEYWORDS;
use crate::generated::rules::{CHILDREN, NODES, RULES, SYMBOLS};
use crate::matcher::SUGGESTIONS;
use crate::rules::{Node, Op, Suggestion};

/// A node with no finite expansion, which is what a rule that can only refer to itself comes out
/// as. Nothing is ever generated from one.
const UNREACHABLE: u32 = u32::MAX;

/// The rule a statement is written from when nothing says otherwise.
const START: &str = "Statement";

/// How many tokens a statement gets before every remaining decision takes the cheap way out.
const BUDGET: u32 = 60;

/// How many times one rule may be on the path from the root before the same thing happens.
const REPEATS: u32 = 3;

/// How many rules may be on that path at once, whatever they are.
///
/// This one is not about the shape of the output. The walk is Rust recursion, so a grammar that
/// went round a cycle of rules that write nothing would run out of thread stack rather than
/// producing a bad statement, and a number that no real statement comes near is cheap insurance.
const STACK: u32 = 512;

/// Where a name position gets its name.
///
/// Owned strings rather than borrowed ones, because the useful catalog is the one a harness reads
/// off a live database rather than the one written here, and the default is only what makes the
/// tests runnable and the examples readable.
#[derive(Debug, Clone)]
pub struct Catalog {
    /// The tables, with their columns. One statement picks one of these and takes its column names
    /// from it, so `SELECT a FROM t` is far likelier than `SELECT a FROM u`.
    pub tables: Vec<Table>,
    pub functions: Vec<String>,
    pub table_functions: Vec<String>,
    pub types: Vec<String>,
    pub schemas: Vec<String>,
    pub catalogs: Vec<String>,
    pub pragmas: Vec<String>,
    pub settings: Vec<String>,
    /// File names, which are the one name position where a single quoted string is what a person
    /// would write, so these carry their quotes.
    pub files: Vec<String>,
    pub variables: Vec<String>,
}

/// One table in the catalog.
#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<String>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            tables: vec![
                Table {
                    name: "t".into(),
                    columns: ["a", "b", "c"].iter().map(|name| (*name).into()).collect(),
                },
                Table {
                    name: "u".into(),
                    columns: ["x", "y"].iter().map(|name| (*name).into()).collect(),
                },
            ],
            functions: names(&["abs", "length", "upper", "count", "coalesce"]),
            table_functions: names(&["range", "generate_series"]),
            types: names(&["INTEGER", "VARCHAR", "DOUBLE", "BOOLEAN", "DATE"]),
            schemas: names(&["main"]),
            catalogs: names(&["memory"]),
            pragmas: names(&["database_list", "show_tables"]),
            settings: names(&["threads", "memory_limit"]),
            files: names(&["'data.parquet'", "'out.csv'"]),
            variables: names(&["v"]),
        }
    }
}

fn names(from: &[&str]) -> Vec<String> {
    from.iter().map(|name| (*name).to_string()).collect()
}

/// Numbers a `Number` position can be filled with.
///
/// All of them are positive, because a minus sign is a token of its own and a literal carrying one
/// would be two tokens where the grammar asked for one.
const NUMBERS: [&str; 7] = ["0", "1", "2", "42", "1.5", "1e3", "9223372036854775807"];

/// Strings a `String` position can be filled with, including an escaped quote and a character
/// outside ASCII, which are the two shapes a tokenizer gets wrong.
const STRINGS: [&str; 4] = ["'a'", "''", "'it''s'", "'é'"];

/// Operators for the generic `Operator` node, which is only ever the multi character ones.
///
/// Every single character operator in the language is spelled by a rule of its own, and the ones
/// the grammar spells for itself are refused by `OperatorMatcher`, so this is the set that is left.
const OPERATORS: [&str; 5] = ["||", "<<", ">>", "@>", "&&"];

/// A statement writer.
///
/// Cheap to build and cheap to clone, and a run is a seed, so the same generator with the same seed
/// writes the same statement on any machine and in any order.
#[derive(Debug, Clone)]
pub struct Generator {
    catalog: Catalog,
    budget: u32,
    repeats: u32,
}

impl Default for Generator {
    fn default() -> Self {
        Self { catalog: Catalog::default(), budget: BUDGET, repeats: REPEATS }
    }
}

impl Generator {
    /// A generator over the default catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A generator over a catalog somebody else built, which is the case that matters.
    #[must_use]
    pub fn with_catalog(catalog: Catalog) -> Self {
        Self { catalog, ..Self::default() }
    }

    /// How many tokens to write before finishing as cheaply as possible.
    #[must_use]
    pub fn budget(mut self, tokens: u32) -> Self {
        self.budget = tokens.max(1);
        self
    }

    /// How many times one rule may appear on the path from the root before the same thing happens.
    ///
    /// One is every statement written with no nesting in it at all. Three is a couple of levels of
    /// subquery and of arithmetic, which is where the interesting shapes are.
    #[must_use]
    pub fn repeats(mut self, times: u32) -> Self {
        self.repeats = times.max(1);
        self
    }

    /// One statement, from this seed.
    ///
    /// # Panics
    ///
    /// Never, in the sense that matters: the rule it writes from is one of the 1088 in the table
    /// and `every_rule_in_the_table_can_be_written_down` says every one of those has a finite text.
    #[must_use]
    pub fn statement(&self, seed: u64) -> String {
        self.from_rule(START, seed).expect("Statement is a rule")
    }

    /// One piece of a statement, from a named rule.
    ///
    /// For a harness that wants expressions rather than statements, and for the tests, which are
    /// much easier to read over `Expression` than over `Statement`.
    ///
    /// # Errors
    ///
    /// There is no rule with that name, or there is and nothing finite can be written from it.
    pub fn from_rule(&self, rule: &str, seed: u64) -> Result<String> {
        let index = RULES
            .binary_search_by(|candidate| candidate.name.cmp(rule))
            .map_err(|_| Error::parser(format!("no rule named {rule}")))?;
        let root = RULES[index].root;
        if costs()[root as usize] == UNREACHABLE {
            return Err(Error::parser(format!("nothing finite can be written from {rule}")));
        }
        let mut run = Run {
            catalog: &self.catalog,
            random: Random::new(seed),
            pieces: Vec::new(),
            spent: 0,
            budget: self.budget,
            repeats: self.repeats,
            path: vec![0; RULES.len()],
            stack: 0,
            tight: false,
            table: 0,
        };
        run.table = run.random.below(self.catalog.tables.len().max(1));
        run.node(root);
        Ok(run.pieces.join(" "))
    }
}

/// One statement being written.
struct Run<'a> {
    catalog: &'a Catalog,
    random: Random,
    /// The tokens so far, joined with a space at the end. A space between every pair is not
    /// prettiness, it is the only way to be sure two tokens do not become a third: `-` then `-`
    /// written without one is a comment to the end of the line, and `/` then `*` is a comment to
    /// the end of the statement.
    pieces: Vec<String>,
    spent: u32,
    budget: u32,
    repeats: u32,
    /// How many times each rule is on the path from the root to here.
    path: Vec<u32>,
    /// How many rules are on that path, which is what keeps the Rust stack out of it.
    stack: u32,
    /// Whether the rest of this subtree is being finished as cheaply as it can be.
    tight: bool,
    /// Which table of the catalog this statement is about.
    table: usize,
}

impl Run<'_> {
    /// Whether it is time to stop making the statement bigger.
    fn cheap(&self) -> bool {
        self.tight || self.spent >= self.budget
    }

    fn emit(&mut self, text: impl Into<String>) {
        self.pieces.push(text.into());
        self.spent += 1;
    }

    fn node(&mut self, index: u32) {
        let node = NODES[index as usize];
        match node.op {
            Op::Rule => self.rule(node.a, node.b),
            Op::Sequence => {
                for child in node.children() {
                    self.node(*child);
                }
            }
            Op::Choice => {
                let child = self.alternative(node.children(), self.cheap());
                self.node(child);
            }
            Op::Optional => {
                // One in three rather than one in two. An optional is usually a clause and a
                // statement is mostly optionals, so an even coin gives every statement half the
                // clauses in the grammar and nothing else.
                if !self.cheap() && self.random.chance(3) {
                    self.node(node.a);
                }
            }
            Op::Repeat => {
                let times = if self.cheap() { 1 } else { self.random.count(1, 3) };
                for _ in 0..times {
                    self.node(node.a);
                }
            }
            Op::Keyword => {
                let word = KEYWORDS[node.a as usize].0.to_uppercase();
                self.emit(word);
            }
            Op::KeywordClass => self.keyword_in(node.a),
            Op::Symbol => self.emit(SYMBOLS[node.a as usize]),
            Op::Identifier => self.name(SUGGESTIONS[node.a as usize]),
            Op::Number => {
                let number = self.random.pick(&NUMBERS);
                self.emit(number);
            }
            Op::String => {
                let text = self.random.pick(&STRINGS);
                self.emit(text);
            }
            Op::Operator => {
                let operator = self.random.pick(&OPERATORS);
                self.emit(operator);
            }
            // It is the end of the input, so writing anything at all would be wrong.
            Op::EndOfInput => {}
        }
    }

    /// A reference to a rule, which is the only place the walk can go round in a circle.
    ///
    /// The bound is on how many times one rule may be on the path rather than on how long the path
    /// is. A depth bound sounds like the same thing and is not, because the expression grammar is a
    /// chain of about twenty rules, one per precedence level, that a single `a + b` goes all the way
    /// down. A depth of fourteen spends itself somewhere around multiplication and every leaf under
    /// it comes out as whatever the cheapest literal is, which is exactly what the first version of
    /// this did: it wrote several hundred expressions and not one of them contained a column.
    fn rule(&mut self, rule: u32, root: u32) {
        let index = rule as usize;
        let was = self.tight;
        // Sticky, and restored on the way out. Once a subtree is being finished cheaply the whole
        // of it is, because that is what makes the walk terminate: the cheapest expansion of
        // anything is finite, and a subtree that went back to choosing freely could go round again.
        self.tight = was || self.path[index] >= self.repeats || self.stack >= STACK;
        self.path[index] += 1;
        self.stack += 1;
        self.node(root);
        self.stack -= 1;
        self.path[index] -= 1;
        self.tight = was;
    }

    /// Which alternative of a choice to take.
    ///
    /// Cheap means the cheapest, which is what makes the walk terminate. Otherwise the weight is
    /// `16 / (cost + 1)`, so a two token alternative is picked about five times as often as a
    /// fifteen token one. Uniform would be wrong in a way that is easy to miss: the expensive
    /// alternatives in this grammar are the recursive ones, so a fair coin at every choice point
    /// walks down them nearly every time and a generated statement is a hundred nested casts.
    fn alternative(&mut self, children: &[u32], cheap: bool) -> u32 {
        let costs = costs();
        if cheap {
            let mut best = children[0];
            for child in children {
                if costs[*child as usize] < costs[best as usize] {
                    best = *child;
                }
            }
            return best;
        }
        let weights: Vec<u64> =
            children.iter().map(|child| weight(costs[*child as usize])).collect();
        let total: u64 = weights.iter().sum();
        // Every alternative is unreachable, so the choice is too, so nothing picked it and this
        // cannot happen. Taking the first one is still a better answer than dividing by zero.
        if total == 0 {
            return children[0];
        }
        let mut pick = self.random.next() % total;
        for (child, weight) in children.iter().zip(&weights) {
            if pick < *weight {
                return *child;
            }
            pick -= *weight;
        }
        children[children.len() - 1]
    }

    /// A word from one of the five keyword classes.
    fn keyword_in(&mut self, mask: u32) {
        let words = keywords_in(mask as u8);
        if words.is_empty() {
            return;
        }
        let index = self.random.below(words.len());
        let word = KEYWORDS[words[index] as usize].0.to_uppercase();
        self.emit(word);
    }

    /// A name for whichever of the eleven positions the grammar is at.
    fn name(&mut self, suggestion: Suggestion) {
        let catalog = self.catalog;
        let table = catalog.tables.get(self.table);
        let pool = match suggestion {
            Suggestion::TableName => {
                let name = table.map_or("t", |table| table.name.as_str()).to_string();
                self.emit(name);
                return;
            }
            Suggestion::ColumnName => {
                let columns = table.map(|table| table.columns.as_slice()).unwrap_or_default();
                if columns.is_empty() {
                    self.emit("a");
                } else {
                    let index = self.random.below(columns.len());
                    let name = columns[index].clone();
                    self.emit(name);
                }
                return;
            }
            Suggestion::Variable => &catalog.variables,
            Suggestion::ScalarFunctionName => &catalog.functions,
            Suggestion::TableFunctionName => &catalog.table_functions,
            Suggestion::TypeName => &catalog.types,
            Suggestion::SchemaName => &catalog.schemas,
            Suggestion::CatalogName => &catalog.catalogs,
            Suggestion::PragmaName => &catalog.pragmas,
            Suggestion::SettingName => &catalog.settings,
            Suggestion::FileName => &catalog.files,
        };
        if pool.is_empty() {
            self.emit("a");
            return;
        }
        let index = self.random.below(pool.len());
        let name = pool[index].clone();
        self.emit(name);
    }
}

/// How much of a choice's weight an alternative of this cost gets.
fn weight(cost: u32) -> u64 {
    if cost == UNREACHABLE {
        return 0;
    }
    (16 / (u64::from(cost) + 1)).max(1)
}

/// The smallest number of tokens each node can be written down in, computed once for the process.
///
/// A fixpoint rather than a walk, because the grammar is recursive and a walk would not terminate.
/// Costs start at unreachable and only ever fall, and a pass that moves nothing is the answer.
/// Around ten passes settle this table, which is a few hundred microseconds once ever.
fn costs() -> &'static [u32] {
    static COSTS: OnceLock<Box<[u32]>> = OnceLock::new();
    COSTS.get_or_init(build_costs)
}

fn build_costs() -> Box<[u32]> {
    let mut costs = vec![UNREACHABLE; NODES.len()];
    loop {
        let mut moved = false;
        for (index, node) in NODES.iter().enumerate() {
            let value = cost_of(*node, &costs);
            if value < costs[index] {
                costs[index] = value;
                moved = true;
            }
        }
        if !moved {
            return costs.into_boxed_slice();
        }
    }
}

fn cost_of(node: Node, costs: &[u32]) -> u32 {
    match node.op {
        // Matching the end of the input writes nothing, and an optional that is skipped writes
        // nothing either, so both are free and neither can make a node unreachable.
        Op::EndOfInput | Op::Optional => 0,
        Op::Keyword
        | Op::KeywordClass
        | Op::Symbol
        | Op::Identifier
        | Op::Number
        | Op::String
        | Op::Operator => 1,
        Op::Rule => costs[node.b as usize],
        Op::Repeat => costs[node.a as usize],
        Op::Sequence => CHILDREN[node.a as usize..(node.a + node.b) as usize]
            .iter()
            .fold(0, |total, child| total.saturating_add(costs[*child as usize])),
        Op::Choice => CHILDREN[node.a as usize..(node.a + node.b) as usize]
            .iter()
            .map(|child| costs[*child as usize])
            .min()
            .unwrap_or(UNREACHABLE),
    }
}

/// The words in each keyword class, by index into `KEYWORDS`, built once.
///
/// Five classes and 514 words, so this is five short lists and not worth being clever about. The
/// alternative is scanning the whole table on every `KeywordClass` node, and those are the nodes a
/// grammar full of keyword lists is mostly made of.
fn keywords_in(mask: u8) -> &'static [u16] {
    static BY_CLASS: OnceLock<[Vec<u16>; 8]> = OnceLock::new();
    let by_class = BY_CLASS.get_or_init(|| {
        let mut lists: [Vec<u16>; 8] = Default::default();
        for (index, (_, classes)) in KEYWORDS.iter().enumerate() {
            for (bit, list) in lists.iter_mut().enumerate() {
                if classes & (1 << bit) != 0 {
                    list.push(index as u16);
                }
            }
        }
        lists
    });
    // A mask names one class in every node the generator has ever seen, and the lowest set bit is
    // as good an answer as any if that ever stops being true.
    match (0..8).find(|bit| mask & (1 << bit) != 0) {
        Some(bit) => &by_class[bit],
        None => &[],
    }
}

/// A xorshift, so that a seed is the whole of a run.
///
/// Not `rand`. This crate has one dependency and the thing being tested here is a grammar walk, so
/// the quality that matters is that the same seed gives the same statement and not that the bits
/// pass a statistical suite.
struct Random(u64);

impl Random {
    fn new(seed: u64) -> Self {
        // Zero is the one state a xorshift cannot leave, and seed zero is the one a person types.
        Self(seed.wrapping_mul(0x2545_f491_4f6c_dd1d) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }

    fn count(&mut self, low: usize, high: usize) -> usize {
        low + self.below(high - low + 1)
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.next() % one_in == 0
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::{Catalog, Generator, Random, Run, Table, UNREACHABLE, costs};
    use crate::generated::rules::RULES;
    use crate::matcher::parse_from;
    use crate::tokenize::tokenize;

    /// How many seeds the tests that measure a share run over when nothing says otherwise.
    ///
    /// Three thousand statements and three thousand parses are under a second, so the measurement
    /// sits in the ordinary test run. `RUDB_GRAMMAR_SEED` sets where a run starts and
    /// `RUDB_GRAMMAR_SEEDS` how many it does, which is how the numbers in
    /// `spec/sql/duckdb/10-generation-and-fuzzing.md` section 10.2.1 were taken.
    const SEEDS: u64 = 3000;

    fn seeds() -> std::ops::RangeInclusive<u64> {
        let first = setting("RUDB_GRAMMAR_SEED", 1);
        let count = setting("RUDB_GRAMMAR_SEEDS", SEEDS).max(1);
        first..=first.saturating_add(count - 1)
    }

    fn setting(name: &str, fallback: u64) -> u64 {
        match std::env::var(name) {
            Ok(text) => text
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("{name} is {text}, which is not a number")),
            Err(_) => fallback,
        }
    }

    fn statements(generator: &Generator, rule: &str) -> Vec<String> {
        seeds().map(|seed| generator.from_rule(rule, seed).expect("a rule")).collect()
    }

    /// The shortest text a rule can be written as, with no seed in it.
    ///
    /// A run that is tight from the first node takes the cheapest alternative, skips every optional
    /// and goes round every repeat once, so there is nothing left for the random numbers to decide
    /// and the answer is a property of the table.
    fn cheapest(rule: &str) -> String {
        let catalog = Catalog::default();
        let root = RULES[RULES.binary_search_by(|r| r.name.cmp(rule)).expect("a rule")].root;
        let mut run = Run {
            catalog: &catalog,
            random: Random::new(1),
            pieces: Vec::new(),
            spent: 0,
            budget: 1,
            repeats: 1,
            path: vec![0; RULES.len()],
            stack: 0,
            tight: true,
            table: 0,
        };
        run.node(root);
        run.pieces.join(" ")
    }

    #[test]
    fn the_same_seed_writes_the_same_statement() {
        let generator = Generator::new();
        for seed in [1, 2, 99, 10_000] {
            assert_eq!(generator.statement(seed), generator.statement(seed));
        }
        // And two seeds do not, which is the other half of the claim and the one that fails if the
        // seed stops reaching the walk.
        assert_ne!(generator.statement(1), generator.statement(2));
    }

    #[test]
    fn every_rule_in_the_table_can_be_written_down() {
        let costs = costs();
        let unreachable: Vec<&str> = RULES
            .iter()
            .filter(|rule| costs[rule.root as usize] == UNREACHABLE)
            .map(|rule| rule.name)
            .collect();
        assert!(unreachable.is_empty(), "no finite text exists for {unreachable:?}");
    }

    #[test]
    fn the_cheapest_text_a_rule_has_is_as_long_as_the_cost_table_says() {
        // The cost table is the whole termination argument, so it is checked against the walk it is
        // supposed to describe rather than against a second copy of its own arithmetic. A run that
        // is tight from the first node takes the cheapest alternative, skips every optional and
        // goes round every repeat once, which is exactly what the cost of a node is defined as, so
        // the two numbers have to be the same number for all 1088 rules.
        for rule in &RULES {
            let text = cheapest(rule.name);
            let written = if text.is_empty() { 0 } else { text.split(' ').count() as u32 };
            assert_eq!(written, costs()[rule.root as usize], "{} wrote {text:?}", rule.name);
        }
    }

    #[test]
    fn what_it_writes_always_tokenizes() {
        // Weaker than parsing and it holds every time rather than nearly every time, because the
        // pieces are written with a space between every pair and no piece is a fragment of a token.
        // A failure here is the generator writing something that is not text, which is a different
        // and worse thing than writing a statement that does not parse.
        for text in statements(&Generator::new(), "Statement") {
            tokenize(&text).unwrap_or_else(|error| panic!("{text}\n{error}"));
        }
    }

    #[test]
    fn the_generator_writes_statements_that_parse() {
        // Not all of them, and the reason is ordered choice: this writes the fifth alternative of a
        // choice and the matcher settles on the second, which then leaves the rest of the sequence
        // with nothing to match. The share is measured so that it cannot quietly collapse, and the
        // floor is well under what it does today, because the number that matters about a generator
        // is that it keeps working and not that it hits a target.
        let written = statements(&Generator::new(), "Statement");
        let parsed =
            written.iter().filter(|text| parse_from(text, "Statement", true).is_ok()).count();
        let total = written.len();
        // Printed rather than only asserted, because this is where the share in section 10.2.1 of
        // `spec/sql/duckdb/10-generation-and-fuzzing.md` comes from: run the test with a seed count
        // and `--nocapture` and the number it prints is the number that goes in the document.
        println!("{parsed} of {total} statements parse");
        assert!(parsed * 100 / total >= 85, "{parsed} of {total} parse");
    }

    #[test]
    fn the_filter_and_the_walk_without_it_agree_on_what_it_writes() {
        // The FIRST filter claims to be a superset, so turning it off can only ever make the
        // matcher slower and never make it accept more. Generated statements are the widest set of
        // inputs there is to ask that over, and they reach parts of the rule table no corpus does.
        for text in statements(&Generator::new(), "Statement") {
            let filtered = parse_from(&text, "Statement", true);
            let whole = parse_from(&text, "Statement", false);
            assert_eq!(filtered.is_ok(), whole.is_ok(), "the filter changed the answer on {text}");
        }
    }

    #[test]
    fn the_names_it_writes_are_the_catalog_it_was_handed() {
        // Every word it writes is either a keyword, which comes out upper case, or a name, which
        // comes out exactly as the catalog spelled it. So a lower case word that is not in the
        // catalog is a name the generator invented, and a generator that invents names is testing
        // the tokenizer rather than the binder.
        let catalog = Catalog {
            tables: vec![Table { name: "zork".into(), columns: vec!["quux".into()] }],
            functions: vec!["frob".into()],
            ..Catalog::default()
        };
        let generator = Generator::with_catalog(catalog.clone());
        let written = statements(&generator, "Statement");
        let total = written.len();
        let mut seen_a_column = false;
        for text in written {
            for piece in text.split(' ') {
                if !piece.starts_with(|first: char| first.is_ascii_lowercase()) {
                    continue;
                }
                seen_a_column |= piece == "quux";
                assert!(known(&catalog, piece), "{piece} is not a name the catalog has, in {text}");
            }
        }
        assert!(seen_a_column, "no statement in {total} mentioned a column");
    }

    fn known(catalog: &Catalog, piece: &str) -> bool {
        catalog
            .tables
            .iter()
            .any(|table| table.name == piece || table.columns.iter().any(|column| column == piece))
            || [
                &catalog.functions,
                &catalog.table_functions,
                &catalog.types,
                &catalog.schemas,
                &catalog.catalogs,
                &catalog.pragmas,
                &catalog.settings,
                &catalog.files,
                &catalog.variables,
            ]
            .iter()
            .any(|pool| pool.iter().any(|name| name == piece))
    }

    #[test]
    fn a_query_is_what_comes_out_of_the_rule_that_writes_queries() {
        // `Statement` is a choice of thirty six and a query is one of them, so a caller that wants
        // queries asks for the rule that writes them rather than filtering what it gets.
        let generator = Generator::new();
        let queries = statements(&generator, "SelectStatement");
        let parsed =
            queries.iter().filter(|text| parse_from(text, "SelectStatement", true).is_ok()).count();
        let total = queries.len();
        println!("{parsed} of {total} queries parse");
        assert!(parsed * 100 / total >= 80, "{parsed} of {total} parse");
        assert!(queries.iter().any(|text| text.contains("SELECT")));
    }

    #[test]
    fn a_budget_of_one_token_still_writes_a_whole_statement() {
        // The budget is where to stop growing and not where to stop, so what comes out is the
        // cheapest whole statement the grammar has rather than a truncated one.
        // One word, and it is a query, because `SELECT` on its own is a statement in this dialect
        // and a tie in the cost table goes to the alternative the grammar writes first.
        assert_eq!(cheapest("Statement"), "SELECT");
        assert!(parse_from("SELECT", "Statement", true).is_ok());
    }

    #[test]
    fn the_shortest_explain_there_is_does_not_parse_and_that_is_the_grammar() {
        // Worth one test of its own, because it is the clearest example of why the share that
        // parses is not one. The cheapest thing `EXPLAIN` can be followed by is the `ANALYZE`
        // statement, so the shortest explain in the grammar is those two words. The matcher reads
        // them the other way round: `ExplainStatement <- 'EXPLAIN' AnalyzeKeyword? ...` takes the
        // word as the optional keyword, nothing is left for the statement that has to follow, and a
        // PEG does not give a taken optional back. DuckDB's parser is this grammar and this
        // algorithm, so it refuses the same two words for the same reason.
        assert_eq!(cheapest("ExplainStatement"), "EXPLAIN ANALYZE");
        assert!(parse_from("EXPLAIN ANALYZE", "ExplainStatement", true).is_err());
        assert!(parse_from("EXPLAIN ANALYZE SELECT 1", "Statement", true).is_ok());
    }

    #[test]
    fn an_unknown_rule_says_so() {
        let error = Generator::new().from_rule("NoSuchRule", 1).unwrap_err();
        assert!(error.to_string().contains("no rule named NoSuchRule"), "{error}");
    }
}
