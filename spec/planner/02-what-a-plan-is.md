# What a plan is

Before anything can be optimized there has to be something to optimize. rudb has one already, and it is better than the thing most projects have at this point, so this document is an inventory before it is a design.

## What `rudb-plan` already has

Three and a half thousand lines across six files, and four properties that matter more than the line count.

**An arena rather than a tree of boxes.** `Plan` holds node, expression, field, name, row and sort-key pools, and a node refers to its input by a `NodeRef` index. A pass that rewrites the plan appends to the pools and returns a new root. Nothing is reference counted and nothing is cloned to be visited.

**Thirteen node kinds**, which is the whole logical language: `Get`, `Dummy`, `Values`, `TableFunction`, `Filter`, `Project`, `Aggregate`, `Sort`, `Limit`, `Distinct`, `Join`, `CrossProduct`, `SetOp`. Compare that to the nine a dataframe library needs. The extra four are the price of being a SQL database, and each one is there for a stated reason in its own doc comment. `Dummy` is separate from an empty result because `SELECT 1` produces one row and an empty result produces none, and conflating them is how a scalar subquery starts returning nothing instead of null. `CrossProduct` is separate from a `Join` with no conditions because join ordering enumerates connected subgraphs and a cross product has no edge.

**Eight join kinds**: `Inner`, `Left`, `Right`, `Full`, `Semi`, `Anti`, `Single`, `Positional`. `Semi` and `Anti` exist because unnesting produces them directly and a semi join expressed as a join plus a distinct is a semi join the executor cannot recognize. `Single` exists because it is what a correlated scalar subquery unnests to. Those two sentences mean the plan language was designed with document 05 already in mind, which is a year of rework that will not happen.

**A printer and a parser that round-trips.** `print.rs` is 544 lines and `parse.rs` is 1289. A plan can be dumped as text and read back as the same plan. This is the property the parent spec calls out as what modular cashes out to, and for this folder it has one specific consequence that decides how every pass is tested: a pass test is a text file in and a text file out, diffable, reviewable, and bisectable. No assertions about tree structure, ever.

**`Plan::validate`.** A plan invariant that can be asserted. Document 03 makes every pass assert it in debug builds, which is the mechanism that turns a miscompilation into an immediate failure rather than a wrong answer three passes later.

## The three representations, and the fourth

Polars keeps three and rudb needs four, because a SQL database has a stage a dataframe library does not.

**The AST**, `rudb-parse`. What was written, with no names resolved. Printed when someone asks what they typed.

**The bound plan**, the output of `rudb-bind`. Names resolved to `ColumnBinding`s, types computed, overloads picked. This is what `EXPLAIN` prints as the logical plan and it is the input to this folder.

**The optimized plan**, the output of `rudb-opt`. The same type as the bound plan, which is the important part: an optimizer pass is `Plan -> Plan` and not `Bound -> Optimized`, so passes compose, the printer is one printer, and a pass can be run twice.

**The physical plan**, which does not exist yet. Document 10 says what goes in it and when. The reason it is a separate representation rather than annotations on the logical plan is that one logical `Aggregate` becomes one of four physical shapes and one logical `Join` becomes one of three, and a representation where a node means two things at once is a representation where every consumer has to ask which.

Until the physical plan exists, `crates/rudb-exec/src/build.rs` lowers the optimized logical plan directly, one arm per node. That is tier 0 and it is honest about being tier 0.

## What is missing from the node set

Four things, each of which document 03 or later needs.

**A materialization node with more than one consumer.** The plan is a tree, and common subplan elimination produces a DAG. TPC-DS repeats the same filtered scan of `date_dim` in almost every query, and turning a repeated subtree into one node with two consumers is what makes that pattern tolerable. This is the one structural change to `rudb-plan` this folder requires and it should be made before the elimination pass rather than with it, because turning a tree into a DAG touches the printer, the parser, the validator and every existing pass's walk order.

**A window node.** Not needed to bind window functions, which is the binder's problem, but needed to place them, because a window and a sort share a sort and a pass that recognizes that has to have something to rewrite.

**`GROUPING SETS`, `CUBE` and `ROLLUP`.** TPC-DS has many and the shape is an `Aggregate` that produces several grouping sets from one pass over the input rather than a `SetOp` over several aggregates. Expressing it as a `SetOp` binds correctly and runs *n* times over the data, so the node earns itself back on the first query that uses it.

**Recursive CTE.** Out of scope for this folder, named so it is not discovered as an omission.

Everything else in the set is enough. The rule from the firepanda folder applies unchanged: write the list down and refuse to add to it without an argument.

## Expressions

`expr.rs` has the `Expr` enum with `CompareOp` and `ConjunctionOp` beside it. The expression tree is separate from the plan tree and lives in its own pool, which is what lets document 04's pushdown rebuild the node list while leaving every `ExprRef` valid.

Three analyses are needed and none of them exists. Every pass in this folder is written in terms of at least one, which is why they arrive here rather than with the first pass that wants one.

**Elementwise.** The value at row *i* depends only on row *i*. `a + b * 2` is, `sum(a)` is not, a window function is not. An elementwise expression can be evaluated on a morsel with no state carried between morsels, which is what makes a projection free in a streaming engine and what makes it safe to move a projection across a boundary that reorders rows. The analysis is a walk over the flattened expression and the answer is statically obvious for every kind in the set.

**Constant.** The expression references no column. `DATE '1998-12-01' - INTERVAL '90 days'` is one, and an engine without this analysis evaluates it once per row. Folding it is document 03's first pass, and this analysis is what tells the pass what is foldable. Note the subtlety that volatile functions are not constant even when they take no column: `random()` and `now()` have to be excluded by name, and the list of which functions are volatile is DuckDB's list and comes from the corpus.

**Table set.** Which table indices an expression reads, as a bitset over the `index` values that `Node::table_index` hands out. This is the analysis predicate pushdown, transitive predicates, join ordering and predicate transfer are all written in terms of, because a predicate can move into a subtree exactly when that subtree provides every index the predicate reads. It is the highest-traffic analysis in the folder and it should be cached on the plan rather than recomputed, keyed by `ExprRef`, and invalidated when the expression pool grows.

A fourth that is worth having and is cheaper than it sounds:

**Null rejecting.** Whether the expression is guaranteed to be false or null when a given input column is null. `x > 5` is null rejecting on `x`; `x IS NULL` is not; `coalesce(x, 0) > 5` is not. This is the analysis that turns an outer join into an inner join when a predicate above it rejects the padded rows, which is one of the highest-value rewrites in the set on TPC-DS and is a wrong answer if the analysis is wrong in the permissive direction. Default to not null rejecting for anything not proven, which is always safe.

## Statistics live beside the plan, not in it

A tempting design is to annotate each node with its estimated cardinality. Resist it, for a reason that is specific to this codebase: the plan has a textual form that round-trips, that form is what every test is written against, and a cardinality estimate is a number that changes when the cost model changes. Putting estimates in the plan means every plan test churns whenever the estimator is touched.

So estimates live in a side table keyed by `NodeRef`, built by the estimator, consumed by join ordering and physical planning, and printed by `EXPLAIN` rather than by the plan printer. `EXPLAIN` output is explicitly excluded from the compatibility guarantee in `spec/12-duckdb-compat.md` section 12.5 and is therefore allowed to churn; the plan's textual form is not and should not.

## Binding, and the thing it already gets right

Binding turns a name into an index and a position and a type, once, over the AST. `crates/rudb-bind/src/scope.rs` does it and `ColumnBinding::new(index, at)` is the result.

One thing it does not yet do and should, and it is this folder's first request of the binder rather than of the optimizer. The binder puts every column of the table into `Node::Get`. It has the information to do better: it knows, by the end of binding a `SELECT`, exactly which `ColumnBinding`s were resolved. Narrowing `Get` in the binder rather than in a pass would be simpler and would be wrong, for the reason document 04 gives: the set of columns a query needs is not known until after predicate pushdown has run, because a predicate that moves below a join changes which columns the join has to carry. So the binder should keep producing the full list and the pass should narrow it. The information being available early is a trap here, not an opportunity.

## What we should take from this document

rudb already has the intermediate representation this folder needs, with an arena, thirteen node kinds, eight join kinds, a validator, and a printer and parser that round-trip. That is most of a year of work already done and it is why this folder can start at the passes.

Four representations, not three, with the physical plan arriving in document 10 and not before.

The one structural change required is a node with more than one consumer, needed before common subplan elimination, and it should land on its own because it changes the printer, the parser, the validator and every walk.

Four analyses: elementwise, constant, table set and null rejecting. Build all four before the first pass, cache the table set, and default null rejecting to false.

Cardinality estimates go in a side table keyed by `NodeRef`, not in the plan, because the plan's textual form is what every test is written against and it should not churn when the cost model does.
