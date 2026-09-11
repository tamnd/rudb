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

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_kernels::{Comparison, Connective, cast, combine, compare, is_true};
use rudb_plan::{CompareOp, ConjunctionOp, Expr, ExprRef, Plan};
use rudb_vector::{Chunk, Selection, Vector};

use crate::schema::Schema;

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
        /// The resolved function name, held here so the plan is not consulted per chunk.
        name: String,
        /// Where the argument list starts.
        start: usize,
        /// How many arguments it has.
        len: usize,
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

/// The per chunk working space of one [`Prepared`].
///
/// One per pipeline instance and never shared, which is the mutable half of the split the module
/// documentation describes. It is handed back in rather than made inside [`Prepared::evaluate`] so
/// that the array of slots survives from one chunk to the next instead of being allocated a hundred
/// thousand times over a scan.
#[derive(Debug)]
pub struct Scratch {
    /// What each step produced, or `None` for a step that produces nothing and for one that has not
    /// run yet.
    slots: Vec<Option<Vector>>,
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
        let mut prepared = Self {
            steps: Vec::new(),
            types: Vec::new(),
            operands: Vec::new(),
            last_use: Vec::new(),
            roots: Vec::new(),
        };
        for &expr in exprs {
            let root = prepared.push(plan, expr, schema)?;
            prepared.roots.push(root);
        }
        prepared.last_use = prepared.last_uses();
        Ok(prepared)
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
            // A case's branches are arrays of their own and read nothing out of this one.
            Step::Column(_) | Step::Constant(_) | Step::Case { .. } => {}
            Step::Cast { input, .. } => visit(*input),
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
        Scratch { slots: (0..self.steps.len()).map(|_| None).collect() }
    }

    /// How many expressions this was built from.
    #[must_use]
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    /// Whether it was built from no expressions at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
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
        for &root in &self.roots {
            // The one place a column is copied, and it is copied because the caller is taking
            // ownership of a vector that has to outlive the chunk it came from. `SELECT a` is that
            // shape and a projection of a bare column is the only expression where it happens.
            match self.steps[root] {
                Step::Column(position) => out.push(chunk.column(position)?.clone()),
                _ => out.push(scratch.slots[root].take().ok_or_else(|| missing(root))?),
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

    /// Runs every step in order, filling the slots.
    fn run(&self, chunk: &Chunk, scratch: &mut Scratch) -> Result<()> {
        scratch.slots.clear();
        scratch.slots.resize_with(self.steps.len(), || None);
        for index in 0..self.steps.len() {
            let produced = self.step(index, chunk, &scratch.slots)?;
            scratch.slots[index] = produced;
            let slots = &mut scratch.slots;
            self.for_each_operand(index, |operand| {
                if self.last_use[operand] == index {
                    slots[operand] = None;
                }
            });
        }
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
            Step::Cast { input, try_cast } => {
                Some(cast(self.operand(*input, chunk, slots)?, ty, *try_cast)?)
            }
            Step::Compare { op, left, right } => Some(compare(
                *op,
                self.operand(*left, chunk, slots)?,
                self.operand(*right, chunk, slots)?,
            )?),
            Step::Conjunction { op, start, len } => {
                Some(
                    self.with_operands(*start, *len, chunk, slots, |children| {
                        combine(*op, children)
                    })?,
                )
            }
            Step::Function { name, start, len } => {
                Some(self.with_operands(*start, *len, chunk, slots, |args| {
                    rudb_kernels::call(name, args, ty)
                })?)
            }
            Step::Case { arms, otherwise } => {
                Some(self.case(chunk, arms, otherwise.as_ref(), ty)?)
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
    /// divides by zero on the rows the arm excludes if the arm is evaluated for them. What is left
    /// of it after #57 is the same rule expressed as a selection rather than as a narrowed chunk,
    /// with the answers scattered back instead of assembled out of a `Vec<Value>`.
    fn case(
        &self,
        chunk: &Chunk,
        arms: &[PreparedArm],
        otherwise: Option<&Prepared>,
        ty: &LogicalType,
    ) -> Result<Vector> {
        let mut answers = vec![Value::Null; chunk.len()];
        let mut pending: Vec<usize> = (0..chunk.len()).collect();
        for arm in arms {
            if pending.is_empty() {
                break;
            }
            let narrowed = narrow(chunk, &pending)?;
            let mut scratch = arm.when.scratch();
            let flags = arm.when.evaluate_one(&narrowed, &mut scratch)?;
            let mut taken = Vec::new();
            let mut still = Vec::new();
            // row at a time: the scatter that replaces these three loops is #57, and this variant
            // goes with it.
            for (at, &row) in pending.iter().enumerate() {
                if is_true(&flags.value_at(at)) {
                    taken.push((at, row));
                } else {
                    still.push(row);
                }
            }
            if !taken.is_empty() {
                let positions: Vec<usize> = taken.iter().map(|&(at, _)| at).collect();
                let matched = narrow(&narrowed, &positions)?;
                let mut scratch = arm.then.scratch();
                let results = arm.then.evaluate_one(&matched, &mut scratch)?;
                // row at a time: the scatter this wants is #57, same as the loop above.
                for (slot, &(_, row)) in taken.iter().enumerate() {
                    answers[row] = results.value_at(slot);
                }
            }
            pending = still;
        }
        if let Some(otherwise) = otherwise {
            if !pending.is_empty() {
                let narrowed = narrow(chunk, &pending)?;
                let mut scratch = otherwise.scratch();
                let results = otherwise.evaluate_one(&narrowed, &mut scratch)?;
                // row at a time: the scatter this wants is #57, same as the two above.
                for (slot, &row) in pending.iter().enumerate() {
                    answers[row] = results.value_at(slot);
                }
            }
        }
        Vector::from_values(ty.clone(), &answers)
    }

    /// Flattens one expression, appending its steps and returning the index of its last one.
    fn push(&mut self, plan: &Plan, expr: ExprRef, schema: &Schema) -> Result<usize> {
        let ty = plan.expr_type(expr).clone();
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
            Expr::Compare { op, left, right } => Step::Compare {
                op: comparison(op),
                left: self.push(plan, left, schema)?,
                right: self.push(plan, right, schema)?,
            },
            Expr::Conjunction { op, children } => {
                let (start, len) = self.push_list(plan, plan.expr_list(children), schema)?;
                Step::Conjunction { op: connective(op), start, len }
            }
            Expr::Function { name, args } => {
                let (start, len) = self.push_list(plan, plan.expr_list(args), schema)?;
                Step::Function { name: plan.string(name).to_string(), start, len }
            }
            Expr::Aggregate { name, .. } => {
                return Err(Error::internal(format!(
                    "the {} aggregate was evaluated as an ordinary expression",
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
                Step::Case { arms: prepared, otherwise }
            }
        };
        self.steps.push(step);
        self.types.push(ty);
        Ok(self.steps.len() - 1)
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

/// The chunk cut down to the given rows.
///
/// The reason `CASE` is written with this rather than by evaluating every arm over the whole chunk
/// and picking afterwards. `CASE WHEN x <> 0 THEN 1 / x ELSE 0 END` divides by zero on the rows the
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
    use rudb_plan::{ExprRef, Node, Plan};
    use rudb_vector::{Chunk, Vector};

    use super::Prepared;
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

    #[test]
    fn a_case_agrees() {
        agrees(
            "CASE WHEN (#0.0::INTEGER > 1::INTEGER)::BOOLEAN THEN 10::INTEGER \
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
                let mut selection = rudb_vector::Selection::with_capacity(2);
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
}
