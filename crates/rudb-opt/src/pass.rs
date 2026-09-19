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
//! of them and rudb has built seven, so most of that list is a name rudb does not answer to yet
//! rather than a name it disagrees about. [`crate::UPSTREAM`] holds all forty four and the setting
//! takes every one of them, because turning off a pass that is not there is a request that has
//! already been granted and refusing it would fail the statement and end the file.

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
/// The settings and the facts. The catalog and the planning deadline are the other two things
/// `spec/engine/11-optimizer.md` puts in here, and each arrives with the first pass that reads it:
/// the deadline with join ordering, which is the only search in the plan and so the only thing
/// that can spend real time. A field that no pass reads is a field whose meaning nobody has had to
/// decide yet, and deciding it early is how it ends up wrong.
///
/// The facts are a shared set of counts rather than a handle on the catalog, which keeps a lifetime
/// out of this type and out of everything that builds one. What it costs is that a context built
/// before a table grows plans against the size the table was, and a context is built per statement,
/// so the window is one statement wide.
///
/// They are shared rather than copied, and the version they were read at is the thing that says
/// whether they are still current. A plan is a function of exactly one generation of the catalog,
/// which is what `spec/stats/04-in-memory.md` asks for, and it is also what lets two statements over
/// an unchanged catalog plan from the same set instead of walking every table and column twice. See
/// [`crate::estimate::Facts::generation`].
#[derive(Debug, Clone, Default)]
pub struct Context {
    disabled: Vec<&'static str>,
    facts: std::sync::Arc<crate::estimate::Facts>,
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
    /// The name is matched without regard to case, because DuckDB matches it that way and two
    /// corpus files say `LATE_MATERIALIZATION` in capitals.
    ///
    /// A name in [`crate::UPSTREAM`] that rudb has not built is accepted and does nothing. That is
    /// not leniency for its own sake. Turning off a pass that is not there is a request that has
    /// already been granted, and the alternative is a `SET` that fails and takes the rest of the
    /// file with it.
    ///
    /// # Errors
    ///
    /// For a name that is not an optimizer anywhere.
    pub fn disable(&mut self, name: &str) -> Result<()> {
        let name = name.to_ascii_lowercase();
        let local = crate::PASSES.iter().find(|pass| pass.name() == name);
        if !crate::UPSTREAM.contains(&name.as_str()) && local.is_none() {
            // Every accepted name rather than the closest one by edit distance, which is what the
            // binary prints. Listing a set that is not the set the caller may choose from would be
            // worse than listing a long one, and the accepted set is now the same forty four either
            // engine takes.
            let mut names = crate::UPSTREAM.to_vec();
            names.extend(crate::PASSES.iter().map(|pass| pass.name()));
            names.sort_unstable();
            names.dedup();
            let known: Vec<String> = names.iter().map(|known| format!("\"{known}\"")).collect();
            return Err(Error::parser(format!(
                "Optimizer type \"{name}\" not recognized\n\nCandidate optimizers: {}",
                known.join(", ")
            )));
        }
        let Some(found) = local else {
            return Ok(());
        };
        if !self.is_disabled(&name) {
            self.disabled.push(found.name());
        }
        Ok(())
    }

    /// The setting text a list of names reads back as, which is not the text that was written.
    ///
    /// DuckDB stores what it understood rather than what it was given, so `' TOP_N , join_order '`
    /// reads back as `join_order,top_n`: trimmed, lowercased, deduplicated, sorted and joined by
    /// commas, with the empty entries a trailing comma leaves gone. A caller that stored the text
    /// as written would answer `SELECT current_setting('disabled_optimizers')` differently from the
    /// binary for every spelling but the tidy one.
    ///
    /// # Errors
    ///
    /// For a name that is not an optimizer anywhere, the same complaint [`Self::disable`] makes,
    /// because this is the validating read and the two have to agree about what is a name.
    pub fn tidy(names: &str) -> Result<String> {
        let mut kept: Vec<String> = Vec::new();
        for name in names.split(',') {
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            Self::new().disable(&name)?;
            if !kept.contains(&name) {
                kept.push(name);
            }
        }
        kept.sort();
        Ok(kept.join(","))
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
    ///
    /// It takes the set by handle rather than by value so that the caller can keep the one it built
    /// and hand the same one to the next statement. The set itself is never written to after it is
    /// built, which is what makes sharing it safe to do without a lock.
    pub fn measure(&mut self, facts: std::sync::Arc<crate::estimate::Facts>) {
        self.facts = facts;
    }

    /// What is known about how large the tables are.
    #[must_use]
    pub fn facts(&self) -> &crate::estimate::Facts {
        &self.facts
    }
}

/// Every node the root reaches, parents before children.
///
/// Not every node in the arena. A rewrite that replaced a node leaves the old one behind, and a
/// pass that walked the arena would go on rewriting nodes that nothing runs, which costs time on
/// every later pass and can report an error about a plan nobody asked about.
///
/// Parents before children is the whole point for [`crate::columns`], which narrows a node to what
/// everything above it reads and so needs everything above it to have been read first. A plan is a
/// graph and not a tree, because a rewrite that wants a subtree twice points at it twice rather than
/// copying it, and breadth first order does not give that: a node two parents reach at different
/// depths comes out after the nearer one and before the further one. Sorting does give it. A node's
/// children are behind it, which [`Plan::validate`] enforces and says so with the node number, so
/// walking the reachable nodes from the largest reference down visits every parent of a node before
/// the node itself however many parents it has.
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
    found.sort_unstable_by(|left, right| right.cmp(left));
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
    fn a_name_duckdb_has_and_rudb_has_not_built_turns_nothing_off_and_is_not_an_error() {
        let context =
            Context::without("statistics_propagation,unused_columns").expect("both are names");
        assert!(context.is_disabled("unused_columns"));
        assert!(
            !context.is_disabled("statistics_propagation"),
            "there is no such pass to have turned off"
        );
    }

    #[test]
    fn the_name_is_matched_without_regard_to_case() {
        let context = Context::without("UNUSED_COLUMNS").expect("a name in capitals");
        assert!(context.is_disabled("unused_columns"));
    }

    #[test]
    fn the_tidy_text_is_trimmed_lowered_deduplicated_and_sorted() {
        assert_eq!(
            Context::tidy(" TOP_N , join_order , top_n ,").expect("three names and a comma"),
            "join_order,top_n"
        );
        assert_eq!(Context::tidy("").expect("nothing is nothing"), "");
        assert_eq!(Context::tidy(" , ").expect("still nothing"), "");
    }

    #[test]
    fn the_tidy_text_complains_about_the_same_names_the_toggle_does() {
        let error = Context::tidy("top_n,bogus").expect_err("not a pass");
        assert_eq!(error.code().duckdb_name(), "Parser Error");
        assert!(
            error.message().starts_with("Optimizer type \"bogus\" not recognized"),
            "{}",
            error.message()
        );
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
