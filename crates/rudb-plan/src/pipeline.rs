//! Which pipeline each node of a plan runs in, and which pipeline waits for which.
//!
//! A pipeline is a run of operators from a source to a sink, and a plan breaks into several of them
//! wherever an operator has to see all of its input before it produces anything. A sort is the
//! plain case: everything under it is one pipeline that ends in the sort, and what reads the sorted
//! rows back is the next one, which cannot start until the first has finished. A join is two below
//! the one above it, because the side that is gathered has to be complete before the side that
//! probes it can run a single row.
//!
//! # Why this is here
//!
//! Two crates need the same answer and neither can see the other. `rudb-exec` needs it to number
//! the operators it builds, and `rudb-opt` needs it to print what `EXPLAIN` shows. Written twice it
//! would be right twice on the day it was written and wrong once some time after that, and the
//! version that would be wrong is the printed one, which is the version somebody reads when they
//! are trying to understand why a query is slow.
//!
//! It is physical knowledge about a logical tree, which is worth saying out loud. Whether an
//! operator is a pipeline breaker is a fact about how it is executed rather than about what it
//! means, and the reason it can live here anyway is that at this milestone the physical plan is the
//! logical plan with different words on it, which `crates/rudb-exec/src/build.rs` says at the top.
//! The day there is a physical plan this moves onto it and every caller keeps its call.
//!
//! # The rule
//!
//! The root of the plan produces into pipeline 0. Walking down from there, a node inherits the
//! pipeline of its parent, except that
//!
//! - an aggregate, a sort, a top n and a distinct are sinks, so the node and everything under it
//!   are a new pipeline that the parent's waits for,
//! - a join and a set operation are two, the side that is gathered first and the side that reads
//!   it, with the second waiting for the first and the parent's waiting for the second,
//! - a cross product keeps its left side and itself in the parent's pipeline, because the product
//!   is produced a chunk at a time and never held, and puts its right side in a new one, because
//!   that side is kept whole to be replayed.
//!
//! There is no scheduler reading any of this yet. It is written down because it is known, and an
//! edge reconstructed later from a tree somebody has already flattened is an edge somebody has to
//! guess at.

use crate::node::Node;
use crate::plan::Plan;
use crate::{NodeRef, PipelineRef};

/// The pipelines a plan breaks into.
#[derive(Debug, Clone)]
pub struct Pipelines {
    /// The pipeline each node in the arena runs in, or none for a node the root does not reach.
    of: Vec<Option<PipelineRef>>,
    /// What each pipeline waits for, indexed by pipeline.
    waits: Vec<Vec<PipelineRef>>,
}

impl Pipelines {
    /// Works out the decomposition of a plan.
    #[must_use]
    pub fn of(plan: &Plan) -> Self {
        let mut pipelines = Self { of: vec![None; plan.node_count()], waits: vec![Vec::new()] };
        pipelines.walk(plan, plan.root(), ROOT);
        pipelines
    }

    /// How many pipelines there are, which is at least one.
    #[must_use]
    pub fn len(&self) -> usize {
        self.waits.len()
    }

    /// Never true, and here because a length without one reads as an oversight.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.waits.is_empty()
    }

    /// The pipeline this node runs in.
    ///
    /// For a sink that is the pipeline it ends rather than the one above it, so a sort is in the
    /// pipeline that feeds it and the operator that reads the sorted rows is in the one above.
    ///
    /// # Panics
    ///
    /// If the node is not reachable from the plan's root, which is a node the arena is still
    /// holding after a rewrite replaced it.
    #[must_use]
    pub fn pipeline(&self, node: NodeRef) -> PipelineRef {
        self.of[node as usize].expect("a node under the root of the plan it was walked from")
    }

    /// What this pipeline has to wait for, in ascending order.
    ///
    /// # Panics
    ///
    /// If there is no such pipeline.
    #[must_use]
    pub fn waits_for(&self, pipeline: PipelineRef) -> &[PipelineRef] {
        &self.waits[pipeline as usize]
    }

    /// Every pipeline, from the root's outwards.
    pub fn all(&self) -> impl Iterator<Item = PipelineRef> {
        0..u32::try_from(self.waits.len()).unwrap_or(u32::MAX)
    }

    /// A new pipeline that nothing waits for yet.
    fn fresh(&mut self) -> PipelineRef {
        self.waits.push(Vec::new());
        u32::try_from(self.waits.len() - 1).unwrap_or(u32::MAX)
    }

    /// Records that `pipeline` cannot start until `on` has finished.
    fn waits_on(&mut self, pipeline: PipelineRef, on: PipelineRef) {
        self.waits[pipeline as usize].push(on);
    }

    fn walk(&mut self, plan: &Plan, node: NodeRef, pipeline: PipelineRef) {
        match *plan.node(node) {
            Node::Aggregate { input, .. }
            | Node::Sort { input, .. }
            | Node::TopN { input, .. }
            | Node::Distinct { input, .. } => {
                let below = self.fresh();
                self.waits_on(pipeline, below);
                self.of[node as usize] = Some(below);
                self.walk(plan, input, below);
            }
            Node::Join { left, right, .. } | Node::SetOp { left, right, .. } => {
                let first = self.fresh();
                let second = self.fresh();
                self.waits_on(second, first);
                self.waits_on(pipeline, second);
                self.of[node as usize] = Some(second);
                self.walk(plan, right, first);
                self.walk(plan, left, second);
            }
            Node::CrossProduct { left, right } => {
                let aside = self.fresh();
                self.waits_on(pipeline, aside);
                self.of[node as usize] = Some(pipeline);
                self.walk(plan, right, aside);
                self.walk(plan, left, pipeline);
            }
            ref other => {
                self.of[node as usize] = Some(pipeline);
                for child in other.children().into_iter().flatten() {
                    self.walk(plan, child, pipeline);
                }
            }
        }
    }
}

/// The pipeline the root of a plan produces into.
const ROOT: PipelineRef = 0;

#[cfg(test)]
mod tests {
    use super::Pipelines;
    use crate::plan::Plan;

    fn decomposed(text: &str) -> (Plan, Pipelines) {
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        let pipelines = Pipelines::of(&plan);
        (plan, pipelines)
    }

    #[test]
    fn a_plan_with_nothing_that_buffers_is_one_pipeline() {
        let (plan, pipelines) = decomposed(concat!(
            "Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
        ));
        assert_eq!(pipelines.len(), 1);
        assert_eq!(pipelines.pipeline(plan.root()), 0);
        assert!(pipelines.waits_for(0).is_empty());
    }

    #[test]
    fn a_sort_ends_the_pipeline_below_it_and_the_one_above_waits() {
        let (plan, pipelines) = decomposed(concat!(
            "Sort [#0.0::INTEGER ASC NULLS LAST]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
        ));
        assert_eq!(pipelines.len(), 2);
        assert_eq!(pipelines.pipeline(plan.root()), 1, "the sort is the sink of the one below");
        assert_eq!(pipelines.waits_for(0), [1]);
        assert!(pipelines.waits_for(1).is_empty());
    }

    #[test]
    fn a_join_is_two_pipelines_in_the_order_they_have_to_run() {
        let (plan, pipelines) = decomposed(concat!(
            "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n",
            "  Get memory.main.l AS l #0 [a::INTEGER]\n",
            "  Get memory.main.r AS r #1 [a::INTEGER]\n",
        ));
        let [left, right] = plan.node(plan.root()).children();
        assert_eq!(pipelines.len(), 3);
        assert_eq!(pipelines.pipeline(right.unwrap()), 1, "the gathered side runs first");
        assert_eq!(pipelines.pipeline(left.unwrap()), 2, "the probing side is the second");
        assert_eq!(pipelines.pipeline(plan.root()), 2, "and the join is its sink");
        assert_eq!(pipelines.waits_for(2), [1]);
        assert_eq!(pipelines.waits_for(0), [2]);
    }

    #[test]
    fn a_cross_product_keeps_its_left_side_where_it_was() {
        let (plan, pipelines) = decomposed(concat!(
            "CrossProduct\n",
            "  Get memory.main.l AS l #0 [a::INTEGER]\n",
            "  Get memory.main.r AS r #1 [a::INTEGER]\n",
        ));
        let [left, right] = plan.node(plan.root()).children();
        assert_eq!(pipelines.len(), 2);
        assert_eq!(pipelines.pipeline(plan.root()), 0, "the product streams");
        assert_eq!(pipelines.pipeline(left.unwrap()), 0, "and so does the side it streams");
        assert_eq!(pipelines.pipeline(right.unwrap()), 1, "the side that is kept is its own");
        assert_eq!(pipelines.waits_for(0), [1]);
    }

    #[test]
    fn two_sorts_under_one_another_are_three_pipelines_in_a_line() {
        let (plan, pipelines) = decomposed(concat!(
            "Sort [#0.0::INTEGER ASC NULLS LAST]\n",
            "  Limit 10 offset 0\n",
            "    Sort [#0.0::INTEGER DESC NULLS FIRST]\n",
            "      Get memory.main.t AS t #0 [a::INTEGER]\n",
        ));
        assert_eq!(pipelines.len(), 3);
        assert_eq!(pipelines.pipeline(plan.root()), 1);
        assert_eq!(pipelines.waits_for(0), [1]);
        assert_eq!(pipelines.waits_for(1), [2]);
        assert!(pipelines.waits_for(2).is_empty());
    }
}
