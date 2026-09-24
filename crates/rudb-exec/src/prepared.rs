//! An expression prepared once for a pipeline and then evaluated over every chunk.
//!
//! `spec/engine/04-expressions.md`. [`evaluate`](crate::evaluate) walks the plan's expression tree
//! on every chunk, which means it does four things per chunk that depend on nothing about the
//! chunk: it recurses, it resolves every column reference by a linear search through the schema, it
//! clones a [`LogicalType`] for every node, and it copies the whole column a [`Expr::Column`] names.
//! Over `hits` at a hundred thousand chunks that is a hundred thousand schema searches per column
//! reference and a hundred thousand copies of every column any expression mentions.
//!
//! This type does all four once. The tree is flattened into a post order array, so evaluating it is
//! a loop over that array and the recursion is gone with it. Column references are resolved to
//! positions when the pipeline is built. Types are held here rather than cloned out of the plan.
//! And a column reference is not a step that produces anything: it is read straight out of the chunk
//! at the point an operand is wanted, so the column is never copied at all.
//!
//! # What is shared and what is not
//!
//! [`Prepared`] is immutable after it is built and is `Send` and `Sync`, so one of them serves every
//! thread running a copy of the pipeline. [`Scratch`] is the per chunk working space and there is
//! one per pipeline instance. That split is not for this layer's benefit. It is the same split every
//! operator needs at layer eight, where the scheduler runs one pipeline on as many threads as it has
//! morsels for, and building it here means the operators above are written against it from the start
//! rather than retrofitted onto it.
//!
//! # What is still allocated per chunk
//!
//! Two things, and both are named rather than hidden. A node with four or more operands gathers
//! references to them into a `Vec<&Vector>` so a kernel can take a slice, which is one allocation of
//! pointers rather than a copy of any data, and which a node of one, two or three operands does on
//! the stack instead. And every kernel allocates the vector it returns, because no kernel in
//! `rudb-kernels` takes an output parameter. The second is much the larger of the two and it is the
//! one tier 1 fusion removes, which is scheduled after layer six for the reason
//! `spec/engine/04-expressions.md` gives: once the tree walk is gone what is left to save is pass
//! count, and at 1024 rows the intermediate vectors are eight kilobytes and stay in L1.

use rudb_common::{
    Error, LogicalType, PhysicalType, Result, Session, SessionTimeZone, Span, Value,
};
use rudb_kernels::{
    Comparison, Connective, Found, Held, Lookup, Members, Recipe, cast_in_time_zone, combine,
    compare_prepared, in_set, is_true, refine_flags, refine_prepared, select_prepared, selection,
};
use rudb_plan::{CompareOp, ConjunctionOp, Expr, ExprRef, Plan};
use rudb_vector::{Assembly, Chunk, Selection, Vector};
use std::collections::HashMap;
use std::sync::Arc;

use crate::fused::Fused;
use crate::lambda::{Lambda, lambda_call};
use crate::ordering::Ordering;
use crate::schema::Schema;
use crate::written::written;

/// The scheduler's half of the expression contract, imposed now rather than at layer eight.
///
/// A prepared expression is the immutable half of a pipeline and layer eight hands one of them to
/// every thread running that pipeline. That is only sound if it holds nothing thread local, and the
/// way to find out on the commit that breaks it rather than eight layers later is to ask the
/// compiler here, exactly as [`Chunk`] does for the data plane.
const _: () = {
    const fn assert_shareable<T: Send + Sync>() {}
    assert_shareable::<Prepared>();
};

/// One or more bound expressions, flattened and resolved against a schema.
///
/// Built once per pipeline with [`Prepared::new`] and evaluated per chunk with
/// [`Prepared::evaluate`] or [`Prepared::evaluate_one`], each of which wants the [`Scratch`] that
/// [`Prepared::scratch`] hands out.
#[derive(Debug)]
pub struct Prepared {
    /// The nodes in post order, so every node's operands have already been computed when it runs.
    steps: Vec<Step>,
    /// The type each step produces, indexed the same way as `steps`.
    ///
    /// A parallel array rather than a field in the variant, for the reason [`Expr`] gives: a
    /// [`LogicalType`] owns a `Vec` for its nested cases and putting one in every variant would make
    /// the common variants several times larger for the benefit of the rare ones.
    types: Vec<LogicalType>,
    /// The source range each step came from, indexed the same way as `steps`.
    spans: Vec<Span>,
    /// The operand lists of the steps that have one, as runs of step indices.
    operands: Vec<usize>,
    /// The last step that reads each step's slot, or `usize::MAX` for one nothing reads.
    ///
    /// A slot is emptied as soon as the step that was the last to read it has run. Keeping every
    /// intermediate alive to the end of the array instead is what the first measured version of this
    /// did, and a chain of eight additions was slower prepared than walked because of it: nine live
    /// intermediates at eight kilobytes each is seventy two kilobytes of working set where the tree
    /// walk has two, and two is the pair the allocator hands back and forth and that stays in L1.
    /// Everything else about the prepared form was faster and this one thing paid all of it back.
    last_use: Vec<usize>,
    /// The step index each expression this was built from ends at.
    roots: Vec<usize>,
    /// The step already compiled for each shared plan expression.
    shared: HashMap<ExprRef, usize>,
    share: bool,
    /// Whether a tree of decimal arithmetic is run as one [`Fused`] step. Off only for the steps a
    /// fused one falls back to, which would otherwise fuse themselves again.
    fuse: bool,
    /// The parsed zone used only by casts whose answer depends on the session.
    time_zone: SessionTimeZone,
}

/// One node of a flattened expression.
///
/// A step refers to its operands by their index in [`Prepared::steps`], which is always smaller than
/// its own because the array is in post order.
#[derive(Debug)]
enum Step {
    /// A column of the chunk, by resolved position.
    ///
    /// This step computes nothing. Its slot stays empty and an operand that names it is read out of
    /// the chunk, which is the whole of what makes a column reference free rather than a copy.
    Column(usize),
    /// A literal, materialized into a constant vector as long as the chunk.
    Constant(Value),
    /// A cast to this step's own type.
    Cast {
        /// The step being cast.
        input: usize,
        /// Whether a failed cast yields null instead of raising.
        try_cast: bool,
    },
    /// A binary comparison.
    Compare {
        /// Which comparison.
        op: Comparison,
        /// The left operand's step.
        left: usize,
        /// The right operand's step.
        right: usize,
        /// The side that is a literal, in the one row column the comparison loops read it through,
        /// and `None` when neither side is one.
        ///
        /// Built here because the loops read both sides through a slice, so the constant side has
        /// to become a column somewhere, and the plan says which side that is. For a string it is
        /// also where the four byte prefix comes from, which is what almost every row of a string
        /// comparison is decided by.
        held: Option<Held>,
    },
    /// An `AND` or `OR` over a run of [`Prepared::operands`].
    Conjunction {
        /// Which connective.
        op: Connective,
        /// Where the operand list starts.
        start: usize,
        /// How many operands it has.
        len: usize,
    },
    /// A scalar function over a run of [`Prepared::operands`].
    Function {
        /// The call, with the name resolved and whatever the kernel could work out from the
        /// arguments that were literals already worked out.
        ///
        /// Held here so the plan is not consulted per chunk, and built here so that a regular
        /// expression is compiled once for the query rather than once for each of the hundred
        /// thousand chunks a pipeline over `hits` runs.
        recipe: Recipe,
        /// How the call is written, for the one error message that quotes it.
        ///
        /// Rendered when the pipeline is built rather than when a chunk arrives, because the plan
        /// is here and is not there. It is a short string per function node in the query and it is
        /// built once, which is a different cost from the tree walk's, where the plan is still to
        /// hand and the rendering can wait until the row that fails.
        written: String,
        /// Where the argument list starts.
        start: usize,
        /// How many arguments it has.
        len: usize,
    },
    /// A membership test over a list the query wrote out.
    ///
    /// The binder has no `IN` node: `x IN (1, 2, 3)` arrives as an `OR` of three equalities and
    /// `x NOT IN (1, 2, 3)` as an `AND` of three inequalities. That is the right shape for a binder
    /// to produce, because nothing after it then needs a second set of rules for null, and it is the
    /// wrong shape to run, because it is a pass over the column and an output vector per entry.
    /// This is that shape folded back up, and folding it here rather than after the operands are
    /// pushed is what keeps the equalities from being run anyway.
    InSet {
        /// The step being tested.
        input: usize,
        /// The list, as a set, with the null rule and the direction it is read in.
        members: Members,
    },
    /// A searched `CASE`, whose branches are prepared expressions of their own.
    ///
    /// Nested rather than flattened into the same array because a branch is not evaluated over the
    /// chunk, it is evaluated over the rows no earlier arm claimed, and a step in the outer array
    /// would have no way to say that. The selection threaded form in #57 replaces this whole
    /// variant, and when it does the branches stop being separate arrays.
    Case {
        /// The `WHEN`/`THEN` pairs, in order.
        arms: Vec<PreparedArm>,
        /// The `ELSE`, if there is one. Absent means null.
        otherwise: Option<Prepared>,
        /// How to answer it as codes, for the shape that can be. Absent means read the values.
        blend: Option<Blend>,
    },
    /// A tree of decimal arithmetic over columns and literals, run as one loop when the columns'
    /// ranges prove it cannot overflow.
    ///
    /// The fallback is the same tree prepared the ordinary way, nested for the reason a case's
    /// branches are, and it is what runs over a chunk the ranges do not settle.
    Fused {
        /// The program.
        fused: Box<Fused>,
        /// The steps it replaced.
        fallback: Box<Prepared>,
    },
    /// A call to a function that takes a lambda, whose body is a prepared expression of its own.
    ///
    /// Nested for the reason a case's branches are: the body does not run over the chunk, it runs
    /// over a chunk with a row per element that [`Lambda`] builds, and a step in the outer array has
    /// no way to say that.
    Lambda {
        /// The steps of the call's other arguments: the list and `list_reduce`'s initial value, or
        /// `invoke`'s parameters.
        inputs: Vec<usize>,
        /// The layout of what the body runs over and what to do with its answers.
        runner: Box<Lambda>,
        /// The body, prepared against the runner's schema.
        body: Box<Prepared>,
    },
}

/// One `WHEN`/`THEN` pair of a prepared [`Step::Case`].
#[derive(Debug)]
struct PreparedArm {
    /// The condition.
    when: Prepared,
    /// The result if the condition is true.
    then: Prepared,
}

/// A `CASE` over text whose every branch is a column or a literal, answered as codes.
///
/// What the general path does with the branches is read their values and write them into a vector of
/// their own, which for a text column out of a native file decodes a compressed dictionary block per
/// row and then throws the dictionary away. An operator above that has to work with strings even
/// though every string it sees came out of one dictionary it could have kept.
///
/// It does not have to. The branches here name values rather than compute them, so if they all name
/// values of one dictionary then so does the answer, and the answer is the codes: one code per row
/// copied from the branch that claimed the row, and a literal is one code for all of its rows once
/// the dictionary has been searched for it. Nothing is read and the dictionary comes out the other
/// side, so a group by over the `CASE` groups on codes the way a group by over the bare column does.
///
/// ClickBench 39 is the query this is for. It groups by `CASE WHEN (SearchEngineID = 0 AND
/// AdvEngineID = 0) THEN Referer ELSE '' END` beside `URL`, and writing that one column out as
/// strings was a quarter of the query.
///
/// The shape is narrow on purpose. A branch that computes anything is not here, because then the
/// answer is a value that no dictionary holds. A literal the dictionary does not hold is not here
/// either, for the same reason, and that is decided per dictionary at run time rather than when the
/// expression is prepared. And a `CASE` with no `ELSE` is not here, because the rows nothing claims
/// are null and a null is not a code.
#[derive(Debug)]
struct Blend {
    /// Where each branch takes its value from: one per arm in order, and the `ELSE` last.
    branches: Vec<Branch>,
    /// The literals the branches name, each with the search that finds it in a dictionary.
    literals: Vec<(String, Lookup)>,
}

/// Where one branch of a [`Blend`] takes its value from.
#[derive(Debug, Clone, Copy)]
enum Branch {
    /// A column of the chunk, by resolved position. Its rows keep the codes they arrived with.
    Column(usize),
    /// The literal at this index of [`Blend::literals`]. Its rows all get one code.
    Literal(usize),
}

/// The per chunk working space of one [`Prepared`].
///
/// One per pipeline instance and never shared, which is the mutable half of the split the module
/// documentation describes. It is handed back in rather than made inside [`Prepared::evaluate`] so
/// that the array of slots survives from one chunk to the next instead of being allocated a hundred
/// thousand times over a scan.
#[derive(Debug, Default)]
pub struct Scratch {
    /// What each step produced, or `None` for a step that produces nothing and for one that has not
    /// run yet.
    slots: Vec<Option<Vector>>,
    /// What each connective step has learned about its operands, indexed by step.
    ///
    /// Empty for every step that is not a connective and for a connective a filter has not reached
    /// yet, since it is built the first time one runs and the shape it needs is not known before
    /// then. This is the mutable half of the adaptive ordering and it is here rather than in
    /// [`Prepared`] because a prepared expression is shared by every thread running the pipeline.
    orders: Vec<Option<Ordering>>,
}

impl Scratch {
    /// The order a connective's operands are run in.
    ///
    /// For the tests that say the learning reached the walk. Nothing in the engine asks a scratch
    /// this, because the walk is the only thing that reads an ordering and it reads its own.
    #[cfg(test)]
    fn order(&self, step: usize) -> Option<&[usize]> {
        self.orders[step].as_ref().map(Ordering::order)
    }
}

impl Prepared {
    /// Prepares `exprs` against `schema`.
    ///
    /// # Errors
    ///
    /// If a column reference names a binding the schema does not have, or if an aggregate appears
    /// where an ordinary expression was expected. Both are failures of the plan rather than of the
    /// data, which is why they are found here, once, rather than on some chunk in the middle of a
    /// scan.
    pub fn new(plan: &Plan, exprs: &[ExprRef], schema: &Schema) -> Result<Self> {
        Self::build(plan, exprs, schema, false)
    }

    /// Prepares expressions whose caller can evaluate a shared expression graph as one unit.
    pub(crate) fn shared(plan: &Plan, exprs: &[ExprRef], schema: &Schema) -> Result<Self> {
        Self::build(plan, exprs, schema, true)
    }

    fn build(plan: &Plan, exprs: &[ExprRef], schema: &Schema, share: bool) -> Result<Self> {
        Self::built(plan, exprs, schema, share, true)
    }

    fn built(
        plan: &Plan,
        exprs: &[ExprRef],
        schema: &Schema,
        share: bool,
        fuse: bool,
    ) -> Result<Self> {
        let mut prepared = Self {
            steps: Vec::new(),
            types: Vec::new(),
            spans: Vec::new(),
            operands: Vec::new(),
            last_use: Vec::new(),
            roots: Vec::new(),
            shared: HashMap::new(),
            share,
            fuse,
            time_zone: SessionTimeZone::default(),
        };
        for &expr in exprs {
            let root = prepared.push(plan, expr, schema)?;
            prepared.roots.push(root);
        }
        prepared.last_use = prepared.last_uses();
        Ok(prepared)
    }

    /// Uses the zone of the session that owns this prepared expression.
    #[must_use]
    pub fn in_session(mut self, session: &Session) -> Self {
        self.set_time_zone(session.session_time_zone());
        self
    }

    /// Sets the zone here and in every lambda body, which is prepared before the session is known.
    fn set_time_zone(&mut self, time_zone: SessionTimeZone) {
        self.time_zone = time_zone;
        for step in &mut self.steps {
            match step {
                Step::Lambda { body, .. } => body.set_time_zone(time_zone),
                Step::Fused { fallback, .. } => fallback.set_time_zone(time_zone),
                _ => {}
            }
        }
    }

    /// Which step is the last to read each step, computed once when the expression is prepared.
    ///
    /// A root is never freed, because the whole point of running the array was to produce it. A
    /// step nothing reads and that is not a root cannot happen, since every step is pushed by the
    /// node that wanted it, but saying `usize::MAX` rather than asserting that keeps this a fact
    /// about the array rather than a claim about the builder.
    fn last_uses(&self) -> Vec<usize> {
        let mut last = vec![usize::MAX; self.steps.len()];
        for index in 0..self.steps.len() {
            self.for_each_operand(index, |operand| last[operand] = index);
        }
        for &root in &self.roots {
            last[root] = usize::MAX;
        }
        last
    }

    /// Visits the steps one step reads, whatever shape its operands are held in.
    fn for_each_operand(&self, index: usize, mut visit: impl FnMut(usize)) {
        match &self.steps[index] {
            // A case's branches are arrays of their own and read nothing out of this one, and a
            // fused tree reads its columns straight out of the chunk.
            Step::Column(_) | Step::Constant(_) | Step::Case { .. } | Step::Fused { .. } => {}
            Step::Cast { input, .. } | Step::InSet { input, .. } => visit(*input),
            Step::Lambda { inputs, .. } => inputs.iter().for_each(|&input| visit(input)),
            Step::Compare { left, right, .. } => {
                visit(*left);
                visit(*right);
            }
            Step::Conjunction { start, len, .. } | Step::Function { start, len, .. } => {
                for &operand in &self.operands[*start..*start + *len] {
                    visit(operand);
                }
            }
        }
    }

    /// Prepares one expression, which is the common case and saves the caller a slice.
    ///
    /// # Errors
    ///
    /// Whatever [`Prepared::new`] reports.
    pub fn one(plan: &Plan, expr: ExprRef, schema: &Schema) -> Result<Self> {
        Self::new(plan, &[expr], schema)
    }

    /// Working space sized for this expression.
    #[must_use]
    pub fn scratch(&self) -> Scratch {
        Scratch {
            slots: (0..self.steps.len()).map(|_| None).collect(),
            orders: (0..self.steps.len()).map(|_| None).collect(),
        }
    }

    /// How many expressions this was built from.
    #[must_use]
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    /// How many comparisons have their literal side already built.
    ///
    /// For the tests, for the same reason as [`Self::sets`]: an answer that moved would be a bug,
    /// so the only thing a test can look at is whether the building happened.
    #[cfg(test)]
    fn literals_built(&self) -> usize {
        self.steps.iter().filter(|step| matches!(step, Step::Compare { held: Some(_), .. })).count()
    }

    /// How many of the steps are an `IN` list folded back up.
    ///
    /// For the tests, which cannot see the fold in an answer because an answer that changed would
    /// be a bug.
    #[cfg(test)]
    fn sets(&self) -> usize {
        self.steps.iter().filter(|step| matches!(step, Step::InSet { .. })).count()
    }

    /// How many of the steps are a tree of decimal arithmetic run as one loop.
    #[cfg(test)]
    fn fused(&self) -> usize {
        self.steps.iter().filter(|step| matches!(step, Step::Fused { .. })).count()
    }

    /// How many of the function steps worked something out when this was built.
    ///
    /// For the tests, which cannot see the hoisting in an answer because an answer that changed
    /// would be a bug.
    #[cfg(test)]
    fn hoisted(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step, Step::Function { recipe, .. } if recipe.hoists()))
            .count()
    }

    /// Whether it was built from no expressions at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// How many of the steps do something to a row.
    ///
    /// A column reference and a literal are not among them. A column reference computes nothing at
    /// all, which is what makes a step that names one free rather than a copy, and a literal is
    /// materialized once for the whole chunk rather than once a row. What is left is a pass over
    /// the rows each, so this is roughly what one row costs, counted in the same unit the scan's
    /// own reading of that row is counted in.
    ///
    /// What reads it is the scan, through the weight an operator reports to the pipeline. See
    /// [`Stream::weight`](rudb_pipeline::Stream::weight).
    #[must_use]
    pub fn passes(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| !matches!(step, Step::Column(_) | Step::Constant(_)))
            .count()
    }

    /// Evaluates every expression over `chunk`, appending one vector each to `out`.
    ///
    /// Appends rather than returns a `Vec`, so a caller in a loop reuses one buffer.
    ///
    /// # Errors
    ///
    /// Anything a kernel reports, on the first expression that reports it.
    pub fn evaluate(
        &self,
        chunk: &Chunk,
        scratch: &mut Scratch,
        out: &mut Vec<Vector>,
    ) -> Result<()> {
        self.run(chunk, scratch)?;
        let mut remaining: HashMap<usize, usize> = HashMap::new();
        for &root in &self.roots {
            *remaining.entry(root).or_default() += 1;
        }
        for &root in &self.roots {
            // The one place a column is copied, and it is copied because the caller is taking
            // ownership of a vector that has to outlive the chunk it came from. `SELECT a` is that
            // shape and a projection of a bare column is the only expression where it happens.
            match self.steps[root] {
                Step::Column(position) => out.push(chunk.column(position)?.clone()),
                _ => {
                    let Some(left) = remaining.get_mut(&root) else {
                        return Err(Error::internal("a prepared root was not counted"));
                    };
                    *left -= 1;
                    if *left == 0 {
                        out.push(scratch.slots[root].take().ok_or_else(|| missing(root))?);
                    } else {
                        out.push(
                            scratch.slots[root].as_ref().ok_or_else(|| missing(root))?.clone(),
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// [`evaluate`](Self::evaluate) for a caller that is done with `chunk`, which a projection is.
    ///
    /// Every step has run before a root is handed over, so nothing reads the chunk after that and a
    /// root that is a bare column can take the column rather than copy it. A column named by more
    /// than one root is copied for all but the last of them. `SELECT *` into a table is all bare
    /// columns, and copying them was most of what its projection did.
    ///
    /// # Errors
    ///
    /// Whatever [`evaluate`](Self::evaluate) reports.
    pub fn evaluate_taking(
        &self,
        chunk: Chunk,
        scratch: &mut Scratch,
        out: &mut Vec<Vector>,
    ) -> Result<()> {
        self.run(&chunk, scratch)?;
        let width = chunk.width();
        let mut columns: Vec<Option<Vector>> = chunk.into_columns().into_iter().map(Some).collect();
        let mut uses = vec![0usize; width];
        let mut remaining: HashMap<usize, usize> = HashMap::new();
        for &root in &self.roots {
            match self.steps[root] {
                Step::Column(position) if position < width => uses[position] += 1,
                _ => *remaining.entry(root).or_default() += 1,
            }
        }
        for &root in &self.roots {
            if let Step::Column(position) = self.steps[root] {
                let missing = || {
                    Error::internal(format!(
                        "column {position} of a chunk that has {width} columns"
                    ))
                };
                let slot = columns.get_mut(position).ok_or_else(missing)?;
                let left = &mut uses[position];
                *left -= 1;
                let column = if *left == 0 { slot.take() } else { slot.clone() };
                out.push(column.ok_or_else(missing)?);
                continue;
            }
            let Some(left) = remaining.get_mut(&root) else {
                return Err(Error::internal("a prepared root was not counted"));
            };
            *left -= 1;
            if *left == 0 {
                out.push(scratch.slots[root].take().ok_or_else(|| missing(root))?);
            } else {
                out.push(scratch.slots[root].as_ref().ok_or_else(|| missing(root))?.clone());
            }
        }
        Ok(())
    }

    /// Evaluates a single expression over `chunk`, handing back a reference to the answer.
    ///
    /// A reference rather than a vector, because the caller of this is a filter, which reads the
    /// flags to build a selection and then drops them. Nothing about that wants ownership, and a
    /// predicate that is a bare column reference, which `WHERE flag` is, would otherwise copy the
    /// column to hand it over.
    ///
    /// # Errors
    ///
    /// Anything a kernel reports, and an internal error if this was not built from exactly one
    /// expression.
    pub fn evaluate_one<'s>(
        &'s self,
        chunk: &'s Chunk,
        scratch: &'s mut Scratch,
    ) -> Result<&'s Vector> {
        let [root] = self.roots[..] else {
            return Err(Error::internal(format!(
                "evaluate_one over a prepared expression of {} roots",
                self.roots.len()
            )));
        };
        self.run(chunk, scratch)?;
        self.operand(root, chunk, &scratch.slots)
    }

    /// Evaluates a single expression as a filter, handing back the rows it keeps.
    ///
    /// The difference between this and [`evaluate_one`](Self::evaluate_one) followed by
    /// [`selection`] is the whole of what a threaded filter is. An `AND` evaluated as an expression
    /// runs every conjunct over every row and then combines the flag vectors, so a predicate of four
    /// conjuncts that each pass a fifth of the rows does five times the work of one that stops
    /// looking at a row as soon as a conjunct rejects it. TPC-H Q6 is exactly that predicate.
    ///
    /// So the conjuncts of a top level `AND` are run one at a time, each over the rows the ones
    /// before it left, and the moment nothing is left the rest of the predicate is not run at all.
    /// The order they run in starts as the order the plan gives and then moves, because which
    /// conjunct is worth running first is a question about the data and the scan is the thing
    /// holding the answer. The `ordering` module has what is measured and how.
    ///
    /// A top level `OR` is threaded the same way against the complement. A row the first branch
    /// accepts is a row the filter keeps whatever the rest of the predicate says about it, so each
    /// branch is run over the rows no branch before it accepted, and the moment every row has been
    /// accepted the rest of the predicate is not run either. That is the mirror of the `AND` case
    /// and not an approximation of it: the answer is the same set of rows, because `OR` over three
    /// valued logic is true wherever any branch is true and nothing a later branch says can take a
    /// row back. It is worth less than the `AND` case in practice, since an `OR` of selective
    /// branches leaves almost every row in play for the branch after, and it is worth having anyway
    /// because the cost of finding that out is one merge per branch.
    ///
    /// What is threaded is the operand's own comparison rather than the whole of its subtree. A
    /// conjunct of `a + b > 5` still adds over the whole chunk, because the scalar kernels take a
    /// vector rather than a selection, and it is the comparison and everything downstream of it that
    /// reads only the rows still in play. An operand that is a bare column or a function produces
    /// flags over the chunk and is narrowed with [`refine_flags`], which is what keeps one awkward
    /// operand from putting the others back on the unthreaded path. An operand that is itself a
    /// connective recurses, so the two conjuncts of each half of `(a AND b) OR (c AND d)` are
    /// threaded the same way the halves are.
    ///
    /// None of this is available to a projection. `SELECT a > 5 AND b LIKE 'x%'` wants a value per
    /// row and the rows a selection dropped have no value in it, so [`evaluate`](Self::evaluate) and
    /// [`evaluate_one`](Self::evaluate_one) evaluate the whole tree over the whole chunk and combine
    /// flags. The two are separate entry points picked when the pipeline is built rather than one
    /// path with a flag in it, because conflating them is a wrong answer rather than a slow one.
    ///
    /// # Errors
    ///
    /// Anything a kernel reports, and an internal error if this was not built from exactly one
    /// expression.
    pub fn evaluate_filter(&self, chunk: &Chunk, scratch: &mut Scratch) -> Result<Selection> {
        let [root] = self.roots[..] else {
            return Err(Error::internal(format!(
                "evaluate_filter over a prepared expression of {} roots",
                self.roots.len()
            )));
        };
        scratch.slots.clear();
        scratch.slots.resize_with(self.steps.len(), || None);
        // A predicate that is not a connective at all is the same walk over one operand, which is
        // where [`thread`](Self::thread) starts: it runs the tree and turns the flags into a
        // selection, with no narrowing to do because nothing has narrowed anything yet.
        self.thread(root, 0, chunk, scratch, None)
    }

    /// The operands of one connective, run in order, each over the rows the ones before it left.
    ///
    /// `live` is the rows this connective has to decide about and `None` means every row of the
    /// chunk, which is not the same as a selection of all of them: it lets the first operand take
    /// the unthreaded kernel rather than a pass over an identity selection. The answer is the rows
    /// out of `live` the connective is true for.
    ///
    /// The walk is the same for both connectives and only the bookkeeping differs. `AND` carries the
    /// rows every operand so far has kept, so each answer replaces it. `OR` carries the rows no
    /// operand so far has accepted, so each answer comes out of it and the rows the connective keeps
    /// are the ones that went missing along the way.
    ///
    /// The operand is not `steps[begin..=operand]` evaluated and then narrowed. Its subtree is run
    /// over the whole chunk and it is the operand itself that reads only the rows in play, except
    /// where the operand is another connective, which recurses and threads its own operands from
    /// here rather than falling back to a flag vector. That is what makes `(a AND b) OR (c AND d)`
    /// four threaded comparisons rather than two threaded ones and two flag passes.
    fn branches(
        &self,
        index: usize,
        begin: usize,
        chunk: &Chunk,
        scratch: &mut Scratch,
        live: Option<&Selection>,
    ) -> Result<Selection> {
        let Step::Conjunction { op, start, len } = self.steps[index] else {
            return Err(Error::internal("a connective walk over a step that is not a connective"));
        };
        let operands = &self.operands[start..start + len];
        let rows = chunk.len();
        // Out of the scratch for the length of the walk, because the walk runs steps and running a
        // step wants the scratch. It goes back at the end, which is also where it learns. A walk
        // that fails leaves the slot empty and the next chunk starts the connective over, which is
        // a history lost on a query that is about to stop running anyway.
        let mut order = scratch.orders[index]
            .take()
            .unwrap_or_else(|| Ordering::new(op, self.weights(operands, begin)));
        let mut carried: Option<Selection> = live.cloned();
        for slot in 0..len {
            if carried.as_ref().is_some_and(Selection::is_empty) {
                break;
            }
            let which = order.at(slot);
            let operand = operands[which];
            // The array is in post order and an operand's whole subtree sits between the operand
            // before it and the operand itself, which is a range the run order cannot move. That is
            // what lets the operands run in any order at all without a second structure to say
            // where each one starts.
            let from = if which == 0 { begin } else { operands[which - 1] + 1 };
            let given = carried.as_ref().map_or(rows, Selection::len);
            let answered = self.thread(operand, from, chunk, scratch, carried.as_ref())?;
            order.observed(which, given, answered.len());
            carried = Some(match (op, carried) {
                (Connective::And, _) => answered,
                (Connective::Or, None) => answered.complement(rows),
                (Connective::Or, Some(carried)) => carried.without(&answered),
            });
            // Keep a shared step alive when a later operand still reads it.
            for step in from..=operand {
                if self.last_use[step] <= operand {
                    scratch.slots[step] = None;
                }
            }
        }
        order.relearn();
        scratch.orders[index] = Some(order);
        Ok(match (op, carried) {
            // A connective with no operands, which the binder does not build and which is answered
            // here rather than left to index arithmetic: an empty `AND` is every row and an empty
            // `OR` is none.
            (Connective::And, None) => live.cloned().unwrap_or_else(|| Selection::identity(rows)),
            (Connective::And, Some(kept)) => kept,
            (Connective::Or, None) => Selection::empty(),
            (Connective::Or, Some(missed)) => match live {
                None => missed.complement(rows),
                Some(live) => live.without(&missed),
            },
        })
    }

    /// What each operand of a connective costs to run over a chunk, for the ordering to divide by.
    ///
    /// An operand costs what its whole subtree costs, which is the steps from where the operand
    /// before it ended up to the operand itself.
    fn weights(&self, operands: &[usize], begin: usize) -> Vec<f64> {
        let mut costs = Vec::with_capacity(operands.len());
        let mut from = begin;
        for &operand in operands {
            costs.push((from..=operand).map(|step| self.weight(step)).sum());
            from = operand + 1;
        }
        costs
    }

    /// Roughly what one step costs to run over a chunk, against a comparison of two fixed width
    /// columns as the unit.
    ///
    /// A ranking rather than a prediction. Nothing downstream reads the number itself, only which
    /// of two of them is larger, and the differences that decide an order are the big ones: a
    /// column reference costs nothing because it is read in place, a string function costs many
    /// times what an integer comparison costs, and a comparison over a variable length type costs
    /// several times what the same comparison over a fixed width one costs. Everything finer than
    /// that is below the noise of what the window is measuring anyway.
    fn weight(&self, index: usize) -> f64 {
        match &self.steps[index] {
            // Read straight out of the chunk at the point an operand is wanted, so there is no step
            // to run and nothing to charge for.
            Step::Column(_) => 0.0,
            // One vector built per chunk, however many rows the chunk has.
            Step::Constant(_) => 0.25,
            // The operands carry the cost of a connective, and they are steps of their own.
            Step::Conjunction { .. } => 0.0,
            Step::Cast { input, .. } => 2.0 * touching(&self.types[*input]),
            Step::Compare { left, .. } => touching(&self.types[*left]),
            // One hash and one probe a row, whatever the list holds, which is the point of it. It
            // is dearer than a comparison and much cheaper than the chain of them it replaced.
            Step::InSet { input, .. } => 2.0 * touching(&self.types[*input]),
            Step::Function { start, len, .. } => {
                let widest = self.operands[*start..*start + *len]
                    .iter()
                    .map(|&argument| touching(&self.types[argument]))
                    .fold(1.0, f64::max);
                4.0 * widest
            }
            // A branch per arm, each of which is a prepared expression of its own that this does
            // not look inside. Charging for the arms alone understates it and says the right thing
            // about the order, which is that a `CASE` is not what you want in front.
            Step::Case { arms, .. } => 4.0 * arms.len() as f64,
            // A run of the body per element, which is several a row, and a list to take apart and
            // put back together around it.
            Step::Lambda { .. } => 16.0,
            // An integer operation a row per node and no check, which is a quarter of what the
            // function steps it replaced cost each.
            Step::Fused { fused, .. } => fused.len() as f64,
        }
    }

    /// One operand of a connective, over the rows it is still worth asking about.
    ///
    /// `begin` is the first step of the operand's subtree, which the caller knows because the steps
    /// are in post order.
    fn thread(
        &self,
        index: usize,
        begin: usize,
        chunk: &Chunk,
        scratch: &mut Scratch,
        live: Option<&Selection>,
    ) -> Result<Selection> {
        if matches!(self.steps[index], Step::Conjunction { .. }) {
            return self.branches(index, begin, chunk, scratch, live);
        }
        for step in begin..index {
            self.run_step(step, chunk, scratch)?;
        }
        if let Step::Compare { op, left, right, held } = &self.steps[index] {
            let one = self.operand(*left, chunk, &scratch.slots)?;
            let other = self.operand(*right, chunk, &scratch.slots)?;
            let held = held.as_ref();
            return match live {
                // The first operand has every row in play, and asking the threaded kernel for that
                // would be a pass over an identity selection the unthreaded one does not need.
                None => select_prepared(*op, one, other, held),
                Some(live) => refine_prepared(*op, one, other, live, held),
            };
        }
        // A later LIKE in a threaded filter often sees only a handful of survivors.
        // Gather its arguments, not the whole chunk, while preserving the stable
        // dictionary behind a gathered string column. The ordinary full-vector
        // path remains cheaper when most rows are still live.
        if let (Some(live), Step::Function { recipe, written, start, len }) =
            (live, &self.steps[index])
        {
            if matches!(recipe.name(), "~~" | "!~~" | "~~*" | "!~~*")
                && live.len().saturating_mul(4) <= chunk.len()
            {
                let flags = self
                    .with_operands(*start, *len, chunk, &scratch.slots, |args| {
                        let gathered = args
                            .iter()
                            .map(|arg| arg.gather(live.indices()))
                            .collect::<Result<Vec<_>>>()?;
                        let narrowed = gathered.iter().collect::<Vec<_>>();
                        rudb_kernels::call_prepared(
                            recipe,
                            &narrowed,
                            &self.types[index],
                            Some(&|| written.clone()),
                        )
                    })
                    .map_err(|error| error.with_fallback_span(self.spans[index]))?;
                return Ok(selection(&flags, live.len()).compose(live));
            }
        }
        self.run_step(index, chunk, scratch)?;
        let flags = self.operand(index, chunk, &scratch.slots)?;
        match live {
            None => Ok(selection(flags, chunk.len())),
            Some(live) => refine_flags(flags, live),
        }
    }

    /// Runs every step in order, filling the slots.
    fn run(&self, chunk: &Chunk, scratch: &mut Scratch) -> Result<()> {
        scratch.slots.clear();
        scratch.slots.resize_with(self.steps.len(), || None);
        for index in 0..self.steps.len() {
            self.run_step(index, chunk, scratch)?;
        }
        Ok(())
    }

    /// Runs one step and empties the slot of every operand this was the last step to read.
    fn run_step(&self, index: usize, chunk: &Chunk, scratch: &mut Scratch) -> Result<()> {
        let produced = self
            .step(index, chunk, &scratch.slots)
            .map_err(|error| error.with_fallback_span(self.spans[index]))?;
        scratch.slots[index] = produced;
        let slots = &mut scratch.slots;
        self.for_each_operand(index, |operand| {
            if self.last_use[operand] == index {
                slots[operand] = None;
            }
        });
        Ok(())
    }

    /// Runs one step, given what the steps before it produced.
    fn step(
        &self,
        index: usize,
        chunk: &Chunk,
        slots: &[Option<Vector>],
    ) -> Result<Option<Vector>> {
        let ty = &self.types[index];
        let produced = match &self.steps[index] {
            Step::Column(_) => None,
            Step::Constant(value) => Some(Vector::constant(ty.clone(), value.clone(), chunk.len())),
            Step::Cast { input, try_cast } => Some(cast_in_time_zone(
                self.operand(*input, chunk, slots)?,
                ty,
                *try_cast,
                Some(self.time_zone),
            )?),
            Step::Compare { op, left, right, held } => Some(compare_prepared(
                *op,
                self.operand(*left, chunk, slots)?,
                self.operand(*right, chunk, slots)?,
                held.as_ref(),
            )?),
            Step::Conjunction { op, start, len } => {
                Some(
                    self.with_operands(*start, *len, chunk, slots, |children| {
                        combine(*op, children)
                    })?,
                )
            }
            Step::Function { recipe, written, start, len } => {
                Some(self.with_operands(*start, *len, chunk, slots, |args| {
                    rudb_kernels::call_prepared(recipe, args, ty, Some(&|| written.clone()))
                })?)
            }
            Step::InSet { input, members } => {
                Some(in_set(self.operand(*input, chunk, slots)?, members, ty)?)
            }
            Step::Case { arms, otherwise, blend } => {
                Some(self.case(chunk, arms, otherwise.as_ref(), blend.as_ref(), ty)?)
            }
            Step::Fused { fused, fallback } => Some(match fused.run(chunk) {
                Some(answer) => answer,
                None => fallback.evaluate_one(chunk, &mut fallback.scratch())?.clone(),
            }),
            Step::Lambda { inputs, runner, body } => {
                let mut operands = Vec::with_capacity(inputs.len());
                for &input in inputs {
                    operands.push(self.operand(input, chunk, slots)?);
                }
                let mut scratch = body.scratch();
                Some(runner.run(&operands, chunk, &mut |inner| {
                    body.evaluate_one(inner, &mut scratch).cloned()
                })?)
            }
        };
        Ok(produced)
    }

    /// The vector a step produced, or the chunk's column if the step is a column reference.
    fn operand<'v>(
        &self,
        index: usize,
        chunk: &'v Chunk,
        slots: &'v [Option<Vector>],
    ) -> Result<&'v Vector> {
        if let Step::Column(position) = self.steps[index] {
            return chunk.column(position);
        }
        slots[index].as_ref().ok_or_else(|| missing(index))
    }

    /// Hands a kernel the references to an operand list, without allocating for the usual widths.
    ///
    /// One, two and three because those are what a bound tree is made of: every scalar function in
    /// the catalog is unary or binary, a comparison is binary, and a conjunction is two or three
    /// often enough to be worth a line. A stack array for those means a chain of eight additions
    /// makes zero allocations for its operand lists over a chunk instead of eight, and eight
    /// allocations a chunk at the rate a pipeline produces chunks is a real number rather than a
    /// tidiness argument. Anything wider falls back to [`gather`](Self::gather), which is a `Vec`
    /// of pointers and still moves no data.
    fn with_operands<'v, T>(
        &self,
        start: usize,
        len: usize,
        chunk: &'v Chunk,
        slots: &'v [Option<Vector>],
        run: impl FnOnce(&[&'v Vector]) -> Result<T>,
    ) -> Result<T> {
        match self.operands[start..start + len] {
            [a] => run(&[self.operand(a, chunk, slots)?]),
            [a, b] => run(&[self.operand(a, chunk, slots)?, self.operand(b, chunk, slots)?]),
            [a, b, c] => run(&[
                self.operand(a, chunk, slots)?,
                self.operand(b, chunk, slots)?,
                self.operand(c, chunk, slots)?,
            ]),
            _ => {
                let gathered = self.gather(start, len, chunk, slots)?;
                run(&gathered)
            }
        }
    }

    /// References to an operand list, for a kernel that takes a slice of them.
    ///
    /// The `Vec` here is the allocation the module documentation names: it holds pointers rather
    /// than vectors, so it is a dozen bytes an operand and no data moves.
    fn gather<'v>(
        &self,
        start: usize,
        len: usize,
        chunk: &'v Chunk,
        slots: &'v [Option<Vector>],
    ) -> Result<Vec<&'v Vector>> {
        let mut gathered = Vec::with_capacity(len);
        for &operand in &self.operands[start..start + len] {
            gathered.push(self.operand(operand, chunk, slots)?);
        }
        Ok(gathered)
    }

    /// A searched `CASE` over the rows no earlier arm claimed.
    ///
    /// The same shape [`evaluate`](crate::evaluate) has, because the thing that makes it that shape
    /// is a correctness rule rather than a performance one: `CASE WHEN x <> 0 THEN 1 / x ELSE 0 END`
    /// divides by zero on the rows the arm excludes if the arm is evaluated for them.
    ///
    /// Each arm answers the rows no earlier arm claimed, so the answers come back short and out of
    /// order and have to be put back in the order the rows arrived in. That is what [`Assembly`] is:
    /// the arms are laid end to end into one run of data and the interleave is a single typed copy
    /// over it. It used to be a `Vec<Value>` filled a row at a time and handed to
    /// `Vector::from_values`, which is a heap allocation and a drop for every string in the answer.
    /// On the ClickBench query that groups by a `CASE` over `Referer` that was about a quarter of
    /// the whole query.
    ///
    /// What is left of #57 here is the narrowing. An arm still narrows the whole chunk rather than
    /// the columns it reads, and the selection threading that replaces the narrowing entirely is
    /// the item this one was carved out of.
    fn case(
        &self,
        chunk: &Chunk,
        arms: &[PreparedArm],
        otherwise: Option<&Prepared>,
        blend: Option<&Blend>,
        ty: &LogicalType,
    ) -> Result<Vector> {
        let claimed = self.claims(chunk, arms)?;
        if let Some(blend) = blend {
            if let Some(blended) = blended(chunk, &claimed, blend)? {
                return Ok(blended);
            }
        }
        let mut built = Assembly::new(ty.clone(), chunk.len())?;
        let branches = arms.iter().map(|arm| &arm.then).map(Some).chain([otherwise]);
        for (branch, rows) in branches.zip(&claimed) {
            let (Some(branch), false) = (branch, rows.is_empty()) else { continue };
            // The same cut the conditions skip above, skipped here for the same reason: a branch
            // that claimed every row claimed them in order, so narrowing to them is a copy of every
            // column in the chunk to arrive back at the chunk.
            let cut;
            let matched = if rows.len() == chunk.len() {
                chunk
            } else {
                cut = narrow(chunk, rows)?;
                &cut
            };
            let mut scratch = branch.scratch();
            let results = branch.evaluate_one(matched, &mut scratch)?;
            built.place(&placed(rows)?, results)?;
        }
        built.finish()
    }

    /// The rows each branch of a `CASE` answers, one list per arm in order and the `ELSE` last.
    ///
    /// Only the conditions are run here, which is what keeps the rule the doc above states: an arm's
    /// condition is evaluated over the rows no earlier arm claimed, so a condition that would raise
    /// on a row an earlier arm took is never asked about it. The results are worked out afterwards,
    /// once, from these lists, and both ways of working them out want the same thing, which is the
    /// rows of one branch in the order they arrived in.
    fn claims(&self, chunk: &Chunk, arms: &[PreparedArm]) -> Result<Vec<Vec<usize>>> {
        let mut claimed = Vec::with_capacity(arms.len() + 1);
        let mut pending: Vec<usize> = (0..chunk.len()).collect();
        for arm in arms {
            if pending.is_empty() {
                claimed.push(Vec::new());
                continue;
            }
            // `pending` starts as every row in order and only ever shrinks, so the same length is
            // the same rows in the same order and there is nothing to cut. That is the whole of the
            // first arm of a one armed `CASE`, which is the shape of the ClickBench query this was
            // measured on, and cutting it was a copy of every column in the chunk for nothing.
            let cut;
            let narrowed = if pending.len() == chunk.len() {
                chunk
            } else {
                cut = narrow(chunk, &pending)?;
                &cut
            };
            let mut scratch = arm.when.scratch();
            let flags = arm.when.evaluate_one(narrowed, &mut scratch)?;
            let mut taken = Vec::new();
            let mut still = Vec::new();
            // row at a time: splitting the rows an arm claims from the ones it leaves is a test per
            // row, and what replaces it is the selection threading the rest of #57 asks for rather
            // than anything that can be done here.
            for (at, &row) in pending.iter().enumerate() {
                if is_true(&flags.value_at(at)) {
                    taken.push(row);
                } else {
                    still.push(row);
                }
            }
            claimed.push(taken);
            pending = still;
        }
        claimed.push(pending);
        Ok(claimed)
    }

    /// Flattens one expression, appending its steps and returning the index of its last one.
    fn push(&mut self, plan: &Plan, expr: ExprRef, schema: &Schema) -> Result<usize> {
        if self.share {
            if let Some(&step) = self.shared.get(&expr) {
                return Ok(step);
            }
        }
        let ty = plan.expr_type(expr).clone();
        if self.fuse {
            if let Some(fused) = Fused::compile(plan, expr, schema) {
                let fallback = Self::built(plan, &[expr], schema, false, false)?;
                let step = Step::Fused { fused: Box::new(fused), fallback: Box::new(fallback) };
                return Ok(self.place(plan, expr, step, ty));
            }
        }
        if let Some((stamp, count)) = stamped_seconds(plan, expr) {
            let (start, len) = self.push_list(plan, &[stamp, count], schema)?;
            let step = Step::Function {
                recipe: Recipe::new("__rudb_stamp_seconds", &self.literals(start, len)),
                written: written(plan, expr, schema),
                start,
                len,
            };
            return Ok(self.place(plan, expr, step, ty));
        }
        let step = match *plan.expr(expr) {
            Expr::Column(binding) => {
                let position = schema.position_of(binding).ok_or_else(|| {
                    Error::internal(format!(
                        "column #{}.{} is not in the schema this operator was given",
                        binding.table, binding.column
                    ))
                })?;
                Step::Column(position)
            }
            Expr::Constant(reference) => Step::Constant(plan.value(reference).clone()),
            Expr::Cast { input, try_cast } => {
                Step::Cast { input: self.push(plan, input, schema)?, try_cast }
            }
            Expr::Compare { op, left, right } => {
                let left = self.push(plan, left, schema)?;
                let right = self.push(plan, right, schema)?;
                Step::Compare { op: comparison(op), left, right, held: self.held(left, right) }
            }
            Expr::Conjunction { op, children } => {
                let list = plan.expr_list(children).to_vec();
                match self.membership(plan, connective(op), &list, schema)? {
                    Some(step) => step,
                    None => {
                        let (start, len) = self.push_list(plan, &list, schema)?;
                        Step::Conjunction { op: connective(op), start, len }
                    }
                }
            }
            Expr::Function { name, args } if lambda_call(plan, args).is_some() => {
                let Some((lambda, inputs)) = lambda_call(plan, args) else {
                    return Err(Error::internal("a lambda call without a lambda"));
                };
                let Expr::Lambda { body, .. } = *plan.expr(lambda) else {
                    return Err(Error::internal("a lambda call without a lambda"));
                };
                let runner = Lambda::new(plan, plan.string(name), lambda, &inputs, schema)?;
                let body = Self::one(plan, body, runner.schema())?;
                let mut steps = Vec::with_capacity(inputs.len());
                for &input in &inputs {
                    steps.push(self.push(plan, input, schema)?);
                }
                Step::Lambda { inputs: steps, runner: Box::new(runner), body: Box::new(body) }
            }
            Expr::LambdaParam(binding) => {
                let position = schema.position_of(binding).ok_or_else(|| {
                    Error::internal(format!(
                        "lambda parameter @{}.{} is not in the schema its body was given",
                        binding.table, binding.column
                    ))
                })?;
                Step::Column(position)
            }
            Expr::Lambda { .. } => {
                return Err(Error::internal(
                    "a lambda was evaluated outside the function that takes it",
                ));
            }
            Expr::Function { name, args } => {
                let (start, len) = self.push_list(plan, plan.expr_list(args), schema)?;
                Step::Function {
                    recipe: Recipe::new(plan.string(name), &self.literals(start, len)),
                    written: written(plan, expr, schema),
                    start,
                    len,
                }
            }
            Expr::Aggregate { name, .. } => {
                return Err(Error::internal(format!(
                    "the {} aggregate was evaluated as an ordinary expression",
                    plan.string(name)
                )));
            }
            Expr::Window { name, .. } => {
                return Err(Error::internal(format!(
                    "the {} window function was evaluated as an ordinary expression",
                    plan.string(name)
                )));
            }
            Expr::Case { arms, otherwise } => {
                let mut prepared = Vec::new();
                for &arm in plan.arm_list(arms) {
                    prepared.push(PreparedArm {
                        when: Self::one(plan, arm.when, schema)?,
                        then: Self::one(plan, arm.then, schema)?,
                    });
                }
                let otherwise = match otherwise {
                    Some(otherwise) => Some(Self::one(plan, otherwise, schema)?),
                    None => None,
                };
                let blend = blending(&ty, &prepared, otherwise.as_ref());
                Step::Case { arms: prepared, otherwise, blend }
            }
        };
        Ok(self.place(plan, expr, step, ty))
    }

    /// Appends a built step and answers its index.
    fn place(&mut self, plan: &Plan, expr: ExprRef, step: Step, ty: LogicalType) -> usize {
        self.steps.push(step);
        self.types.push(ty);
        self.spans.push(plan.expr_span(expr));
        let step = self.steps.len() - 1;
        if self.share {
            self.shared.insert(expr, step);
        }
        step
    }

    /// Flattens a list of expressions and records where its operand run starts and how long it is.
    ///
    /// The operand run is written after every child has been flattened rather than as they go,
    /// because a child that is itself a list would otherwise interleave its run with this one.
    fn push_list(
        &mut self,
        plan: &Plan,
        exprs: &[ExprRef],
        schema: &Schema,
    ) -> Result<(usize, usize)> {
        let mut indices = Vec::with_capacity(exprs.len());
        for &expr in exprs {
            indices.push(self.push(plan, expr, schema)?);
        }
        let start = self.operands.len();
        let len = indices.len();
        self.operands.extend(indices);
        Ok((start, len))
    }

    /// This connective folded back into the `IN` the user wrote, or `None` when it is not one.
    ///
    /// What the binder writes for `x IN (1, 2, 3)` is `x = 1 OR x = 2 OR x = 3`, and for
    /// `x NOT IN (1, 2, 3)` it is `x <> 1 AND x <> 2 AND x <> 3`. So the shape looked for is every
    /// child a comparison of the one direction, every left the same expression, and every right a
    /// literal. Anything else is left alone, which covers the `OR` that was written as an `OR` and
    /// the one where an `IN` has been flattened together with another branch. The second is a fold
    /// this could make and does not, and it is worth having later out of a query that wants it
    /// rather than now out of a guess.
    ///
    /// This runs before the children are pushed, and that is the whole reason it is here rather than
    /// as a pass over the finished array. A step that nothing reads is still a step the walk runs,
    /// because the walk over a subtree is a range and not a graph, so folding after the fact would
    /// leave every equality in place and running.
    fn membership(
        &mut self,
        plan: &Plan,
        op: Connective,
        children: &[ExprRef],
        schema: &Schema,
    ) -> Result<Option<Step>> {
        let wanted = match op {
            Connective::Or => CompareOp::Equal,
            Connective::And => CompareOp::NotEqual,
        };
        let mut subject: Option<ExprRef> = None;
        let mut values = Vec::with_capacity(children.len());
        for &child in children {
            let Expr::Compare { op: found, left, right } = *plan.expr(child) else {
                return Ok(None);
            };
            if found != wanted || !same(plan, *subject.get_or_insert(left), left) {
                return Ok(None);
            }
            let Expr::Constant(reference) = *plan.expr(right) else {
                return Ok(None);
            };
            values.push(plan.value(reference).clone());
        }
        let (Some(subject), Some(members)) = (subject, Members::of(&values, op == Connective::And))
        else {
            return Ok(None);
        };
        Ok(Some(Step::InSet { input: self.push(plan, subject, schema)?, members }))
    }

    /// The literal side of a comparison, in the one row column the comparison reads it through.
    ///
    /// The right side first, because that is the side the binder puts a literal on and the side the
    /// loops are written for. Two literals is a comparison the optimizer folded, and if it did not
    /// then the kernel answers it once for the whole vector and never reads either column, so
    /// neither side is built here.
    fn held(&self, left: usize, right: usize) -> Option<Held> {
        let (at, other) = match (&self.steps[left], &self.steps[right]) {
            (Step::Constant(_), Step::Constant(_)) => return None,
            (_, Step::Constant(value)) => (right, value),
            (Step::Constant(value), _) => (left, value),
            _ => return None,
        };
        Held::of(&self.types[at], other)
    }

    /// The literal behind each argument in a run of the operand list, and `None` for an argument
    /// that is anything else.
    ///
    /// This is what a [`Recipe`] hoists from. An argument that is a literal in the plan arrives as a
    /// constant vector holding exactly this value on every chunk, so what a kernel reads here is
    /// what it would have read per chunk. An argument that is a cast of a literal reads as `None`,
    /// which is a call the kernel decides per chunk as it always did, and the optimizer folds most
    /// of those before the plan gets here anyway.
    fn literals(&self, start: usize, len: usize) -> Vec<Option<Value>> {
        self.operands[start..start + len]
            .iter()
            .map(|&operand| match &self.steps[operand] {
                Step::Constant(value) => Some(value.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Whether two expressions of one plan are the same expression, written once or written twice.
///
/// The binder binds the subject of an `IN` once and points every comparison it writes at that one
/// reference, so the answer is almost always the first line. A plan that has been through a rewrite,
/// and a plan read back from its own text, hold two copies of the same tree instead, and for the
/// fold in [`Prepared::membership`] those are the same expression.
///
/// The four shapes handled are what an `IN` is written over: a column, a literal, a cast of either,
/// and a call, which is TPC-H query 22 asking whether the first two digits of a phone number are in
/// a list. Anything else answers no, which costs a fold that could have happened rather than a wrong
/// one. The walk is bounded by the size of the subject and a subject is small.
/// The timestamp and the whole count of `stamp + to_seconds(CAST(count AS DOUBLE))`, the shape the
/// benchmark view writes `INTERVAL (EventTime) SECOND` in, and `None` for anything else.
///
/// It runs as one call, [`rudb_kernels`]'s `__rudb_stamp_seconds`, rather than as a cast to a
/// double, an interval per row and a shift by it.
fn stamped_seconds(plan: &Plan, expr: ExprRef) -> Option<(ExprRef, ExprRef)> {
    let Expr::Function { name, args } = *plan.expr(expr) else { return None };
    if plan.string(name) != "+" || plan.expr_type(expr) != &LogicalType::Timestamp {
        return None;
    }
    let &[one, other] = plan.expr_list(args) else { return None };
    let (stamp, interval) =
        if plan.expr_type(one) == &LogicalType::Timestamp { (one, other) } else { (other, one) };
    if plan.expr_type(stamp) != &LogicalType::Timestamp {
        return None;
    }
    let Expr::Function { name, args } = *plan.expr(interval) else { return None };
    let &[cast] = plan.expr_list(args) else { return None };
    let Expr::Cast { input, try_cast: false } = *plan.expr(cast) else { return None };
    let whole = matches!(
        plan.expr_type(input),
        LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
    );
    (plan.string(name) == "to_seconds" && plan.expr_type(cast) == &LogicalType::Double && whole)
        .then_some((stamp, input))
}

fn same(plan: &Plan, left: ExprRef, right: ExprRef) -> bool {
    if left == right {
        return true;
    }
    if plan.expr_type(left) != plan.expr_type(right) {
        return false;
    }
    match (plan.expr(left), plan.expr(right)) {
        (Expr::Column(one), Expr::Column(other)) => one == other,
        (Expr::Constant(one), Expr::Constant(other)) => plan.value(*one) == plan.value(*other),
        (
            Expr::Cast { input: one, try_cast: first },
            Expr::Cast { input: other, try_cast: second },
        ) => first == second && same(plan, *one, *other),
        (
            Expr::Function { name: one, args: first },
            Expr::Function { name: other, args: second },
        ) => {
            let (first, second) = (plan.expr_list(*first), plan.expr_list(*second));
            plan.string(*one) == plan.string(*other)
                && first.len() == second.len()
                && first.iter().zip(second).all(|(&one, &other)| same(plan, one, other))
        }
        _ => false,
    }
}

/// What touching a value of this type costs, against a fixed width one as the unit.
///
/// A variable length value is a pointer to follow and a length that is not the same twice, and a
/// nested one is that per element. Four is not measured, and what it has to be is large enough that
/// the ordering puts a fixed width comparison in front of a string one and small enough that it does
/// not put one in front of a string comparison that rejects every row.
fn touching(ty: &LogicalType) -> f64 {
    match ty.physical() {
        PhysicalType::Varlen => 4.0,
        PhysicalType::List | PhysicalType::Array | PhysicalType::Struct => 8.0,
        _ => 1.0,
    }
}

/// The error for a slot that should have held something and did not.
///
/// This cannot happen while the array is in post order, since every operand's index is smaller than
/// the index of the step using it and every step runs in order. It is an error rather than a panic
/// because the property it depends on is a property of [`Prepared::push`], and the day somebody
/// writes a pass that reorders the array is the day it stops holding.
fn missing(index: usize) -> Error {
    Error::internal(format!("step {index} was used as an operand before it produced anything"))
}

/// Chunk rows as the positions an [`Assembly`] places a piece at.
///
/// A chunk is at most [`VECTOR_SIZE`](rudb_vector::VECTOR_SIZE) rows, so the conversion cannot fail
/// in practice. It is checked rather than cast because a silent truncation here would put a value in
/// the wrong row, and a wrong row is the one kind of bug nothing downstream can notice.
fn placed(rows: &[usize]) -> Result<Vec<u32>> {
    rows.iter()
        .map(|&row| {
            u32::try_from(row).map_err(|_| Error::internal("a chunk of more than u32 rows"))
        })
        .collect()
}

/// A `CASE` answered as codes over the dictionary its branches share, or `None` for a chunk that
/// cannot be.
///
/// Declined per chunk rather than once, because whether a column arrives coded is a fact about the
/// chunk and not about the expression. The same query reads codes out of a native file and plain
/// strings out of rows held in memory, and one file can hand a column over as a dictionary in one
/// part and as plain data in the next. Everything that declines does so before a code is written, so
/// the caller starts the general path from nothing rather than from a half filled answer.
fn blended(chunk: &Chunk, claimed: &[Vec<usize>], blend: &Blend) -> Result<Option<Vector>> {
    let Some((dictionary, literals)) = agreed(chunk, blend)? else { return Ok(None) };
    let mut codes = vec![0; chunk.len()];
    for (branch, rows) in blend.branches.iter().zip(claimed) {
        match *branch {
            Branch::Column(position) => {
                let Some((from, _)) = chunk.column(position)?.stable_dictionary_parts() else {
                    return Ok(None);
                };
                for &row in rows {
                    codes[row] = from[row];
                }
            }
            Branch::Literal(at) => {
                for &row in rows {
                    codes[row] = literals[at];
                }
            }
        }
    }
    Vector::stable_dictionary(codes, dictionary).map(Some)
}

/// The one dictionary every branch of a blend names values in, and the code each literal sits at.
///
/// Three things say no. A column that did not arrive as a stable dictionary has no codes to copy. A
/// second column over a different dictionary would have codes that mean something else, and a code
/// is a position in one dictionary and nothing anywhere else. And a literal the dictionary does not
/// hold has no code at all, which for `ELSE ''` over a column where no row is empty is the honest
/// answer rather than a missing one.
///
/// The null check is the fourth. A dictionary keeps its nulls in the values it points at rather than
/// beside its codes, so a column carrying its own validity is one whose codes do not say everything
/// the column says, and copying them would turn its nulls into whatever their codes happen to name.
fn agreed(chunk: &Chunk, blend: &Blend) -> Result<Option<(Arc<Vector>, Vec<u32>)>> {
    let mut held: Option<(&Vector, &Arc<Vector>)> = None;
    for branch in &blend.branches {
        let Branch::Column(position) = *branch else { continue };
        let column = chunk.column(position)?;
        let Some((_, dictionary)) = column.stable_dictionary_parts() else { return Ok(None) };
        if column.validity().has_nulls(chunk.len()) {
            return Ok(None);
        }
        match held {
            Some((_, first)) if !Arc::ptr_eq(first, dictionary) => return Ok(None),
            Some(_) => {}
            None => held = Some((column, dictionary)),
        }
    }
    let Some((column, dictionary)) = held else { return Ok(None) };
    let mut codes = Vec::with_capacity(blend.literals.len());
    for (text, lookup) in &blend.literals {
        match lookup.find(column, text.as_bytes()) {
            Some(Ok(Found::At(code))) => codes.push(code),
            Some(Err(error)) => return Err(error),
            Some(Ok(Found::Absent)) | None => return Ok(None),
        }
    }
    Ok(Some((Arc::clone(dictionary), codes)))
}

/// The blend a `CASE` can be answered by, or `None` for one that has to read its branches' values.
fn blending(ty: &LogicalType, arms: &[PreparedArm], otherwise: Option<&Prepared>) -> Option<Blend> {
    if !matches!(ty, LogicalType::Varchar) {
        return None;
    }
    let otherwise = otherwise?;
    let mut branches = Vec::with_capacity(arms.len() + 1);
    let mut literals = Vec::new();
    for branch in arms.iter().map(|arm| &arm.then).chain([otherwise]) {
        branches.push(named(branch, &mut literals)?);
    }
    // All of them literals means there is no dictionary to name any of them in, and a `CASE` whose
    // every branch is a constant is not a thing anybody writes.
    let any = branches.iter().any(|branch| matches!(branch, Branch::Column(_)));
    any.then_some(Blend { branches, literals })
}

/// The branch a prepared expression stands for, when it names a value rather than computing one.
fn named(prepared: &Prepared, literals: &mut Vec<(String, Lookup)>) -> Option<Branch> {
    match prepared.steps.as_slice() {
        [Step::Column(position)] => Some(Branch::Column(*position)),
        [Step::Constant(Value::Varchar(text))] => {
            literals.push((text.clone(), Lookup::default()));
            Some(Branch::Literal(literals.len() - 1))
        }
        _ => None,
    }
}

/// The chunk cut down to the given rows.
///
/// The reason `CASE` is written with this rather than by evaluating every arm over the whole chunk
/// and picking afterwards. `CASE WHEN x <> 0 THEN 1 // x ELSE 0 END` divides by zero on the rows the
/// arm does not apply to if the arm is evaluated for them, and a `CASE` that raises on a row it was
/// written to exclude is the classic wrong answer this shape prevents.
pub(crate) fn narrow(chunk: &Chunk, rows: &[usize]) -> Result<Chunk> {
    let mut selection = Selection::with_capacity(rows.len());
    for &row in rows {
        selection.push(row);
    }
    chunk.clone().select(&selection)
}

/// The kernels' comparison for the plan's.
///
/// A translation rather than one shared enum, because the kernels are rank 3 and the plan is rank
/// 9. This function is the whole of what that separation costs.
pub(crate) fn comparison(op: CompareOp) -> Comparison {
    match op {
        CompareOp::Equal => Comparison::Equal,
        CompareOp::NotEqual => Comparison::NotEqual,
        CompareOp::Less => Comparison::Less,
        CompareOp::LessOrEqual => Comparison::LessOrEqual,
        CompareOp::Greater => Comparison::Greater,
        CompareOp::GreaterOrEqual => Comparison::GreaterOrEqual,
        CompareOp::DistinctFrom => Comparison::DistinctFrom,
        CompareOp::NotDistinctFrom => Comparison::NotDistinctFrom,
    }
}

/// The kernels' connective for the plan's.
pub(crate) fn connective(op: ConjunctionOp) -> Connective {
    match op {
        ConjunctionOp::And => Connective::And,
        ConjunctionOp::Or => Connective::Or,
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::{Field, LogicalType, Value};
    use rudb_kernels::is_true;
    use rudb_plan::{ExprRef, Node, Plan};
    use rudb_vector::{Chunk, Selection, Vector};

    use super::{Prepared, narrow};
    use crate::expr::evaluate;
    use crate::schema::Schema;

    /// Two columns with a null in each, because every disagreement between these two evaluators
    /// that is worth finding is a disagreement about which rows are null.
    fn input() -> (Schema, Chunk) {
        let schema = Schema::numbered(
            vec![Field::new("x", LogicalType::Integer), Field::new("s", LogicalType::Varchar)],
            0,
        );
        let x = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(3), Value::Integer(1), Value::Null, Value::Integer(2)],
        )
        .expect("four integers");
        let s = Vector::from_values(
            LogicalType::Varchar,
            &[
                Value::Varchar("a".to_string()),
                Value::Null,
                Value::Varchar("c".to_string()),
                Value::Varchar("a".to_string()),
            ],
        )
        .expect("four strings");
        (schema, Chunk::new(vec![x, s]).expect("two columns of four rows"))
    }

    /// The expressions of a projection written in the plan's textual form, over the two columns
    /// [`input`] produces.
    ///
    /// Going through the text rather than the arena builders for the reason the other test module
    /// gives: a test that says what it evaluates in the notation a plan dump uses is a test whose
    /// failure can be pasted into a plan and vice versa.
    fn projection(exprs: &str) -> (Plan, Vec<ExprRef>) {
        let text =
            format!("Project #1 [{exprs}]\n  Get memory.main.t AS t #0 [x::INTEGER, s::VARCHAR]");
        let plan = Plan::parse(&text).expect("a well formed plan");
        let Node::Project { exprs, .. } = *plan.node(plan.root()) else {
            panic!("the root of that text is a projection");
        };
        let list = plan.expr_list(exprs).to_vec();
        (plan, list)
    }

    /// Every expression shape, evaluated both ways over the same chunk.
    ///
    /// This is the agreement the module documentation claims and it is the only thing that makes
    /// the prepared form safe to put in front of the tree walk. The generated well typed trees the
    /// test gate of #57 asks for are a wider version of this and are worth building once the
    /// selection threaded shapes exist to disagree about.
    fn agrees(exprs: &str) {
        let (schema, chunk) = input();
        let (plan, list) = projection(exprs);
        let prepared = Prepared::new(&plan, &list, &schema).expect("the expressions resolve");
        let mut scratch = prepared.scratch();
        let mut fast = Vec::new();
        prepared.evaluate(&chunk, &mut scratch, &mut fast).expect("the prepared form runs");
        for (at, &expr) in list.iter().enumerate() {
            let slow = evaluate(&plan, expr, &schema, &chunk).expect("the tree walk runs");
            for row in 0..chunk.len() {
                assert_eq!(
                    fast[at].value_at(row),
                    slow.value_at(row),
                    "expression {at} of `{exprs}` at row {row}"
                );
            }
        }
    }

    /// Three decimal columns of TPC-H's shape, in the form `form` puts them in.
    fn decimals(prices: &[i128], form: fn(Vector) -> Vector) -> (Schema, Chunk) {
        let ty = LogicalType::Decimal { width: 15, scale: 2 };
        let schema = Schema::numbered(
            vec![
                Field::new("p", ty.clone()),
                Field::new("d", ty.clone()),
                Field::new("t", ty.clone()),
            ],
            0,
        );
        let column =
            |values: Vec<Value>| form(Vector::from_values(ty.clone(), &values).expect("decimals"));
        let decimal = |unscaled| Value::Decimal { unscaled, width: 15, scale: 2 };
        let p = column(prices.iter().map(|&v| decimal(v)).collect());
        let d = column((0..prices.len() as i128).map(|v| decimal(v % 11)).collect());
        let t = column((0..prices.len() as i128).map(|v| decimal(v % 9)).collect());
        (schema, Chunk::new(vec![p, d, t]).expect("three columns"))
    }

    /// q01's charge, as the binder writes it.
    const CHARGE: &str = "\"*\"(\"*\"(CAST(#0.0::DECIMAL(15,2))::DECIMAL(18,2), \
        CAST(\"-\"(1.00::DECIMAL(16,2), CAST(#0.1::DECIMAL(15,2))::DECIMAL(16,2))::DECIMAL(16,2))\
        ::DECIMAL(18,2))::DECIMAL(18,4), CAST(\"+\"(1.00::DECIMAL(16,2), \
        CAST(#0.2::DECIMAL(15,2))::DECIMAL(16,2))::DECIMAL(16,2))::DECIMAL(18,2))::DECIMAL(18,6) AS a";

    /// The fused answer, the unfused one and the tree walk's, over one chunk.
    fn three_ways(chunk: &Chunk, schema: &Schema) -> [rudb_common::Result<Vec<Value>>; 3] {
        let text = format!(
            "Project #1 [{CHARGE}]\n  Get memory.main.t AS t #0 \
             [p::DECIMAL(15,2), d::DECIMAL(15,2), t::DECIMAL(15,2)]"
        );
        let plan = Plan::parse(&text).expect("a well formed plan");
        let Node::Project { exprs, .. } = *plan.node(plan.root()) else {
            panic!("the root of that text is a projection");
        };
        let expr = plan.expr_list(exprs)[0];
        let values = |vector: &Vector| (0..chunk.len()).map(|row| vector.value_at(row)).collect();
        let fused = Prepared::one(&plan, expr, schema).expect("resolves");
        assert_eq!(fused.fused(), 1, "the whole tree is one step");
        let unfused = Prepared::built(&plan, &[expr], schema, false, false).expect("resolves");
        assert_eq!(unfused.fused(), 0);
        let run = |prepared: &Prepared| {
            prepared.evaluate_one(chunk, &mut prepared.scratch()).map(&values)
        };
        [run(&fused), run(&unfused), evaluate(&plan, expr, schema, chunk).map(|v| values(&v))]
    }

    fn all_agree(chunk: &Chunk, schema: &Schema) {
        let [fused, unfused, walked] = three_ways(chunk, schema);
        let fused = fused.expect("fits");
        assert_eq!(fused, unfused.expect("fits"));
        assert_eq!(fused, walked.expect("fits"));
    }

    /// The epoch plus a whole count of seconds runs as one call, and agrees with the cast, the
    /// interval and the shift it stands for, on both sides of the count where the double stops
    /// being exact and on a count that takes the answer out of range.
    #[test]
    fn a_timestamp_plus_whole_seconds_agrees_with_the_interval_it_stands_for() {
        let schema = Schema::numbered(vec![Field::new("x", LogicalType::BigInt)], 0);
        let counts = [
            Value::BigInt(1_373_000_000),
            Value::BigInt(-5),
            Value::Null,
            Value::BigInt(9_007_199_254),
            Value::BigInt(9_007_199_255),
            Value::BigInt(9_000_000_000_123),
        ];
        let x = Vector::from_values(LogicalType::BigInt, &counts).expect("six counts");
        let chunk = Chunk::new(vec![x]).expect("one column");
        let text = "Project #1 [\"+\"(0::TIMESTAMP, to_seconds(CAST(#0.0::BIGINT)::DOUBLE)::INTERVAL)::TIMESTAMP AS e]\n  Get memory.main.t AS t #0 [x::BIGINT]";
        let plan = Plan::parse(text).expect("a well formed plan");
        let Node::Project { exprs, .. } = *plan.node(plan.root()) else {
            panic!("the root of that text is a projection");
        };
        let list = plan.expr_list(exprs).to_vec();
        let prepared = Prepared::new(&plan, &list, &schema).expect("the expression resolves");
        assert!(
            prepared.steps.iter().any(
                |step| matches!(step, super::Step::Function { recipe, .. } if recipe.name() == "__rudb_stamp_seconds")
            ),
            "the shift is one call"
        );
        let mut scratch = prepared.scratch();
        let mut fast = Vec::new();
        prepared.evaluate(&chunk, &mut scratch, &mut fast).expect("the prepared form runs");
        let slow = evaluate(&plan, list[0], &schema, &chunk).expect("the tree walk runs");
        for row in 0..chunk.len() {
            assert_eq!(fast[0].value_at(row), slow.value_at(row), "row {row}");
        }
        assert_eq!(fast[0].value_at(0), Value::Timestamp(1_373_000_000_000_000));

        let far = Vector::from_values(LogicalType::BigInt, &[Value::BigInt(9_300_000_000_000)])
            .expect("one count");
        let chunk = Chunk::new(vec![far]).expect("one column");
        let mut fast = Vec::new();
        let fused = prepared.evaluate(&chunk, &mut scratch, &mut fast);
        let slow = evaluate(&plan, list[0], &schema, &chunk).map(|_| ());
        assert!(fused.is_err() && slow.is_err(), "past the last timestamp both raise");
    }

    #[test]
    fn decimal_arithmetic_run_as_one_loop_agrees_in_every_form() {
        let prices: Vec<i128> = (0..2500).map(|v| 90_000 + v * 37).collect();
        let packed = |vector: Vector| vector.bit_packed().expect("packs");
        let coded = |vector: Vector| {
            let rows = vector.len();
            let codes = (0..rows as u32).rev().collect();
            Vector::dictionary(codes, vector.bit_packed().expect("packs")).expect("in range")
        };
        // Codes too far apart for a block to unpack the run they cover.
        let scattered = |vector: Vector| {
            let rows = vector.len() as u32;
            let codes = (0..rows).map(|row| row * 997 % rows).collect();
            Vector::dictionary(codes, vector.bit_packed().expect("packs")).expect("in range")
        };
        for form in [std::convert::identity, packed, coded, scattered] {
            let (schema, chunk) = decimals(&prices, form);
            all_agree(&chunk, &schema);
        }
    }

    #[test]
    fn a_chunk_the_ranges_cannot_prove_raises_what_the_steps_raise() {
        // The large price in the second block, so a flat column gets as far as running the first.
        let mut prices = vec![5; 300];
        prices.push(999_999_999_999_999);
        let packed = |vector: Vector| vector.bit_packed().expect("packs");
        for form in [std::convert::identity, packed] {
            let (schema, chunk) = decimals(&prices, form);
            let [fused, unfused, _] = three_ways(&chunk, &schema);
            let (fused, unfused) = (fused.expect_err("overflows"), unfused.expect_err("overflows"));
            assert_eq!(fused.message(), unfused.message());
        }
    }

    #[test]
    fn a_chunk_with_a_null_goes_through_the_steps() {
        let ty = LogicalType::Decimal { width: 15, scale: 2 };
        let (schema, mut chunk) = decimals(&[100, 200, 300], std::convert::identity);
        let with_null = Vector::from_values(
            ty,
            &[Value::Decimal { unscaled: 5, width: 15, scale: 2 }, Value::Null, Value::Null],
        )
        .expect("decimals");
        chunk = Chunk::new(vec![
            chunk.column(0).expect("p").clone(),
            with_null,
            chunk.column(2).expect("t").clone(),
        ])
        .expect("three columns");
        all_agree(&chunk, &schema);
    }

    #[test]
    fn a_column_reference_agrees() {
        agrees("#0.0::INTEGER AS a, #0.1::VARCHAR AS b");
    }

    #[test]
    fn a_constant_agrees() {
        agrees("7::INTEGER AS a, NULL::INTEGER AS b");
    }

    #[test]
    fn a_cast_agrees() {
        agrees("CAST(#0.0::INTEGER)::BIGINT AS a, CAST(#0.0::INTEGER)::VARCHAR AS b");
    }

    #[test]
    fn a_comparison_agrees() {
        agrees("(#0.0::INTEGER > 1::INTEGER)::BOOLEAN AS a");
    }

    #[test]
    fn a_conjunction_agrees() {
        agrees(
            "((#0.0::INTEGER > 1::INTEGER)::BOOLEAN AND (#0.0::INTEGER < 3::INTEGER)::BOOLEAN)\
             ::BOOLEAN AS a",
        );
    }

    #[test]
    fn a_function_agrees() {
        agrees("\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER AS a");
    }

    /// The two evaluators quote the same expression when a divisor is zero. Per #262.
    ///
    /// This is the one message in the engine that depends on how an expression is written rather
    /// than on what it computes, and the two evaluators render it at different times: the prepared
    /// form when the pipeline is built, the tree walk on the row that fails. Same renderer, so the
    /// same sentence, and this is what says so.
    #[test]
    fn both_evaluators_quote_the_same_expression_when_a_divisor_is_zero() {
        let (schema, chunk) = input();
        let (plan, list) = projection("\"//\"(#0.0::INTEGER, 0::INTEGER)::INTEGER AS a");
        let prepared = Prepared::new(&plan, &list, &schema).expect("the expression resolves");
        let mut scratch = prepared.scratch();
        let mut out = Vec::new();
        let fast = prepared.evaluate(&chunk, &mut scratch, &mut out).expect_err("divides by zero");
        let slow = evaluate(&plan, list[0], &schema, &chunk).expect_err("divides by zero");
        assert_eq!(fast.message(), slow.message());
        assert!(fast.message().starts_with("Division by zero in expression (x // 0)."), "{fast}");
    }

    #[test]
    fn a_case_agrees() {
        agrees(
            "CASE WHEN (#0.0::INTEGER > 1::INTEGER)::BOOLEAN THEN 10::INTEGER \
             ELSE 20::INTEGER END::INTEGER AS a",
        );
    }

    /// A second arm, which is the first one that sees a cut chunk rather than the whole one.
    ///
    /// The first arm of any `CASE` runs over every row, so it takes the path that does not cut at
    /// all, and a `CASE` of one arm never exercises the other one. Two arms and an `ELSE` puts a
    /// different set of rows in front of each of the three.
    ///
    /// That this is the only test here reaching the cut was checked rather than assumed, by gating a
    /// panic on it and rerunning the seven. This one failed and the other six did not.
    #[test]
    fn a_case_of_two_arms_agrees() {
        agrees(
            "CASE WHEN (#0.0::INTEGER > 2::INTEGER)::BOOLEAN THEN 10::INTEGER \
             WHEN (#0.0::INTEGER > 1::INTEGER)::BOOLEAN THEN 20::INTEGER \
             ELSE 30::INTEGER END::INTEGER AS a",
        );
    }

    /// No `ELSE`, so the rows no arm claims are null rather than anything.
    ///
    /// The case a run of data with a hole in it gets wrong: a null still occupies a position, and an
    /// assembly that skipped it would put every value after it one row early.
    #[test]
    fn a_case_with_no_else_agrees() {
        agrees(
            "CASE WHEN (#0.0::INTEGER > 2::INTEGER)::BOOLEAN THEN 10::INTEGER \
             END::INTEGER AS a",
        );
    }

    /// An arm no row takes, so it contributes nothing to the answer and must not shift it.
    #[test]
    fn a_case_whose_arm_claims_nothing_agrees() {
        agrees(
            "CASE WHEN (#0.0::INTEGER > 99::INTEGER)::BOOLEAN THEN 10::INTEGER \
             ELSE 20::INTEGER END::INTEGER AS a",
        );
    }

    /// Strings, which is the case that used to allocate one of them per row and drop it afterwards.
    ///
    /// The arm reads a column and the `ELSE` is a constant, which is the shape of the ClickBench
    /// query this path was rewritten for: the arm arrives as views over an arena and the `ELSE` as
    /// one value repeated, and the two have to be laid end to end into a single arena.
    #[test]
    fn a_case_over_strings_agrees() {
        agrees(
            "CASE WHEN (#0.0::INTEGER > 1::INTEGER)::BOOLEAN THEN #0.1::VARCHAR \
             ELSE ''::VARCHAR END::VARCHAR AS a",
        );
    }

    /// A null inside an arm, which is a different thing from a row no arm claimed.
    ///
    /// Both come out null and they reach the validity mask by different routes, so a mask built for
    /// one of them and not the other reads correct on whichever test only has the other in it.
    #[test]
    fn a_case_whose_arm_answers_null_agrees() {
        agrees(
            "CASE WHEN (#0.0::INTEGER > 1::INTEGER)::BOOLEAN THEN #0.1::VARCHAR \
             ELSE NULL::VARCHAR END::VARCHAR AS a",
        );
    }

    /// A `WHEN` over a column that is null on some rows, which is neither true nor false there.
    ///
    /// A three valued `WHEN` is what decides whether a row goes to the arm or falls through, and
    /// treating unknown as true would claim a row the `ELSE` should have had.
    #[test]
    fn a_case_whose_test_is_null_on_some_rows_agrees() {
        agrees(
            "CASE WHEN (#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN THEN 10::INTEGER \
             ELSE 20::INTEGER END::INTEGER AS a",
        );
    }

    /// The same expression twice, which is where the tree walk copies the column twice and this
    /// does not, and the answers still have to be identical.
    #[test]
    fn a_column_mentioned_three_times_agrees() {
        agrees("\"+\"(\"+\"(#0.0::INTEGER, #0.0::INTEGER)::INTEGER, #0.0::INTEGER)::INTEGER AS a");
    }

    /// The intermediates of a chain are not all held to the end of it.
    ///
    /// This is the whole difference between the prepared form being faster than the tree walk on a
    /// deep chain and being slower than it, and it is a property of the slot array rather than of
    /// any answer, so it is asserted here rather than left to the benchmark to catch.
    #[test]
    fn a_chain_holds_one_intermediate_at_a_time() {
        let (schema, chunk) = input();
        let mut expr = "#0.0::INTEGER".to_string();
        for _ in 0..8 {
            expr = format!("\"+\"({expr}, 1::INTEGER)::INTEGER");
        }
        let (plan, list) = projection(&format!("{expr} AS a"));
        let prepared = Prepared::new(&plan, &list, &schema).expect("the chain resolves");
        let mut scratch = prepared.scratch();
        prepared.run(&chunk, &mut scratch).expect("the chain runs");
        let live = scratch.slots.iter().filter(|slot| slot.is_some()).count();
        assert_eq!(live, 1, "a chain that has run should be holding its answer and nothing else");
    }

    /// The rows a threaded filter keeps are the rows the tree walk says the predicate is true for.
    ///
    /// Every threaded conjunct is a chance to disagree with the unthreaded answer about a null,
    /// about a row an earlier conjunct had already dropped, or about a chunk nothing survives, and
    /// the answer is a set of row numbers rather than a vector, so this is checked against the tree
    /// walk read a row at a time rather than against the prepared form it is part of.
    fn filters(predicate: &str) {
        let (schema, chunk) = input();
        let (plan, list) = projection(&format!("{predicate} AS p"));
        let prepared = Prepared::new(&plan, &list, &schema).expect("the predicate resolves");
        let mut scratch = prepared.scratch();
        let threaded = prepared.evaluate_filter(&chunk, &mut scratch).expect("the filter runs");
        let flags = evaluate(&plan, list[0], &schema, &chunk).expect("the tree walk runs");
        let expected = Selection::from_predicate(chunk.len(), |row| is_true(&flags.value_at(row)));
        assert_eq!(threaded, expected, "`{predicate}`");
        // And running it again over the same scratch is the same answer, because a pipeline calls
        // this once a chunk and a slot left behind by the conjunct before would show up here.
        let again = prepared.evaluate_filter(&chunk, &mut scratch).expect("the filter runs again");
        assert_eq!(again, expected, "`{predicate}` a second time");
    }

    /// A predicate with no `AND` in it is not threaded and has to keep saying the same thing.
    #[test]
    fn a_single_comparison_filters_the_same_rows() {
        filters("(#0.0::INTEGER > 1::INTEGER)::BOOLEAN");
        filters("(#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN");
        filters("(#0.0::INTEGER IS NOT DISTINCT FROM NULL::INTEGER)::BOOLEAN");
    }

    #[test]
    fn a_chain_of_conjuncts_keeps_what_all_of_them_keep() {
        filters(
            "((#0.0::INTEGER > 1::INTEGER)::BOOLEAN AND (#0.0::INTEGER < 3::INTEGER)::BOOLEAN)\
             ::BOOLEAN",
        );
        filters(
            "((#0.0::INTEGER >= 1::INTEGER)::BOOLEAN AND (#0.0::INTEGER <= 3::INTEGER)::BOOLEAN \
             AND (#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN AND (#0.0::INTEGER <> 2::INTEGER)\
             ::BOOLEAN)::BOOLEAN",
        );
    }

    /// A conjunct that rejects every row, in front of one that would have kept some. The rows are
    /// the same either way and the point of the shape is that the second conjunct never runs.
    #[test]
    fn a_conjunct_that_keeps_nothing_ends_the_predicate() {
        filters(
            "((#0.0::INTEGER > 9::INTEGER)::BOOLEAN AND (#0.0::INTEGER < 9::INTEGER)::BOOLEAN)\
             ::BOOLEAN",
        );
    }

    /// A conjunct whose operands are computed rather than read, which is the shape where the
    /// comparison is threaded and the arithmetic under it is not.
    #[test]
    fn a_conjunct_over_a_computed_operand_keeps_the_same_rows() {
        filters(
            "((#0.0::INTEGER > 1::INTEGER)::BOOLEAN AND \
             (\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER < 4::INTEGER)::BOOLEAN)::BOOLEAN",
        );
    }

    /// A conjunct that is not a comparison at all, which is the one that goes through the flag
    /// kernel rather than the comparison kernel.
    #[test]
    fn a_conjunct_that_is_not_a_comparison_is_threaded_too() {
        filters(
            "((#0.0::INTEGER > 1::INTEGER)::BOOLEAN AND ((#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN \
             OR (#0.0::INTEGER = 1::INTEGER)::BOOLEAN)::BOOLEAN)::BOOLEAN",
        );
        filters(
            "(((#0.1::VARCHAR = 'c'::VARCHAR)::BOOLEAN OR (#0.0::INTEGER = 3::INTEGER)::BOOLEAN)\
             ::BOOLEAN AND (#0.0::INTEGER <> 1::INTEGER)::BOOLEAN)::BOOLEAN",
        );
    }

    #[test]
    fn a_selective_conjunct_evaluates_later_like_on_its_survivors() {
        filters(
            "((#0.0::INTEGER > 2::INTEGER)::BOOLEAN AND \
             \"~~\"(#0.1::VARCHAR, '%a%'::VARCHAR)::BOOLEAN)::BOOLEAN",
        );
        filters(
            "((#0.0::INTEGER > 2::INTEGER)::BOOLEAN AND \
             \"!~~\"(#0.1::VARCHAR, '%a%'::VARCHAR)::BOOLEAN)::BOOLEAN",
        );
    }

    /// An `OR` at the top threads the complement: the second branch only sees the rows the first
    /// one did not accept, and the rows it accepts are added to them rather than replacing them.
    ///
    /// The input has a row where the first branch is true, one where the second is, one where both
    /// are false and one where the first is null and the second is true, which is the row that says
    /// whether the complement was taken over "not true" or over "false".
    #[test]
    fn an_or_at_the_top_threads_the_complement() {
        filters(
            "((#0.0::INTEGER > 2::INTEGER)::BOOLEAN OR (#0.1::VARCHAR = 'c'::VARCHAR)::BOOLEAN)\
             ::BOOLEAN",
        );
        filters(
            "((#0.0::INTEGER = 1::INTEGER)::BOOLEAN OR (#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN \
             OR (#0.0::INTEGER > 2::INTEGER)::BOOLEAN)::BOOLEAN",
        );
    }

    /// A branch that accepts every row, in front of one that would have accepted none. The rows are
    /// the same either way and the point of the shape is that the second branch never runs.
    #[test]
    fn a_branch_that_keeps_everything_ends_the_predicate() {
        filters(
            "((#0.0::INTEGER IS NOT DISTINCT FROM #0.0::INTEGER)::BOOLEAN OR \
             (#0.0::INTEGER > 9::INTEGER)::BOOLEAN)::BOOLEAN",
        );
    }

    /// The branches after one that has accepted every row really are skipped.
    ///
    /// Every other test here says the threaded answer matches the unthreaded one, which it would
    /// even if nothing were threaded at all. This one puts a division by zero behind a branch that
    /// accepts everything, so the predicate raises if the second branch runs and does not if the
    /// walk stopped where it was supposed to.
    #[test]
    fn a_branch_behind_one_that_accepted_every_row_does_not_run() {
        let (schema, chunk) = input();
        let predicate = "((#0.0::INTEGER IS NOT DISTINCT FROM #0.0::INTEGER)::BOOLEAN OR \
                         (\"//\"(#0.0::INTEGER, 0::INTEGER)::INTEGER > 0::INTEGER)::BOOLEAN)\
                         ::BOOLEAN";
        let (plan, list) = projection(&format!("{predicate} AS p"));
        let prepared = Prepared::new(&plan, &list, &schema).expect("the predicate resolves");
        let mut scratch = prepared.scratch();
        let kept =
            prepared.evaluate_filter(&chunk, &mut scratch).expect("the second branch never runs");
        assert_eq!(kept, Selection::identity(chunk.len()));
        // And the same predicate evaluated as an expression does divide by zero, which is what says
        // the test is testing the threading rather than a predicate that happens not to raise.
        evaluate(&plan, list[0], &schema, &chunk).expect_err("the tree walk divides by zero");
    }

    /// The conjunct that rejects the most rows ends up in front of the one that rejects none.
    ///
    /// The predicate is written the wrong way round on purpose. The plan order costs two passes a
    /// chunk where one would do, and after a chunk of watching it the filter runs the selective one
    /// first and the other one stops running at all.
    #[test]
    fn a_filter_learns_which_conjunct_to_run_first() {
        let (schema, chunk) = input();
        let predicate = "((#0.0::INTEGER > 0::INTEGER)::BOOLEAN AND (#0.0::INTEGER > 9::INTEGER)\
                         ::BOOLEAN)::BOOLEAN";
        let (plan, list) = projection(&format!("{predicate} AS p"));
        let prepared = Prepared::new(&plan, &list, &schema).expect("the predicate resolves");
        let mut scratch = prepared.scratch();
        let root = prepared.roots[0];
        assert_eq!(scratch.order(root), None, "nothing has run yet");
        let kept = prepared.evaluate_filter(&chunk, &mut scratch).expect("the filter runs");
        assert!(kept.is_empty());
        assert_eq!(scratch.order(root), Some(&[1, 0][..]), "the second conjunct rejects the most");
        // And it stays there, because the conjunct that now runs first empties the selection and
        // the one behind it keeps the history it already had rather than losing it.
        let kept = prepared.evaluate_filter(&chunk, &mut scratch).expect("the filter runs again");
        assert!(kept.is_empty());
        assert_eq!(scratch.order(root), Some(&[1, 0][..]));
    }

    /// Whatever order it settles on, the rows are the rows.
    ///
    /// Run for longer than the window is wide, because an order that changes halfway through a scan
    /// is the shape where a walk that got the subtree bookkeeping wrong would start reading the
    /// wrong steps, and the first chunk would not show it.
    #[test]
    fn reordering_never_changes_which_rows_survive() {
        let (schema, chunk) = input();
        let predicate = "((#0.0::INTEGER >= 1::INTEGER)::BOOLEAN AND \
                         (\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER < 4::INTEGER)::BOOLEAN AND \
                         (#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN)::BOOLEAN";
        let (plan, list) = projection(&format!("{predicate} AS p"));
        let prepared = Prepared::new(&plan, &list, &schema).expect("the predicate resolves");
        let mut scratch = prepared.scratch();
        let flags = evaluate(&plan, list[0], &schema, &chunk).expect("the tree walk runs");
        let expected = Selection::from_predicate(chunk.len(), |row| is_true(&flags.value_at(row)));
        for round in 0..40 {
            let kept = prepared.evaluate_filter(&chunk, &mut scratch).expect("the filter runs");
            assert_eq!(kept, expected, "round {round}");
        }
    }

    /// A nested connective is threaded rather than evaluated into flags.
    ///
    /// The inner `AND` keeps nothing, so its second conjunct is never reached and the division by
    /// zero in it never happens. Evaluating the branch as an expression and narrowing the flags
    /// afterwards, which is what an operand that is not a connective still does, would have run it.
    #[test]
    fn a_nested_connective_stops_where_the_outer_one_would() {
        let (schema, chunk) = input();
        let predicate = "((#0.0::INTEGER > 9::INTEGER)::BOOLEAN OR ((#0.0::INTEGER > 9::INTEGER)\
                         ::BOOLEAN AND (\"//\"(#0.0::INTEGER, 0::INTEGER)::INTEGER > 0::INTEGER)\
                         ::BOOLEAN)::BOOLEAN)::BOOLEAN";
        let (plan, list) = projection(&format!("{predicate} AS p"));
        let prepared = Prepared::new(&plan, &list, &schema).expect("the predicate resolves");
        let mut scratch = prepared.scratch();
        let kept =
            prepared.evaluate_filter(&chunk, &mut scratch).expect("the division never happens");
        assert!(kept.is_empty());
        evaluate(&plan, list[0], &schema, &chunk).expect_err("the tree walk divides by zero");
    }

    /// A branch that is not a comparison, which is the one that goes through the flag kernel.
    #[test]
    fn an_or_branch_that_is_not_a_comparison_is_threaded_too() {
        filters(
            "((#0.0::INTEGER > 2::INTEGER)::BOOLEAN OR \
             \"~~\"(#0.1::VARCHAR, 'a%'::VARCHAR)::BOOLEAN)::BOOLEAN",
        );
        filters(
            "(\"~~\"(#0.1::VARCHAR, 'c%'::VARCHAR)::BOOLEAN OR (#0.0::INTEGER = 1::INTEGER)\
             ::BOOLEAN)::BOOLEAN",
        );
    }

    /// A connective inside a connective, which recurses rather than falling back to flags.
    ///
    /// Both nestings, because the two carry opposite things: an `AND` under an `OR` starts from the
    /// rows no branch has accepted, and an `OR` under an `AND` starts from the rows every conjunct
    /// has kept, and getting either one backwards is a wrong set of rows.
    #[test]
    fn a_connective_inside_a_connective_threads_both_ways() {
        filters(
            "(((#0.0::INTEGER >= 2::INTEGER)::BOOLEAN AND (#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN)\
             ::BOOLEAN OR ((#0.0::INTEGER < 2::INTEGER)::BOOLEAN AND (#0.1::VARCHAR <> 'c'\
             ::VARCHAR)::BOOLEAN)::BOOLEAN)::BOOLEAN",
        );
        filters(
            "(((#0.1::VARCHAR = 'c'::VARCHAR)::BOOLEAN OR (#0.0::INTEGER = 3::INTEGER)::BOOLEAN)\
             ::BOOLEAN AND ((#0.0::INTEGER <> 1::INTEGER)::BOOLEAN OR (#0.1::VARCHAR = 'a'\
             ::VARCHAR)::BOOLEAN)::BOOLEAN)::BOOLEAN",
        );
        // Three deep, since two levels is where an off by one in the subtree bookkeeping can still
        // be hidden by the ranges lining up.
        filters(
            "((#0.0::INTEGER > 9::INTEGER)::BOOLEAN OR ((#0.0::INTEGER >= 1::INTEGER)::BOOLEAN \
             AND ((#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN OR (#0.0::INTEGER = 1::INTEGER)\
             ::BOOLEAN)::BOOLEAN)::BOOLEAN)::BOOLEAN",
        );
    }

    /// A predicate where one side is null and the other is true, in both orders. `OR` is true there
    /// and a complement taken over the rows a branch rejected rather than the rows it accepted
    /// would drop the row, which is the one way this can be wrong and is not a wrong vector but a
    /// missing row.
    #[test]
    fn a_null_branch_beside_a_true_one_keeps_the_row() {
        filters(
            "((#0.0::INTEGER > 2::INTEGER)::BOOLEAN OR (#0.1::VARCHAR = 'c'::VARCHAR)::BOOLEAN \
             OR (#0.0::INTEGER IS NOT DISTINCT FROM NULL::INTEGER)::BOOLEAN)::BOOLEAN",
        );
        filters(
            "((#0.1::VARCHAR > 'b'::VARCHAR)::BOOLEAN OR (#0.0::INTEGER = 1::INTEGER)::BOOLEAN)\
             ::BOOLEAN",
        );
    }

    /// A filter over a chunk that has already been narrowed, which is what a second filter in a
    /// pipeline sees and is the form pair the threaded kernels have to handle rather than fall
    /// through on.
    #[test]
    fn a_filter_over_a_selected_chunk_keeps_the_same_rows() {
        let (schema, chunk) = input();
        let predicate = "((#0.0::INTEGER >= 1::INTEGER)::BOOLEAN AND \
                         (#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN)::BOOLEAN";
        let (plan, list) = projection(&format!("{predicate} AS p"));
        let prepared = Prepared::new(&plan, &list, &schema).expect("the predicate resolves");
        let mut scratch = prepared.scratch();
        let narrowed = narrow(&chunk, &[0, 3]).expect("two of the four rows");
        let threaded = prepared.evaluate_filter(&narrowed, &mut scratch).expect("the filter runs");
        let flags = evaluate(&plan, list[0], &schema, &narrowed).expect("the tree walk runs");
        let expected =
            Selection::from_predicate(narrowed.len(), |row| is_true(&flags.value_at(row)));
        assert_eq!(threaded, expected);
    }

    /// Preparing is per pipeline and evaluating is per chunk, so the scratch has to survive being
    /// used again and give the same answer the second time.
    #[test]
    fn a_scratch_used_twice_gives_the_same_answer_twice() {
        let (schema, chunk) = input();
        let (plan, list) = projection("\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER AS a");
        let prepared = Prepared::new(&plan, &list, &schema).expect("the expressions resolve");
        let mut scratch = prepared.scratch();
        let mut once = Vec::new();
        prepared.evaluate(&chunk, &mut scratch, &mut once).expect("the first chunk runs");
        let mut twice = Vec::new();
        prepared.evaluate(&chunk, &mut scratch, &mut twice).expect("the second chunk runs");
        assert_eq!(once, twice);
    }

    #[test]
    fn taking_the_chunk_answers_what_borrowing_it_does() {
        let (schema, chunk) = input();
        let (plan, list) = projection(
            "#0.0::INTEGER AS a, \"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER AS b, #0.0::INTEGER AS c",
        );
        let prepared = Prepared::new(&plan, &list, &schema).expect("the expressions resolve");
        let mut scratch = prepared.scratch();
        let mut borrowed = Vec::new();
        prepared.evaluate(&chunk, &mut scratch, &mut borrowed).expect("the borrowed chunk runs");
        let mut taken = Vec::new();
        prepared.evaluate_taking(chunk, &mut scratch, &mut taken).expect("the taken chunk runs");
        assert_eq!(borrowed, taken);
    }

    #[test]
    fn a_shared_computed_root_is_compiled_once() {
        let (schema, chunk) = input();
        let (plan, list) = projection("\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER AS a");
        let prepared = Prepared::shared(&plan, &[list[0], list[0]], &schema)
            .expect("the shared expression resolves");
        assert_eq!(prepared.steps.len(), 3);
        let mut scratch = prepared.scratch();
        let mut answers = Vec::new();
        prepared.evaluate(&chunk, &mut scratch, &mut answers).expect("both roots are returned");
        assert_eq!(answers[0], answers[1]);
    }

    /// A chunk shorter than the last one, because a scan's final chunk is that and a constant
    /// materialized to the wrong length would be an out of range read rather than a wrong answer.
    #[test]
    fn a_shorter_chunk_after_a_longer_one_is_evaluated_at_its_own_length() {
        let (schema, chunk) = input();
        let (plan, list) = projection("7::INTEGER AS a");
        let prepared = Prepared::new(&plan, &list, &schema).expect("the expressions resolve");
        let mut scratch = prepared.scratch();
        let mut full = Vec::new();
        prepared.evaluate(&chunk, &mut scratch, &mut full).expect("the full chunk runs");
        assert_eq!(full[0].len(), 4);
        let short = chunk
            .clone()
            .select(&{
                let mut selection = Selection::with_capacity(2);
                selection.push(0);
                selection.push(2);
                selection
            })
            .expect("two of the four rows");
        let mut cut = Vec::new();
        prepared.evaluate(&short, &mut scratch, &mut cut).expect("the short chunk runs");
        assert_eq!(cut[0].len(), 2);
    }

    /// An aggregate is not an expression and saying so when the pipeline is built is better than
    /// saying it on the first chunk.
    #[test]
    fn an_aggregate_is_refused_when_it_is_prepared() {
        let (schema, _) = input();
        let text = "Aggregate #1 groups=[] aggregates=[sum(#0.0::INTEGER)::HUGEINT]\n  \
                    Get memory.main.t AS t #0 [x::INTEGER, s::VARCHAR]";
        let plan = Plan::parse(text).expect("a well formed plan");
        let Node::Aggregate { aggregates, .. } = *plan.node(plan.root()) else {
            panic!("the root of that text is an aggregate");
        };
        let list = plan.expr_list(aggregates).to_vec();
        let error = Prepared::new(&plan, &list, &schema).expect_err("sum is not a scalar");
        assert!(error.message().contains("sum"), "{error}");
    }

    /// How many of an expression's function steps worked something out when it was prepared, and
    /// whether the answer it gives is still the tree walk's answer.
    ///
    /// The count is the point of the assertion, because an answer that moved would be a bug. The
    /// agreement is what says the answer did not move.
    fn prepares(expr: &str, lifted: usize) {
        let (schema, _) = input();
        let projected = format!("{expr} AS a");
        let (plan, list) = projection(&projected);
        let prepared = Prepared::new(&plan, &list, &schema).expect("the expression resolves");
        assert_eq!(prepared.hoisted(), lifted, "`{expr}`");
        agrees(&projected);
    }

    /// A pattern the user wrote is compiled where the plan is, which is once.
    #[test]
    fn a_literal_pattern_is_compiled_when_the_pipeline_is_built() {
        prepares("\"~~\"(#0.1::VARCHAR, 'a%'::VARCHAR)::BOOLEAN", 1);
        prepares("\"~~*\"(#0.1::VARCHAR, '%A%'::VARCHAR)::BOOLEAN", 1);
    }

    /// A regular expression, which is the one where the compiling is worth real time.
    ///
    /// ClickBench query 29 runs one pattern over a hundred million rows, which is a hundred thousand
    /// chunks, and before this each of those hundred thousand compiled the pattern again.
    #[test]
    fn a_regular_expression_is_compiled_when_the_pipeline_is_built() {
        prepares("\"regexp_matches\"(#0.1::VARCHAR, '^a'::VARCHAR)::BOOLEAN", 1);
        prepares("\"regexp_replace\"(#0.1::VARCHAR, 'a'::VARCHAR, 'b'::VARCHAR)::VARCHAR", 1);
    }

    /// A pattern that is not a literal, which is legal SQL and is decided per chunk as it was.
    #[test]
    fn a_pattern_that_is_not_a_literal_is_left_to_the_chunk() {
        prepares("\"~~\"(#0.1::VARCHAR, #0.1::VARCHAR)::BOOLEAN", 0);
    }

    /// A function with nothing to work out, which is almost all of them.
    #[test]
    fn a_function_with_no_prepare_step_prepares_nothing() {
        prepares("\"upper\"(#0.1::VARCHAR)::VARCHAR", 0);
    }

    /// How many of an expression's steps are a folded `IN`, and whether the answer still agrees.
    fn folds(expr: &str, sets: usize) {
        let (schema, _) = input();
        let projected = format!("{expr} AS a");
        let (plan, list) = projection(&projected);
        let prepared = Prepared::new(&plan, &list, &schema).expect("the expression resolves");
        assert_eq!(prepared.sets(), sets, "`{expr}`");
        agrees(&projected);
    }

    /// What the binder writes for `x IN (1, 3)`, folded back into one lookup.
    ///
    /// The test goes through the plan's text, where the three mentions of the column are three
    /// expressions rather than one, which is the case `same` exists for. A plan the binder built has
    /// one mention and takes the first line of it.
    #[test]
    fn an_in_list_becomes_one_lookup() {
        folds(
            "((#0.0::INTEGER = 1::INTEGER)::BOOLEAN OR (#0.0::INTEGER = 3::INTEGER)::BOOLEAN)\
             ::BOOLEAN",
            1,
        );
        folds(
            "((#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN OR (#0.1::VARCHAR = 'z'::VARCHAR)::BOOLEAN)\
             ::BOOLEAN",
            1,
        );
    }

    /// `NOT IN`, which the binder writes as an `AND` of inequalities and which reads the same
    /// lookup the other way round.
    #[test]
    fn a_not_in_list_becomes_the_same_lookup() {
        folds(
            "((#0.0::INTEGER <> 1::INTEGER)::BOOLEAN AND (#0.0::INTEGER <> 3::INTEGER)::BOOLEAN)\
             ::BOOLEAN",
            1,
        );
    }

    /// A list with a null in it, which is the rule that makes an `IN` not a set lookup.
    ///
    /// A row that is not in the list is null rather than false, because it might have equalled the
    /// value the null stands for. `agrees` is what says the fold kept that, since the `OR` of
    /// comparisons it is checked against gets it from three valued logic for free.
    #[test]
    fn a_list_with_a_null_in_it_folds_and_keeps_the_null_rule() {
        folds(
            "((#0.0::INTEGER = 1::INTEGER)::BOOLEAN OR (#0.0::INTEGER = NULL::INTEGER)::BOOLEAN \
             OR (#0.0::INTEGER = 3::INTEGER)::BOOLEAN)::BOOLEAN",
            1,
        );
        folds(
            "((#0.0::INTEGER <> 1::INTEGER)::BOOLEAN AND (#0.0::INTEGER <> NULL::INTEGER)\
             ::BOOLEAN AND (#0.0::INTEGER <> 3::INTEGER)::BOOLEAN)::BOOLEAN",
            1,
        );
    }

    /// The connectives that are not an `IN`, each for its own reason.
    #[test]
    fn a_connective_that_is_not_an_in_list_is_left_alone() {
        // Two different columns.
        folds(
            "((#0.0::INTEGER = 1::INTEGER)::BOOLEAN OR (#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN)\
             ::BOOLEAN",
            0,
        );
        // One equality and one of something else.
        folds(
            "((#0.0::INTEGER = 1::INTEGER)::BOOLEAN OR (#0.0::INTEGER > 3::INTEGER)::BOOLEAN)\
             ::BOOLEAN",
            0,
        );
        // The right hand side is a column rather than a literal.
        folds(
            "((#0.0::INTEGER = 1::INTEGER)::BOOLEAN OR (#0.0::INTEGER = #0.0::INTEGER)::BOOLEAN)\
             ::BOOLEAN",
            0,
        );
        // An `AND` of equalities is not a `NOT IN`, it is a predicate that is false unless the two
        // literals are the same. Folding it as one would answer true where it answers false.
        folds(
            "((#0.0::INTEGER = 1::INTEGER)::BOOLEAN AND (#0.0::INTEGER = 3::INTEGER)::BOOLEAN)\
             ::BOOLEAN",
            0,
        );
    }

    /// The same thing in a filter, which is the shape it is written in.
    #[test]
    fn an_in_list_filters_the_same_rows() {
        filters(
            "((#0.0::INTEGER = 1::INTEGER)::BOOLEAN OR (#0.0::INTEGER = 3::INTEGER)::BOOLEAN)\
             ::BOOLEAN",
        );
        filters(
            "((#0.0::INTEGER <> 1::INTEGER)::BOOLEAN AND (#0.0::INTEGER <> 3::INTEGER)::BOOLEAN)\
             ::BOOLEAN",
        );
        // Inside a larger predicate, where the fold is one operand of the connective above it.
        filters(
            "(((#0.0::INTEGER = 1::INTEGER)::BOOLEAN OR (#0.0::INTEGER = 3::INTEGER)::BOOLEAN)\
             ::BOOLEAN AND (#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN)::BOOLEAN",
        );
    }

    /// The literal side of a comparison is turned into a column when the pipeline is built.
    #[test]
    fn a_comparison_against_a_literal_builds_it_once() {
        let (schema, _) = input();
        for (expr, built) in [
            ("(#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN AS p", 1),
            ("(#0.0::INTEGER > 1::INTEGER)::BOOLEAN AS p", 1),
            // The literal on the left, which is the same comparison written the other way round.
            ("(1::INTEGER < #0.0::INTEGER)::BOOLEAN AS p", 1),
            // Two columns, which has no literal side to build.
            ("(#0.0::INTEGER = #0.0::INTEGER)::BOOLEAN AS p", 0),
            // Two literals, which the kernel answers once for the whole vector without reading a
            // column, so building one would be work that nothing reads.
            ("(1::INTEGER = 2::INTEGER)::BOOLEAN AS p", 0),
        ] {
            let (plan, list) = projection(expr);
            let prepared = Prepared::new(&plan, &list, &schema).expect("the expression resolves");
            assert_eq!(prepared.literals_built(), built, "`{expr}`");
            agrees(expr);
        }
    }

    /// A pattern that does not compile still fails where the query said it does.
    ///
    /// Preparing is not allowed to move an error earlier. Compiling at build time and reporting
    /// there would raise before a row had been read, and under a `CASE` arm it would raise on a
    /// query whose rows never reach the call at all.
    #[test]
    fn a_pattern_that_does_not_compile_fails_on_the_chunk_and_not_before() {
        let (schema, chunk) = input();
        let (plan, list) =
            projection("\"regexp_matches\"(#0.1::VARCHAR, 'a('::VARCHAR)::BOOLEAN AS a");
        let prepared = Prepared::new(&plan, &list, &schema).expect("preparing does not compile it");
        assert_eq!(prepared.hoisted(), 0);
        let mut scratch = prepared.scratch();
        let mut out = Vec::new();
        prepared.evaluate(&chunk, &mut scratch, &mut out).expect_err("the chunk raises");
    }
}
