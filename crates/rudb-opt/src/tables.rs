//! Which tables an expression reads, and which ones an operator produces.
//!
//! The first of the four expression analyses `spec/engine/11-optimizer.md` asks for. Every question
//! the optimizer has about a join is really this question: a predicate can be pushed into a side
//! when everything it reads is produced by that side, two relations have an edge between them when a
//! condition reads both, and a join is a cross product when no condition reads both. All three are
//! set containment, so the analysis is a set and the rules that use it are one line each.
//!
//! A bitset rather than a list of indices, because the operation these rules do is containment and
//! not iteration. Table indices are handed out by the binder in order from zero, so a query's
//! indices are dense and a bitset over them is as wide as the query is large rather than as wide as
//! the largest index. The words grow rather than being one `u64`, because the binder gives an index
//! to every operator that introduces columns and not only to every table, so a query with seventy
//! projections in it has a table index past sixty four and is not a query anybody would call large.

use std::collections::HashMap;

use rudb_plan::{Expr, ExprRef, Node, NodeRef, Plan};

/// A set of table indices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableSet {
    words: Vec<u64>,
}

impl TableSet {
    /// The empty set, which is what a constant reads.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The set holding one index.
    #[must_use]
    pub fn of(index: u32) -> Self {
        let mut set = Self::new();
        set.insert(index);
        set
    }

    /// Adds an index.
    pub fn insert(&mut self, index: u32) {
        let (word, bit) = place(index);
        if self.words.len() <= word {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= bit;
    }

    /// Whether the index is in the set.
    #[must_use]
    pub fn contains(&self, index: u32) -> bool {
        let (word, bit) = place(index);
        self.words.get(word).is_some_and(|held| held & bit != 0)
    }

    /// Whether the set holds nothing, which is what a predicate over no column reads.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    /// Adds everything in `other`.
    pub fn extend(&mut self, other: &Self) {
        if self.words.len() < other.words.len() {
            self.words.resize(other.words.len(), 0);
        }
        for (held, word) in self.words.iter_mut().zip(&other.words) {
            *held |= word;
        }
    }

    /// Whether everything in this set is also in `other`.
    ///
    /// The question filter pushdown asks of every predicate at every join. An empty set is a subset
    /// of everything, which is the right answer for a predicate that reads no column: it gives the
    /// same answer wherever it is evaluated.
    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        self.words
            .iter()
            .enumerate()
            .all(|(at, word)| word & !other.words.get(at).copied().unwrap_or(0) == 0)
    }
}

/// Which word holds an index and which bit of it.
fn place(index: u32) -> (usize, u64) {
    let index = index as usize;
    (index / 64, 1 << (index % 64))
}

/// The table indices an expression reads, worked out once per expression.
///
/// Cached because the passes after this one ask the same question of the same expression many
/// times: join ordering asks it of every condition once per subset it enumerates, which is the one
/// place in the optimizer where a repeated walk of an expression tree would show up in a profile.
/// Filter pushdown asks once per predicate per join and would be fine without it.
#[derive(Debug, Default)]
pub struct Tables {
    known: HashMap<ExprRef, TableSet>,
}

impl Tables {
    /// A cache with nothing in it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The table indices `expr` reads.
    ///
    /// Handed back by value rather than by reference, since the recursion needs the cache back
    /// before it can union what the operands answered, and a set is one word for every sixty four
    /// tables in the query.
    pub fn of(&mut self, plan: &Plan, expr: ExprRef) -> TableSet {
        if let Some(known) = self.known.get(&expr) {
            return known.clone();
        }
        let mut set = TableSet::new();
        match *plan.expr(expr) {
            Expr::Column(binding) => set.insert(binding.table),
            Expr::Constant(_) => {}
            Expr::Cast { input, .. } => set.extend(&self.of(plan, input)),
            Expr::Compare { left, right, .. } => {
                set.extend(&self.of(plan, left));
                set.extend(&self.of(plan, right));
            }
            Expr::Conjunction { children, .. } | Expr::Function { args: children, .. } => {
                for child in plan.expr_list(children).to_vec() {
                    set.extend(&self.of(plan, child));
                }
            }
            Expr::Aggregate { args, filter, .. } => {
                for arg in plan.expr_list(args).to_vec() {
                    set.extend(&self.of(plan, arg));
                }
                if let Some(inner) = filter {
                    set.extend(&self.of(plan, inner));
                }
            }
            Expr::Case { arms, otherwise } => {
                for arm in plan.arm_list(arms).to_vec() {
                    set.extend(&self.of(plan, arm.when));
                    set.extend(&self.of(plan, arm.then));
                }
                if let Some(inner) = otherwise {
                    set.extend(&self.of(plan, inner));
                }
            }
        }
        self.known.insert(expr, set.clone());
        set
    }
}

/// The table indices the subtree under `at` produces.
///
/// A projection, a grouping and a set operation stop the walk, because each of them introduces its
/// own index and nothing above it can name what is underneath. That is what makes the answer a set
/// of what is visible rather than a set of everything down there.
#[must_use]
pub fn produced(plan: &Plan, at: NodeRef) -> TableSet {
    let mut set = TableSet::new();
    collect(plan, at, &mut set);
    set
}

/// Adds what the subtree under `at` produces to `set`.
fn collect(plan: &Plan, at: NodeRef, set: &mut TableSet) {
    match *plan.node(at) {
        Node::Get { index, .. }
        | Node::Values { index, .. }
        | Node::TableFunction { index, .. }
        | Node::Project { index, .. }
        | Node::Aggregate { index, .. }
        | Node::SetOp { index, .. } => set.insert(index),
        Node::Dummy => {}
        Node::Filter { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::Distinct { input, .. } => collect(plan, input, set),
        Node::Join { left, right, .. } | Node::CrossProduct { left, right } => {
            collect(plan, left, set);
            collect(plan, right, set);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TableSet, Tables, produced};
    use rudb_plan::Plan;

    #[test]
    fn a_set_holds_what_was_put_in_it_and_grows_past_one_word() {
        let mut set = TableSet::new();
        assert!(set.is_empty());
        set.insert(0);
        set.insert(200);
        assert!(set.contains(0));
        assert!(set.contains(200));
        assert!(!set.contains(1));
        assert!(!set.contains(199));
        assert!(!set.is_empty());
    }

    #[test]
    fn the_empty_set_is_a_subset_of_everything_including_itself() {
        let empty = TableSet::new();
        assert!(empty.is_subset_of(&empty));
        assert!(empty.is_subset_of(&TableSet::of(3)));
        assert!(!TableSet::of(3).is_subset_of(&empty));
    }

    #[test]
    fn containment_holds_across_the_word_boundary() {
        let mut wide = TableSet::of(1);
        wide.insert(100);
        assert!(TableSet::of(100).is_subset_of(&wide));
        assert!(!TableSet::of(101).is_subset_of(&wide));
        assert!(wide.is_subset_of(&wide));

        let mut narrow = TableSet::of(1);
        assert!(narrow.is_subset_of(&wide));
        narrow.extend(&TableSet::of(100));
        assert_eq!(narrow, wide);
    }

    const JOIN: &str = "\
Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
  Get memory.main.t AS a #0 [a::INTEGER]
  Get memory.main.t AS b #1 [a::INTEGER]
";

    #[test]
    fn a_condition_that_reads_both_sides_reads_both_table_indices() {
        let plan = Plan::parse(JOIN).expect("a join");
        let condition = match *plan.node(plan.root()) {
            rudb_plan::Node::Join { conditions, .. } => plan.expr_list(conditions)[0],
            _ => unreachable!("the root is the join"),
        };
        let mut tables = Tables::new();
        let read = tables.of(&plan, condition);
        assert!(read.contains(0));
        assert!(read.contains(1));
        // And a second ask is the cached answer rather than a second walk.
        assert_eq!(tables.of(&plan, condition), read);
    }

    #[test]
    fn a_join_produces_both_sides_and_a_projection_produces_only_itself() {
        let plan = Plan::parse(JOIN).expect("a join");
        let both = produced(&plan, plan.root());
        assert!(both.contains(0));
        assert!(both.contains(1));

        let text = "\
Project #2 [#0.0::INTEGER AS x]
  Get memory.main.t AS t #0 [a::INTEGER]
";
        let plan = Plan::parse(text).expect("a projection");
        let visible = produced(&plan, plan.root());
        assert!(visible.contains(2));
        assert!(!visible.contains(0), "nothing above a projection can name what is under it");
    }
}
