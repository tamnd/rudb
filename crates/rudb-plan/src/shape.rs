//! The shape a plan runs as: which operator each node becomes, which pipeline it runs in, and which
//! pipeline waits for which.
//!
//! A pipeline is a run of operators from a source to a sink, and a plan breaks into several of them
//! wherever an operator has to see all of its input before it produces anything. A sort is the
//! plain case: everything under it is one pipeline that ends in the sort, and what reads the sorted
//! rows back is the next one, which cannot start until the first has finished. A join is two below
//! the one above it, because the side that is gathered has to be complete before the side that
//! probes it can run a single row.
//!
//! The operators are numbered by the same walk, because the two answers are the same answer. An
//! operator's id is what a metrics document calls it, what `EXPLAIN` prints beside it and what the
//! builder tags its counters with, and a number that three pieces of code work out separately is a
//! number that three pieces of code can disagree about.
//!
//! # Why this is here
//!
//! Two crates need all of it and neither can see the other. `rudb-exec` builds the tree, and
//! `rudb-opt` prints what `EXPLAIN` shows without building anything. Written twice it would be
//! right twice on the day it was written and wrong once some time after that, and the version that
//! would be wrong is the printed one, which is the version somebody reads when they are trying to
//! understand why a query is slow.
//!
//! It is physical knowledge about a logical tree, which is worth saying out loud. Whether an
//! operator is a pipeline breaker is a fact about how it is executed rather than about what it
//! means, and the reason it can live here anyway is that at this milestone the physical plan is the
//! logical plan with different words on it, which `crates/rudb-exec/src/build.rs` says at the top.
//! The day there is a physical plan this moves onto it and every caller keeps its call.
//!
//! # The rule for the pipelines
//!
//! The root of the plan produces into pipeline 0. Walking down from there, a node inherits the
//! pipeline of its parent, except that
//!
//! - an aggregate, a window, a sort, a top n and a distinct are sinks, so the node and everything
//!   under it are a new pipeline that the parent's waits for,
//! - a join and a set operation are two, the side that is gathered first and the side that reads
//!   it, with the second waiting for the first and the parent's waiting for the second,
//! - a cross product keeps its left side and itself in the parent's pipeline, because the product
//!   is produced a chunk at a time and never held, and puts its right side in a new one, because
//!   that side is kept whole to be replayed,
//! - a materialised `WITH` is the sink of a new pipeline that its definition fills, and its body
//!   stays in the parent's, with the pipeline holding each read of the name waiting for the one
//!   that fills it.
//!
//! There is no scheduler reading any of this yet. It is written down because it is known, and an
//! edge reconstructed later from a tree somebody has already flattened is an edge somebody has to
//! guess at.
//!
//! # The rule for the numbers
//!
//! A node takes the next id when the walk reaches it, so the root is operator 0 and a parent is
//! always numbered before everything under it. A node with two inputs takes a second id straight
//! after its own, for the operator that holds the side that has to finish first: the gather under a
//! join or a set operation, and the kept chunks under a cross product. Those are operators in their
//! own right, they have their own counters and their own row in a metrics document, and they exist
//! because the plan has two inputs there rather than because somebody chose to add one.
//!
//! Then the children, and for a node with two inputs the side that runs first is walked first, so
//! the ids go in the order the work happens rather than in the order the tree prints.

use crate::node::Node;
use crate::plan::Plan;
use crate::{NodeRef, OperatorRef, PipelineRef};

/// What a plan runs as.
#[derive(Debug, Clone)]
pub struct Shape {
    /// Per node in the arena, the operator it becomes and the pipeline that runs it, or none for a
    /// node the root does not reach.
    of: Vec<Option<Placed>>,
    /// What each pipeline waits for, indexed by pipeline.
    waits: Vec<Vec<PipelineRef>>,
    /// How many operators there are.
    operators: OperatorRef,
    /// Per operator, the operator its rows go to, or none for the one that produces the answer.
    ///
    /// Indexed by operator id, which is why it is filled where the ids are handed out rather than
    /// by a second walk. See [`Shape::consumer`] for what a reader does with it.
    consumes: Vec<Option<OperatorRef>>,
    /// The pipeline each materialisation currently being walked over is filled by.
    ///
    /// A stack rather than a map, because a materialised `WITH` inside another one is a `WITH`
    /// inside the body of the first, and the inner name is the one a scan under it reads.
    holding: Vec<(u32, PipelineRef)>,
}

/// One node's place in the shape.
#[derive(Debug, Clone, Copy)]
struct Placed {
    operator: OperatorRef,
    /// The operator that holds the side which has to finish first, for a node with two inputs.
    gathered: Option<OperatorRef>,
    pipeline: PipelineRef,
}

impl Shape {
    /// Works out the shape of a plan.
    #[must_use]
    pub fn of(plan: &Plan) -> Self {
        let mut shape = Self {
            of: vec![None; plan.node_count()],
            waits: vec![Vec::new()],
            operators: 0,
            consumes: Vec::new(),
            holding: Vec::new(),
        };
        shape.walk(plan, plan.root(), ROOT, None);
        shape
    }

    /// How many pipelines there are, which is at least one.
    #[must_use]
    pub fn pipelines(&self) -> usize {
        self.waits.len()
    }

    /// How many operators the tree has, which is at least one and is more than the plan has nodes
    /// whenever the plan has a node with two inputs in it.
    #[must_use]
    pub fn operators(&self) -> OperatorRef {
        self.operators
    }

    /// The operator this node becomes.
    ///
    /// # Panics
    ///
    /// If the node is not reachable from the plan's root, which is a node the arena is still
    /// holding after a rewrite replaced it.
    #[must_use]
    pub fn operator(&self, node: NodeRef) -> OperatorRef {
        self.placed(node).operator
    }

    /// The operator this node becomes, or none for a node the root does not reach.
    ///
    /// The tolerant form of [`Shape::operator`], for a caller walking the whole arena rather than
    /// the tree, which is what somebody filling one fact in per operator ends up doing.
    #[must_use]
    pub fn operator_of(&self, node: NodeRef) -> Option<OperatorRef> {
        self.of.get(node as usize).copied().flatten().map(|placed| placed.operator)
    }

    /// The operator holding the side of this node that has to finish first, if it has two inputs.
    ///
    /// # Panics
    ///
    /// The same as [`Shape::operator`].
    #[must_use]
    pub fn gathered(&self, node: NodeRef) -> Option<OperatorRef> {
        self.placed(node).gathered
    }

    /// The operator this one's rows go into, or none for the operator that produces the answer.
    ///
    /// Keyed by operator rather than by node because the operator tree is not quite the plan tree:
    /// a node with two inputs is two operators, and the side that is gathered feeds the one that
    /// holds it rather than the join above it. A reader of a metrics document that wants to check
    /// an operator's input against what its children produced needs the edges the way the rows
    /// actually moved, which is this.
    ///
    /// # Panics
    ///
    /// If there is no such operator.
    #[must_use]
    pub fn consumer(&self, operator: OperatorRef) -> Option<OperatorRef> {
        self.consumes[operator as usize]
    }

    /// The pipeline this node runs in.
    ///
    /// For a sink that is the pipeline it ends rather than the one above it, so a sort is in the
    /// pipeline that feeds it and the operator that reads the sorted rows is in the one above.
    ///
    /// # Panics
    ///
    /// The same as [`Shape::operator`].
    #[must_use]
    pub fn pipeline(&self, node: NodeRef) -> PipelineRef {
        self.placed(node).pipeline
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

    /// Where a node ended up.
    ///
    /// # Panics
    ///
    /// If the node is not reachable from the plan's root.
    fn placed(&self, node: NodeRef) -> Placed {
        self.of[node as usize].expect("a node under the root of the plan it was walked from")
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

    /// The next operator id, and the operator its rows go to.
    fn number(&mut self, into: Option<OperatorRef>) -> OperatorRef {
        let id = self.operators;
        self.operators += 1;
        self.consumes.push(into);
        id
    }

    fn walk(
        &mut self,
        plan: &Plan,
        node: NodeRef,
        pipeline: PipelineRef,
        into: Option<OperatorRef>,
    ) {
        let operator = self.number(into);
        match *plan.node(node) {
            Node::Aggregate { input, .. }
            | Node::Window { input, .. }
            | Node::Sort { input, .. }
            | Node::TopN { input, .. }
            // A share of the input is not known until the input has ended, so this holds its rows
            // and is a sink, where a plain limit hands each chunk on and stays in the pipeline.
            | Node::LimitPercent { input, .. }
            | Node::Distinct { input, .. } => {
                let below = self.fresh();
                self.waits_on(pipeline, below);
                self.of[node as usize] = Some(Placed { operator, gathered: None, pipeline: below });
                self.walk(plan, input, below, Some(operator));
            }
            Node::Join { left, right, .. }
            | Node::DependentJoin { left, right, .. }
            | Node::SetOp { left, right, .. } => {
                let gathered = self.number(Some(operator));
                let first = self.fresh();
                let second = self.fresh();
                self.waits_on(second, first);
                self.waits_on(pipeline, second);
                self.of[node as usize] =
                    Some(Placed { operator, gathered: Some(gathered), pipeline: second });
                // The gathered side's rows go into the operator that holds them rather than
                // straight into the join, which is the one place the operator tree has a shape the
                // plan does not.
                self.walk(plan, right, first, Some(gathered));
                self.walk(plan, left, second, Some(operator));
            }
            // The definition is held whole and the body reads it, so the node is the sink of the
            // pipeline that fills it, the same way a sort is the sink of the pipeline under it. The
            // body stays where the parent is, because it produces into the parent a chunk at a time
            // and is never held.
            //
            // The edge is recorded at every scan rather than here, because that is where the
            // waiting actually is: a scan under a sort in the body is in a pipeline of its own, and
            // it is that pipeline which cannot start until the rows exist. A body that reads the
            // name nowhere waits for nothing, which is the shape of a materialisation the optimizer
            // is about to drop.
            Node::MaterializedCte { definition, body, cte, .. } => {
                // Its rows go nowhere. It is filled by its definition and read back by the scans
                // of its name, so nothing above it ever sees a row of it, and naming the node it
                // hangs under would claim rows that the body produced and this never handed on.
                // That makes it a second operator with nothing above it, which is what it is.
                self.consumes[operator as usize] = None;
                let filling = self.fresh();
                self.of[node as usize] =
                    Some(Placed { operator, gathered: None, pipeline: filling });
                self.walk(plan, definition, filling, Some(operator));
                self.holding.push((cte, filling));
                // The body produces the answer a chunk at a time and the node it hangs under never
                // sees those rows, so what consumes them is whatever consumes this node.
                self.walk(plan, body, pipeline, into);
                self.holding.pop();
            }
            Node::CteScan { cte, .. } => {
                self.of[node as usize] = Some(Placed { operator, gathered: None, pipeline });
                if let Some(&(_, filled)) =
                    self.holding.iter().rev().find(|&&(held, _)| held == cte)
                {
                    if !self.waits[pipeline as usize].contains(&filled) {
                        self.waits_on(pipeline, filled);
                    }
                }
            }
            Node::CrossProduct { left, right } => {
                let gathered = self.number(Some(operator));
                let aside = self.fresh();
                self.waits_on(pipeline, aside);
                self.of[node as usize] =
                    Some(Placed { operator, gathered: Some(gathered), pipeline });
                self.walk(plan, right, aside, Some(gathered));
                self.walk(plan, left, pipeline, Some(operator));
            }
            ref other => {
                self.of[node as usize] = Some(Placed { operator, gathered: None, pipeline });
                for child in other.children().into_iter().flatten() {
                    self.walk(plan, child, pipeline, Some(operator));
                }
            }
        }
    }
}

/// The pipeline the root of a plan produces into.
///
/// Public because it is the one pipeline nothing drains. Every other pipeline ends in a sink and is
/// run by the loop that fills that sink, and this one is pulled from by whoever wanted the answer,
/// so whoever that is has to know which pipeline the loop they are writing belongs to.
pub const ROOT: PipelineRef = 0;

#[cfg(test)]
mod tests {
    use super::Shape;
    use crate::plan::Plan;

    fn shaped(text: &str) -> (Plan, Shape) {
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        let shape = Shape::of(&plan);
        (plan, shape)
    }

    #[test]
    fn a_plan_with_nothing_that_buffers_is_one_pipeline() {
        let (plan, shape) = shaped(concat!(
            "Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
        ));
        assert_eq!(shape.pipelines(), 1);
        assert_eq!(shape.pipeline(plan.root()), 0);
        assert!(shape.waits_for(0).is_empty());
    }

    #[test]
    fn a_sort_ends_the_pipeline_below_it_and_the_one_above_waits() {
        let (plan, shape) = shaped(concat!(
            "Sort [#0.0::INTEGER ASC NULLS LAST]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
        ));
        assert_eq!(shape.pipelines(), 2);
        assert_eq!(shape.pipeline(plan.root()), 1, "the sort is the sink of the one below");
        assert_eq!(shape.waits_for(0), [1]);
        assert!(shape.waits_for(1).is_empty());
    }

    #[test]
    fn a_join_is_two_pipelines_in_the_order_they_have_to_run() {
        let (plan, shape) = shaped(concat!(
            "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n",
            "  Get memory.main.l AS l #0 [a::INTEGER]\n",
            "  Get memory.main.r AS r #1 [a::INTEGER]\n",
        ));
        let [left, right] = plan.node(plan.root()).children();
        assert_eq!(shape.pipelines(), 3);
        assert_eq!(shape.pipeline(right.unwrap()), 1, "the gathered side runs first");
        assert_eq!(shape.pipeline(left.unwrap()), 2, "the probing side is the second");
        assert_eq!(shape.pipeline(plan.root()), 2, "and the join is its sink");
        assert_eq!(shape.waits_for(2), [1]);
        assert_eq!(shape.waits_for(0), [2]);
    }

    #[test]
    fn a_cross_product_keeps_its_left_side_where_it_was() {
        let (plan, shape) = shaped(concat!(
            "CrossProduct\n",
            "  Get memory.main.l AS l #0 [a::INTEGER]\n",
            "  Get memory.main.r AS r #1 [a::INTEGER]\n",
        ));
        let [left, right] = plan.node(plan.root()).children();
        assert_eq!(shape.pipelines(), 2);
        assert_eq!(shape.pipeline(plan.root()), 0, "the product streams");
        assert_eq!(shape.pipeline(left.unwrap()), 0, "and so does the side it streams");
        assert_eq!(shape.pipeline(right.unwrap()), 1, "the side that is kept is its own");
        assert_eq!(shape.waits_for(0), [1]);
    }

    #[test]
    fn a_materialisation_is_the_sink_of_the_pipeline_that_fills_it() {
        let (plan, shape) = shaped(concat!(
            "MaterializedCte c @0 [a::INTEGER]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
            "  Project #2 [#1.0::INTEGER AS a]\n",
            "    CteScan c @0 #1 [a::INTEGER]\n",
        ));
        let [definition, body] = plan.node(plan.root()).children();
        assert_eq!(shape.pipelines(), 2);
        assert_eq!(shape.pipeline(plan.root()), 1, "the node is what the definition fills");
        assert_eq!(shape.pipeline(definition.unwrap()), 1, "and the definition is under it");
        assert_eq!(shape.pipeline(body.unwrap()), 0, "the body produces the answer");
        assert_eq!(shape.waits_for(0), [1], "and cannot start before the rows are there");
    }

    #[test]
    fn the_pipeline_that_waits_is_the_one_the_read_is_in() {
        let (plan, shape) = shaped(concat!(
            "MaterializedCte c @0 [a::INTEGER]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
            "  Sort [#1.0::INTEGER ASC NULLS LAST]\n",
            "    CteScan c @0 #1 [a::INTEGER]\n",
        ));
        assert_eq!(shape.pipelines(), 3);
        assert_eq!(shape.waits_for(0), [2], "the answer waits for the sort");
        assert_eq!(shape.waits_for(2), [1], "and the sort waits for the rows it reads");
        assert!(shape.waits_for(1).is_empty(), "which wait for nothing");
        let [_, body] = plan.node(plan.root()).children();
        assert_eq!(shape.pipeline(body.unwrap()), 2);
    }

    /// A body that never names the materialisation waits for nothing, which is the shape the pass
    /// that drops an unread one is about to remove.
    #[test]
    fn a_body_that_reads_nothing_waits_for_nothing() {
        let (_, shape) = shaped(concat!(
            "MaterializedCte c @0 [a::INTEGER]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
            "  Project #2 [1::INTEGER AS one]\n",
            "    Dummy\n",
        ));
        assert_eq!(shape.pipelines(), 2);
        assert!(shape.waits_for(0).is_empty());
    }

    #[test]
    fn two_sorts_under_one_another_are_three_pipelines_in_a_line() {
        let (plan, shape) = shaped(concat!(
            "Sort [#0.0::INTEGER ASC NULLS LAST]\n",
            "  Limit 10 offset 0\n",
            "    Sort [#0.0::INTEGER DESC NULLS FIRST]\n",
            "      Get memory.main.t AS t #0 [a::INTEGER]\n",
        ));
        assert_eq!(shape.pipelines(), 3);
        assert_eq!(shape.pipeline(plan.root()), 1);
        assert_eq!(shape.waits_for(0), [1]);
        assert_eq!(shape.waits_for(1), [2]);
        assert!(shape.waits_for(2).is_empty());
    }

    #[test]
    fn a_parent_is_numbered_before_everything_under_it() {
        let (plan, shape) = shaped(concat!(
            "Sort [#0.0::INTEGER ASC NULLS LAST]\n",
            "  Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n",
            "    Get memory.main.t AS t #0 [a::INTEGER]\n",
        ));
        let filter = plan.node(plan.root()).children()[0].unwrap();
        let get = plan.node(filter).children()[0].unwrap();
        assert_eq!(shape.operator(plan.root()), 0);
        assert_eq!(shape.operator(filter), 1);
        assert_eq!(shape.operator(get), 2);
        assert_eq!(shape.operators(), 3);
        assert_eq!(shape.gathered(plan.root()), None, "one input, nothing to hold");
    }

    #[test]
    fn a_node_with_two_inputs_is_two_operators_and_the_first_side_is_numbered_first() {
        let (plan, shape) = shaped(concat!(
            "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n",
            "  Get memory.main.l AS l #0 [a::INTEGER]\n",
            "  Get memory.main.r AS r #1 [a::INTEGER]\n",
        ));
        let [left, right] = plan.node(plan.root()).children();
        assert_eq!(shape.operator(plan.root()), 0);
        assert_eq!(shape.gathered(plan.root()), Some(1), "the gather is an operator of its own");
        assert_eq!(shape.operator(right.unwrap()), 2, "the side that has to finish first");
        assert_eq!(shape.operator(left.unwrap()), 3);
        assert_eq!(shape.operators(), 4);
    }

    #[test]
    fn every_operator_but_the_one_that_answers_names_what_its_rows_go_into() {
        let (_, shape) = shaped(concat!(
            "Sort [#0.0::INTEGER ASC NULLS LAST]\n",
            "  Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n",
            "    Get memory.main.t AS t #0 [a::INTEGER]\n",
        ));
        assert_eq!(shape.consumer(0), None, "the sort is what the answer is read from");
        assert_eq!(shape.consumer(1), Some(0));
        assert_eq!(shape.consumer(2), Some(1));
    }

    /// The one place the operator tree is a different shape than the plan. The gathered side feeds
    /// the operator that holds it, and that one feeds the join, so a check that read the plan tree
    /// instead would compare the join's input against rows it never saw.
    #[test]
    fn the_gathered_side_feeds_the_operator_that_holds_it_rather_than_the_join() {
        let (_, shape) = shaped(concat!(
            "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n",
            "  Get memory.main.l AS l #0 [a::INTEGER]\n",
            "  Get memory.main.r AS r #1 [a::INTEGER]\n",
        ));
        assert_eq!(shape.consumer(0), None, "the join produces the answer");
        assert_eq!(shape.consumer(1), Some(0), "the gather hands the held side to the join");
        assert_eq!(shape.consumer(2), Some(1), "the side that finishes first is what it holds");
        assert_eq!(shape.consumer(3), Some(0), "the driving side goes straight into the join");
    }

    /// A materialisation is filled by its definition and read back by the scans of its name, so no
    /// row of it is ever handed upwards and it is an operator with nothing above it. Its body is
    /// what streams into whatever the whole thing hangs under.
    #[test]
    fn a_materialisation_is_fed_by_its_definition_and_its_body_streams_past_it() {
        let (_, shape) = shaped(concat!(
            "Project #2 [#3.0::INTEGER AS a]\n",
            "  MaterializedCte c @0 [a::INTEGER]\n",
            "    Get memory.main.t AS t #0 [a::INTEGER]\n",
            "    CteScan c @0 #1 [a::INTEGER]\n",
        ));
        assert_eq!(shape.consumer(0), None);
        assert_eq!(shape.consumer(1), None, "the materialisation hands nothing upwards");
        assert_eq!(shape.consumer(2), Some(1), "the definition is what fills it");
        assert_eq!(shape.consumer(3), Some(0), "and the body produces into the project");
    }
}
