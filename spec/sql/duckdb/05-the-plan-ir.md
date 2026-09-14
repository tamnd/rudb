# The shared plan

Document 04 put the cross language seam at the bound plan. This is what the bound plan is today, what it has to grow for SQL alone, and which of those additions a second language would have needed anyway.

## 5.1 What it is today

`crates/rudb-plan` has a `Node` with 14 variants: `Get`, `Dummy`, `Values`, `TableFunction`, `Filter`, `Project`, `Aggregate`, `Sort`, `Limit`, `TopN`, `Distinct`, `Join`, `CrossProduct`, `SetOp`. `JoinKind` has 8 and `SetOpKind` has 3. Plan `Expr` has 8: `Column`, `Constant`, `Cast`, `Compare`, `Conjunction`, `Function`, `Aggregate`, `Case`.

Three properties of the crate are worth more than the variant list. Nothing in it looks anything up in a catalog, so a plan is complete on its own. Every column reference is a `ColumnBinding` rather than a name, so no pass below the binder can be confused by scoping, shadowing or case. And there is a printer and a parser that round trip, tested to a fixed point, which is what makes a plan a thing you can put in a test file and diff rather than a thing you inspect in a debugger.

Those three are the reason this crate can be the seam. They were not built for that and they hold anyway.

## 5.2 What SQL alone still needs

**A window operator.** 13 window function names upstream, zero here, no `OVER` in the AST, and `crates/rudb-bind/src/lib.rs` line 17 refusing them by name. A `Window` node carrying partition keys, an order, a frame specification and a list of window expressions. The frame is the part that is larger than it looks: `ROWS`, `RANGE` and `GROUPS`, each with a start and an end from five kinds of bound, plus `EXCLUDE`.

**A dependent join.** Subquery expressions are 805 records in the upstream corpus and the binder refuses them. The published way to do this is Neumann and Kemper's unnesting of arbitrary queries, BTW 2015, which is what DuckDB implements: bind the correlated subquery to a dependent join and then rewrite it away with algebraic rules until no dependency remains. The alternative, evaluating the subquery per outer row, is correct and is a performance cliff that never gets fixed later because by then everything depends on it. Put the operator in the plan and the rewrite in the optimizer.

**A fixpoint operator.** `WITH` is 1244 records and `WITH RECURSIVE` is the part of it that needs a new node rather than a rewrite. A non recursive CTE is either inlined or materialized and both are expressible today. A recursive one is a loop: evaluate the anchor, then evaluate the recursive term against the last iteration's output until it produces nothing, with a union all or a union distinct between iterations.

This is the single most reusable addition in the document, for a reason section 5.4 returns to.

**An unnest operator.** Needed for `UNNEST` in SQL, which the 118 list function names make unavoidable, and for set returning functions generally. It takes one or more list valued expressions and emits one row per element, with the zip or cross product rule that DuckDB uses when there are several.

**Mutation nodes.** `INSERT` exists as a statement and `UPDATE`, `DELETE`, `COPY` and `MERGE INTO` do not. They are plan nodes, not a side path, because the row source of an `UPDATE ... FROM` or a `DELETE ... USING` is a full query.

**Pivot and unpivot.** DuckDB has both as first class syntax. They bind to an aggregate and a projection over a list of values that may itself come from a query, so they are binder work rather than new nodes, and they are named here because they are easy to mistake for new nodes.

**Source spans.** Document 08 needs a caret under the offending token in a runtime error, not just a parse error, because that is what the pinned binary prints. That means a plan node and a plan expression carry a byte range back into the original text, and the range survives every optimizer pass. Adding a span field after the optimizer exists is a rewrite of every pass, so it goes in before them, not after.

## 5.3 The invariant

No pass below the binder may ask which language the query was written in, and no plan may carry a field whose meaning depends on the answer.

The practical form of this is stronger than it sounds. A `Sort` node carries its null ordering explicitly rather than inheriting the session default, because the session default is per dialect and the optimizer is not. A coercion is a `Cast` node in the plan rather than a rule the executor applies, because the coercion table is per dialect. Division semantics are chosen by the binder picking a function, not by the executor reading a setting. Every one of the nineteen settings in section 2.6 is resolved above this line.

The test for the invariant is mechanical and should be a real test: grep the crates at rank above `rudb-plan` for the dialect type and for the settings type, and fail if either appears. That is worth having as a build check rather than a convention, because this is exactly the invariant that erodes one urgent bug fix at a time.

## 5.4 What the other languages need, and how much of it is new

Take the additions in 5.2 and ask which ones Cypher and KQL would have forced.

A variable length path in Cypher, `(a)-[*1..5]->(b)`, is a fixpoint over an edge relation with a depth bound and a duplicate rule. That is the same operator `WITH RECURSIVE` needs, with different parameters. Document 06 works this through.

`UNWIND` in Cypher and `mv-expand` in KQL are both unnest. Same operator.

`summarize ... by` in KQL is `Aggregate` with a grouping list, and `bin(timestamp, 5m)` is a scalar function inside the grouping key, which `Aggregate` already allows. `make-series` is an aggregate plus a fixpoint or a generated range joined on the left, depending on how the gap filling is expressed.

Cypher's implicit grouping, where the non aggregated return items are the grouping key with nothing written down, is a binder rule and not a plan feature. The formal treatment is in the DBPL 2019 paper on Cypher's semantics, and the relational algebra mapping generally is Junghanns and colleagues' formalisation of openCypher graph queries in relational algebra, arXiv:1705.02844. Both say the same thing for this purpose: the target algebra is the ordinary one.

So of the seven additions in 5.2, three are directly reused by a second language and none of them is made harder by planning for that reuse. That is the argument for doing the seam work now rather than later stated as a count instead of as a principle.

What genuinely is not in the list, and is the honest cost of a graph language, is a path as a value. Document 06 is about whether to pay for it.

## 5.5 Substrait is an export format

Substrait is a cross engine serialization of a relational plan, with a DataFusion binding shipped as the separate `datafusion-substrait` crate rather than as DataFusion's internal representation. That split is the right one and it is the one to copy.

An internal IR is optimized for the passes that rewrite it: cheap to pattern match, cheap to mutate, arena allocated, with invariants the compiler can help hold. An interchange format is optimized for two engines that disagree about everything to still exchange a query: versioned, extensible, textual or protobuf, and necessarily lossy in both directions. Making one thing do both jobs gives you a plan that is awkward to rewrite and an interchange format constrained by our optimizer's needs.

So Substrait, if it happens, is a converter at the edge of `rudb-plan`, in its own crate, with a round trip test against the plan printer that already exists. It is not a reason to change a single field in `Node`.

## 5.6 The order this gets built in

Spans first, because retrofitting them is a rewrite of every pass. Then window, dependent join and unnest, because they are what the corpus is actually blocked on and they are independent of each other. Then fixpoint. Then mutation nodes, which are more executor work than plan work.

Document 12 puts this into the same order as everything else and gives each item the corpus record count that justifies its position.
