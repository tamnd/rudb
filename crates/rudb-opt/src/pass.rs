//! What a rewrite is, and what it is given besides the plan.
//!
//! `spec/09-optimizer.md` section 9.1 asks for a fixed sequence of passes, each one toggleable by
//! name. The sequence is [`crate::PASSES`] and the toggle is [`Context`]. Both of those need more
//! than one pass to mean anything, which is why neither of them existed while column pruning was
//! the only rewrite: a trait with one implementor is a description of that implementor and a
//! pipeline of one is a function call.
//!
//! The names are DuckDB's, because `SET disabled_optimizers = 'filter_pushdown'` appears in corpus
//! files that were written against DuckDB and a corpus file that turns a pass off has to turn off
//! the pass it meant. `SELECT name FROM duckdb_optimizers()` on the pinned binary lists forty four
//! of them and rudb has two, so most of that list is a name rudb does not answer to yet rather
//! than a name it disagrees about.

use std::collections::VecDeque;

use rudb_common::{Error, Result};
use rudb_plan::{NodeRef, Plan};

/// One rewrite from a plan to a plan.
///
/// Every pass preserves the plan invariant and the width of the root, which [`crate::optimize`]
/// checks once at the end rather than each pass checking itself.
///
/// A pass is a unit struct rather than a closure because it has a name, and the name is what the
/// toggle, the per pass corpus sweep and the bisector all address it by. `spec/engine/11-optimizer.md`
/// section 11.9 makes the bisector a binary search over the set of names, which needs the set to be
/// a value rather than a position in a list.
pub trait Pass {
    /// What this pass is called, in DuckDB's spelling.
    fn name(&self) -> &'static str;

    /// Rewrites the plan in place.
    ///
    /// # Errors
    ///
    /// Anything the pass cannot carry on past. A pass that merely cannot improve a plan leaves it
    /// alone and reports success, because "there was nothing to do" and "this query is broken" are
    /// not the same answer.
    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()>;
}

/// What the passes are given besides the plan.
///
/// The settings and the statistics. The catalog and the planning deadline are the other two things
/// `spec/engine/11-optimizer.md` puts in here, and each arrives with the first pass that reads it:
/// the deadline with join ordering, which is the only search in the plan and so the only thing
/// that can spend real time. A field that no pass reads is a field whose meaning nobody has had to
/// decide yet, and deciding it early is how it ends up wrong.
///
/// The statistics are a copy of the row counts rather than a handle on the catalog, which keeps a
/// lifetime out of this type and out of everything that builds one. What it costs is that a
/// context built before a table grows estimates against the size the table was, and a context is
/// built per statement, so the window is one statement wide.
#[derive(Debug, Clone, Default)]
pub struct Context {
    disabled: Vec<&'static str>,
    statistics: crate::estimate::Statistics,
}

impl Context {
    /// Every pass enabled, which is what a query gets unless it says otherwise.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Turns off the passes named in DuckDB's comma separated spelling.
    ///
    /// Empty entries are skipped, so a trailing comma is not an error, which is what the binary
    /// does with one.
    ///
    /// # Errors
    ///
    /// For a name that is not a pass, with the sentence the binary prints for one. rudb lists
    /// every name it has rather than the closest one by edit distance, which is the same
    /// divergence it already has on every other complaint about a name it does not know.
    pub fn without(names: &str) -> Result<Self> {
        let mut context = Self::new();
        for name in names.split(',') {
            let name = name.trim();
            if !name.is_empty() {
                context.disable(name)?;
            }
        }
        Ok(context)
    }

    /// Turns off one pass by name.
    ///
    /// # Errors
    ///
    /// For a name that is not a pass.
    pub fn disable(&mut self, name: &str) -> Result<()> {
        let Some(found) = crate::PASSES.iter().find(|pass| pass.name() == name) else {
            let known: Vec<String> =
                crate::PASSES.iter().map(|pass| format!("\"{}\"", pass.name())).collect();
            return Err(Error::parser(format!(
                "Optimizer type \"{name}\" not recognized\n\nCandidate optimizers: {}",
                known.join(", ")
            )));
        };
        if !self.is_disabled(name) {
            self.disabled.push(found.name());
        }
        Ok(())
    }

    /// Whether the pass by that name has been turned off.
    #[must_use]
    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled.contains(&name)
    }

    /// Hands the optimizer what is known about how large the tables are.
    ///
    /// Whoever builds the context does this, because the catalog lives a layer above the optimizer
    /// and is not going to be reached from inside it. A context nobody told is a context that
    /// estimates nothing, which is the right answer for the optimizer's own tests and for a plan
    /// that arrived as text.
    pub fn measure(&mut self, statistics: crate::estimate::Statistics) {
        self.statistics = statistics;
    }

    /// What is known about how large the tables are.
    #[must_use]
    pub fn statistics(&self) -> &crate::estimate::Statistics {
        &self.statistics
    }
}

/// Every node the root reaches, parents before children.
///
/// Not every node in the arena. A rewrite that replaced a node leaves the old one behind, and a
/// pass that walked the arena would go on rewriting nodes that nothing runs, which costs time on
/// every later pass and can report an error about a plan nobody asked about.
pub(crate) fn top_down(plan: &Plan) -> Vec<NodeRef> {
    let mut found = Vec::new();
    let mut pending = VecDeque::from([plan.root()]);
    while let Some(node) = pending.pop_front() {
        if found.contains(&node) {
            continue;
        }
        found.push(node);
        pending.extend(plan.node(node).children().into_iter().flatten());
    }
    found
}

#[cfg(test)]
mod tests {
    use super::Context;

    #[test]
    fn every_pass_is_on_unless_it_is_named() {
        let context = Context::new();
        assert!(!context.is_disabled("expression_rewriter"));
        let context = Context::without("expression_rewriter").expect("a name that is a pass");
        assert!(context.is_disabled("expression_rewriter"));
        assert!(!context.is_disabled("unused_columns"));
    }

    #[test]
    fn a_list_turns_off_each_of_them_and_a_trailing_comma_is_not_an_error() {
        let context = Context::without("expression_rewriter, unused_columns,")
            .expect("two names and a comma");
        assert!(context.is_disabled("expression_rewriter"));
        assert!(context.is_disabled("unused_columns"));
    }

    #[test]
    fn naming_the_same_pass_twice_is_naming_it_once() {
        let context = Context::without("unused_columns,unused_columns").expect("the same name");
        assert!(context.is_disabled("unused_columns"));
    }

    #[test]
    fn a_name_that_is_not_a_pass_is_the_error_duckdb_prints() {
        let error = Context::without("bogus").expect_err("not a pass");
        assert_eq!(error.code().duckdb_name(), "Parser Error");
        assert!(
            error.message().starts_with("Optimizer type \"bogus\" not recognized"),
            "{}",
            error.message()
        );
        assert!(error.message().contains("Candidate optimizers:"), "{}", error.message());
    }
}
