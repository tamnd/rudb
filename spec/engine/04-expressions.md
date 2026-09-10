# Layer two: expressions

This is sub-milestone 2c. It covers the evaluation of a bound expression tree over a chunk, which is what a filter is, what a projection is, what a join condition is and what the input to every aggregate is. It sits directly on layer one and it is the last layer before anything touches a file.

Section 8.2 of the parent spec describes three tiers: a tree interpreter, a fused loop, and a compiled loop. Tier 0 exists in `crates/rudb-exec/src/expr.rs` and its module doc says correctly that tiers 1 and 2 exist to remove the intermediate vectors. What this document says is that tier 0 has four defects that are worth more than tier 1 is, that tier 1 is worth doing after they are fixed, and that tier 2 does not belong in this layer at all.

## 4.1 What exists today

One recursive function, `evaluate(plan, expr, schema, chunk) -> Result<Vector>`, 161 lines including `evaluate_all` and a `narrow` helper. Eight expression kinds: column, constant, cast, compare, conjunction, function, aggregate (which errors here) and case.

The structure is right. Types come from the bound plan and are never inferred, which the module doc argues for and which is correct: an evaluator that inferred a type would be a second type system that has to agree with the first one, and the interesting bugs in a database are where two such things disagree. Nothing in this document changes that.

What the structure gets wrong is everything about how much work it does per row.

## 4.2 The four defects

**A column reference copies the column.** `Expr::Column` resolves the position and then evaluates to `chunk.column(position)?.clone()`. For a flat vector that is a full copy of the payload, so an expression that mentions the same column three times copies it three times, and an expression that mentions one column once still copies it once. On `hits`, where a scan hands up a chunk of a thousand rows and a filter mentions two columns, that is two thousand values copied before any work is done. The fix is that `evaluate` returns something that can borrow, which given the ownership decision in document 03 section 3.8 means the vector's payload is refcounted rather than cloned, and a column reference is an increment.

**Conjunction evaluates every branch on every row.** `Expr::Conjunction` calls `evaluate_all` on all children and then combines the results with a bitmap operation. `WHERE a > 5 AND b LIKE '%x%' AND c = 3` therefore runs the `LIKE` on every row, including the rows the first conjunct already rejected. On TPC-H Q6, which is four conjuncts over `lineitem` with a combined selectivity around two percent, the second, third and fourth predicates are each doing roughly fifty times the work they need to. This is the largest single number in this document.

**`CASE` is row-at-a-time and allocating.** It builds a `Vec<Value>` of the chunk length, keeps a `Vec<usize>` of pending rows, and for each arm calls `narrow` to build a whole new chunk of the surviving rows, evaluates the condition, then loops over the survivors calling `flags.value_at(at)` and `results.value_at(slot)` and writing owned `Value`s into the answers array, then calls `Vector::from_values` at the end. Every one of those steps is the thing document 03 section 3.2 identified as the reason the engine is slow, and `narrow` copies the entire chunk once per arm. `CASE` is not rare: it is in ClickBench Q29 and in several TPC-H queries, and it is how most real analytics SQL expresses a bucketing.

**There is no state between chunks.** Every call to `evaluate` re-walks the tree, re-resolves every column position against the schema by a linear search in `Schema::position_of`, and re-materializes every constant into a fresh constant vector of the chunk length. All three are per-chunk costs that are properly per-pipeline costs. A hundred thousand chunks over `hits` means a hundred thousand schema lookups per column reference.

None of these is subtle and none of them requires research to find. They are what a first correct implementation looks like, they were the right thing to write first, and this layer is where they get paid off.

## 4.3 Tier 0 done properly: a prepared expression

The interpreter stops being a function over a tree and becomes a prepared object over a tree, built once per pipeline and executed per chunk. This is DuckDB's `ExpressionExecutor` and `ExpressionState` and it is the standard shape for a reason.

```rust
pub struct Prepared {
    nodes: Vec<Node>,          // flattened, children before parents
    scratch: Vec<Vector>,      // one intermediate per node, reused across chunks
    inputs: Vec<usize>,        // resolved column positions, resolved once
    constants: Vec<Vector>,    // materialized once, at chunk width
}
```

Three things fall out of that shape immediately and they are the whole of the per-chunk win.

The tree is flattened into a post-order array at prepare time, so execution is a loop over an array rather than a recursion, and there is no per-node function call and no stack depth proportional to expression depth. Deep expressions are common in generated SQL and in `CASE` chains.

Column positions are resolved at prepare time, so `Schema::position_of` is called once per reference per pipeline instead of once per reference per chunk.

The intermediate vectors are allocated once and reused. Today every node allocates its output, so an expression with n nodes does n allocations per chunk, and at a hundred thousand chunks that is where the allocator time goes. A reused scratch buffer per node makes that zero after the first chunk. This is the single change that makes the tier 0 to tier 1 gap much smaller than the parent spec assumed, which is why tier 1 is reconsidered in section 4.6.

The prepared object is per pipeline instance and not shared between threads, because the scratch buffers are mutable. The immutable part, the flattened nodes and the resolved positions, is shared. That split is the same one the scheduler in document 10 needs for every operator, and doing it here first is deliberate.

## 4.4 Selection threading, which is where the factor is

The prepared executor carries a selection through the tree rather than evaluating every node over the full chunk.

For a filter, the mechanism is: start with all rows selected, evaluate the first conjunct over the selected rows, produce a new selection of the rows that passed, and evaluate the second conjunct over only those. At two percent combined selectivity over four conjuncts the fourth predicate sees two percent of the rows. The output of the whole filter is a selection, which is exactly what `Chunk::select` in layer one consumes.

This requires kernels that take a selection, which is the unified presentation from document 03 section 3.3, and it is the reason that presentation exists rather than being an academic nicety. It also requires that the specialization set includes flat-data-with-selection, because after the first conjunct that is the shape every later conjunct sees.

Two subtleties decide whether this is correct.

Threading is valid for `AND` in a filter because the result is only ever consumed as a mask. It is not valid for `AND` in a projection, because `SELECT a AND b` has to produce a three-valued answer for every row including the ones where `a` is false, and short-circuiting loses the distinction between false and null. The prepared executor therefore has two modes, and the plan says which one it is: filter mode returns a selection, projection mode returns a full vector. Getting this wrong produces a wrong answer only on nullable boolean columns, which is a narrow enough case that it survives casual testing, so it gets an explicit corpus-shaped test rather than being left to the corpus to find.

Threading changes when a side effect happens. SQL does not promise evaluation order and DuckDB does not promise it either, so `WHERE x <> 0 AND 10 / x > 1` may still divide by zero in both engines, and matching DuckDB's behaviour here means matching its actual behaviour rather than its documentation. That is a compatibility question and it goes to `rudb-compat` as a set of cases rather than being decided in this document.

`OR` threads the other way. The rows that pass the first branch are removed from consideration for the second, and only the undecided rows are evaluated. That is the same mechanism with the selection complemented, and it matters on ClickBench because several of the string queries are disjunctions of `LIKE`.

## 4.5 Adaptive conjunct ordering

Once conjuncts are evaluated in sequence, the order they are evaluated in decides the cost, and the optimizer's estimate of selectivity is a guess made before the data was read.

Vectorwise's micro-adaptivity work and DuckDB's adaptive filter both do the same thing: measure per conjunct, per chunk, how many rows survived and how long it took, and permute the order to put the cheapest most-selective predicate first. DuckDB's implementation swaps adjacent conjuncts on a running average and randomly explores a different permutation occasionally so that it can escape a local ordering that only looked good on the first few chunks.

rudb does this and it goes in at this layer rather than at layer ten, even though document 00 puts adaptivity at layer ten, because the mechanism is three lines once selection threading exists and because the cost of a bad conjunct order is a multiple rather than a percentage. The exploration policy is the part that goes to layer ten, where it can be reasoned about alongside the other adaptive decisions rather than invented separately here.

The measurement per conjunct is rows in, rows out and nanoseconds, kept as a running average over the last few chunks rather than over all of them, because selectivity in a sorted or clustered column changes as the scan moves through the file and an average over the whole scan is an average over two different distributions. That last point is not academic on `hits`, where the data is clustered by time.

## 4.6 What tier 1 is worth after all of that

The parent spec's tier 1 fuses a chain of expression nodes into one loop so that the intermediates never exist. The claim was that intermediates are the cost.

After section 4.3 the intermediates are allocated once and reused, so what remains is not allocation but memory traffic and pass count. An expression like `(a + b) * c > 10` at tier 0 does four passes over a thousand rows each writing an intermediate, where a fused loop does one pass and keeps everything in registers. At 1024 rows of 8 bytes the intermediates are 8 KB each and they stay in L1, so the traffic argument is weaker than it sounds and what is actually saved is three loop setups and three passes of load-store against one.

The honest expectation, from the Kersten et al. VLDB 2018 comparison of compiled and vectorized execution, is that this is worth tens of percent on expression-heavy queries and close to nothing on queries dominated by the scan or the hash table. That is worth having and it is not worth having before layers three through six, which are worth multiples.

So tier 1 is specified here and scheduled after layer six. The specification is that fusion applies to a maximal chain of unary and binary arithmetic and comparison nodes over fixed-width types with no nulls in the inputs, which is the case where the fused loop is a straight line the compiler vectorizes, and every other node breaks the chain. That restriction is what makes it implementable without a code generator, as a set of macro-generated fused kernels for the common shapes rather than as a general fusion engine. A general fusion engine is tier 2 wearing a disguise.

## 4.7 What tier 2 is not

Compiling expressions to machine code is `rudb-jit`, it is rank 4 in the layer graph, and it is M8 in the milestone plan. It is not in this directory's ten layers and this document does not schedule it.

The reason for stating that here rather than silently omitting it is that the temptation at this layer is to skip tier 1 and go straight to compilation, and the argument against is Kersten again: a well implemented vectorized interpreter is within a factor of two of compiled code on most analytic queries, the gap is largest exactly on the expression-heavy queries that are least common, and compilation adds a latency floor per query that the parent spec's second axis, the per-query floor, is explicitly trying to keep low. An engine that compiles has to have a decision procedure for when not to compile, and that decision procedure needs a fast interpreter to fall back to. Building the interpreter properly is therefore a prerequisite for compilation and not an alternative to it.

## 4.8 Functions

`rudb_kernels::call(name, args, ty)` resolves a function by string name on every call. At prepare time that becomes a resolved function pointer, which removes a string comparison per chunk and, more importantly, makes it possible for a function to have prepare-time state.

Prepare-time state is what makes the string functions fast and it is most of what ClickBench measures. `LIKE '%mail%'` compiles its pattern once into a matcher rather than reparsing the pattern per row, and a pattern with no wildcards in the middle becomes a substring search. A regex compiles once. A cast between two fixed types resolves to a concrete conversion once. A comparison against a constant string precomputes the constant's 4 byte prefix so that the prefix trick from document 03 section 3.5 works against it without recomputing.

The function signature therefore grows a prepare step, and the prepared state lives in the `Prepared` object next to the scratch buffers. This is a change to `rudb-kernels`, it is small, and it has to happen at this layer because doing it later means revisiting every function.

## 4.9 `CASE` rewritten

`CASE` becomes selection-threaded like everything else, which removes both the `Vec<Value>` and the chunk copying.

Maintain a selection of undecided rows, starting as all rows. For each arm, evaluate the condition over the undecided rows, split them into taken and still-undecided, evaluate the arm's result expression over only the taken rows, and scatter into the output vector at those positions. When no rows remain undecided, stop. The `ELSE` handles whatever is left, and positions never covered are null.

That is the same algorithm the current code describes, with the row-at-a-time middle replaced. The scatter is a real operation that has to exist as a kernel, writing a vector's values into another vector at given positions, and it is used again by the aggregate layer and by the join's payload assembly, so it is written once here.

The early exit when nothing is pending stays, because a `CASE` chain where the first arm catches everything is common and it should cost one arm.

## 4.10 The test gate

The oracle strategy from document 03 applies again and it is stronger here. The current `evaluate` becomes the reference implementation, kept in the test module, and every prepared execution is checked against it over generated expression trees. Generating random well-typed expression trees over a random schema and asserting that the two agree position by position, including nulls, is the test that catches selection threading bugs, and it is worth building the generator properly because layers five through nine will reuse it.

Nullable booleans get their own suite, because that is where filter mode and projection mode differ and where a wrong answer would be quiet. Every combination of true, false and null across `AND`, `OR` and `NOT`, in both modes, checked against the SQL truth table written out by hand rather than against another implementation.

Adaptive reordering must not change any answer, ever, so the reordering test runs the same query with the reordering forced to every permutation and asserts the results are identical. That is a cheap test and it is the one that catches a conjunct with a side effect being moved.

The corpus is the outer check and again it must not fall. Expression evaluation is the most corpus-covered part of the engine, several thousand records touch it directly, and that coverage is what makes this rewrite safe.

## 4.11 The benchmark gate

Microbenchmarks in the `kernels` suite, extended with an `expressions` group.

A four-conjunct filter at combined selectivities of 50, 10, 1 and 0.1 percent, measured with threading on and off, which directly produces the number claimed in section 4.2. The prediction is that at one percent the threaded version is between three and ten times faster, and if it is not, the specialization for flat-with-selection is missing.

An arithmetic chain of depth 2, 4 and 8 over `BIGINT` and `DOUBLE`, which is the measurement that decides how much tier 1 is worth and therefore whether the schedule in section 4.6 was right. This number is a deliverable of 2c even though the work it justifies is later.

`CASE` with 2, 5 and 20 arms at uniform and skewed arm selection.

`LIKE` with a leading wildcard, a trailing wildcard, both, and none, against a constant, on the `hits` URL column, because that is what ClickBench Q20 to Q27 are and because the prepared-pattern change in section 4.8 is expected to be worth a lot there.

The whole-query gate is TPC-H Q6 and Q1 again, now expected to show the conjunct ordering effect, plus ClickBench Q20 to Q27 which are the string filter queries, plus ClickBench Q29 which has a `CASE`. On `server3`, against the 2b number, and against DuckDB.

The target: CPU seconds on TPC-H Q6 fall by at least three times against 2b, and ClickBench Q20 to Q27 fall by at least two times. Those are conservative, they assume the scan underneath is still naive and reading everything, and layer three is what fixes that.

## 4.12 Exit criterion for 2c

**Expression evaluation is prepared once per pipeline and executed per chunk with no allocation on the per-chunk path, filters thread a selection through their conjuncts with adaptive ordering, `CASE` and the function layer have no `Value` on any loop path, the generated-expression oracle test passes, the corpus pass rate has not fallen, and the `expressions` benchmark group is committed with the tier 1 decision number in it.**

Deferred by name so that they are not mistaken for forgotten: tier 1 fusion is specified in section 4.6 and scheduled after layer six, tier 2 compilation is out of scope and belongs to M8, and the exploration policy for adaptive ordering belongs to document 12.
