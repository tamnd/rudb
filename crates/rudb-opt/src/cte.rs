//! Dropping a materialisation nothing reads.
//!
//! `WITH name AS MATERIALIZED (...)` says the query is run once and the rows are held, and the one
//! decision left after that is whether it is run at all. The reference binary makes it here, and it
//! is not a guess: `WITH c AS MATERIALIZED (SELECT error('boom')) SELECT 42` answers 42 on the
//! pinned build, where naming `c` in the body raises `boom`. The definition is gone from `EXPLAIN`
//! too, so it is dropped rather than run into nothing.
//!
//! This is the whole of what the optimizer decides about a `WITH` here. Whether the rows are held
//! or the query is put into each place it is named is settled in the parser from the word the
//! person wrote, because that is where the reference binary settles it as well.
//!
//! The walk is bottom up, which is not a detail. Dropping a materialisation takes its definition
//! with it, and a definition may be the only thing that reads an outer one, so `WITH a AS
//! MATERIALIZED (...), b AS MATERIALIZED (SELECT * FROM a) SELECT 1` has to lose both in one run.
//! Top down would lose `b`, leave `a` behind because the read it was looking at was inside `b`, and
//! then drop `a` on a second run, which is the fixed sequence not settling.

use rudb_common::Result;
use rudb_plan::{Node, NodeRef, Plan};

use crate::pass::{Context, Pass};
use crate::walk::restack;

/// Removes every materialised `WITH` the query that reads it never names.
#[derive(Debug, Clone, Copy)]
pub struct UnusedMaterialization;

impl Pass for UnusedMaterialization {
    fn name(&self) -> &'static str {
        "materialized_cte"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        drop_unread(plan);
        Ok(())
    }
}

/// Removes every materialisation in `plan` that nothing under its body reads.
pub fn drop_unread(plan: &mut Plan) {
    let mut changed = false;
    let root = restack(plan, plan.root(), &mut changed, &mut |plan, at| {
        let Node::MaterializedCte { body, cte, .. } = *plan.node(at) else { return None };
        if reads(plan, body, cte) {
            return None;
        }
        Some(body)
    });
    plan.set_root(root);
}

/// Whether anything under `at` reads the materialisation numbered `cte`.
///
/// The number rather than the name, because the name is what was written and two materialisations
/// in one plan are allowed to have been written with the same one.
fn reads(plan: &Plan, at: NodeRef, cte: u32) -> bool {
    if let Node::CteScan { cte: read, .. } = *plan.node(at) {
        return read == cte;
    }
    plan.node(at).children().into_iter().flatten().any(|child| reads(plan, child, cte))
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::drop_unread;

    /// The plan that comes back from running the pass over a written one.
    fn dropped(text: &str) -> String {
        let mut plan = Plan::parse(text).expect("a plan the reader accepts");
        drop_unread(&mut plan);
        plan.to_string()
    }

    #[test]
    fn a_materialisation_the_body_reads_stays() {
        let text = dropped(
            "MaterializedCte c @0 [n::INTEGER]\n  \
               Values #0 [n::INTEGER] rows=[[1::INTEGER]]\n  \
               Project #2 [#1.0::INTEGER AS n]\n    \
                 CteScan c @0 #1 [n::INTEGER]\n",
        );
        assert!(text.contains("MaterializedCte c @0"), "{text}");
        assert!(text.contains("CteScan c @0"), "{text}");
    }

    #[test]
    fn a_materialisation_nothing_reads_goes_and_takes_its_definition_with_it() {
        let text = dropped(
            "MaterializedCte c @0 [n::INTEGER]\n  \
               Values #0 [n::INTEGER] rows=[[1::INTEGER]]\n  \
               Project #2 [2::INTEGER AS two]\n    \
                 Dummy\n",
        );
        assert_eq!(text, "Project #2 [2::INTEGER AS two]\n  Dummy\n", "{text}");
    }

    #[test]
    fn one_run_drops_a_pair_where_the_only_read_was_in_the_other_definition() {
        let text = dropped(
            "MaterializedCte a @0 [n::INTEGER]\n  \
               Values #0 [n::INTEGER] rows=[[1::INTEGER]]\n  \
               MaterializedCte b @1 [n::INTEGER]\n    \
                 Project #3 [#2.0::INTEGER AS n]\n      \
                   CteScan a @0 #2 [n::INTEGER]\n    \
                 Project #4 [2::INTEGER AS two]\n      \
                   Dummy\n",
        );
        assert_eq!(text, "Project #4 [2::INTEGER AS two]\n  Dummy\n", "{text}");
    }
}
