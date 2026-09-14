# Kusto and the pipe shaped languages

Same exercise as document 06, on a language that is much closer to relational algebra than a graph language is, which makes it the useful second test of the seam rather than a repeat of the first.

## 7.1 The shape

A KQL query is a table name followed by a chain of operators separated by `|`, each taking a table and returning a table. Read order is execution order, which is the whole point of the design and is the opposite of SQL, where the clause you read first is evaluated fifth.

That difference is smaller than it looks for us, for the reason document 03 gave. The binder already walks SQL in execution order, FROM then WHERE then GROUP BY then HAVING then SELECT then DISTINCT then ORDER BY then LIMIT, because that is the order it has to build the plan in. A pipe language is written in the order the binder already works in. The pipe binder is the simpler of the two.

## 7.2 The operators that are free

Taking the core operator set against the fourteen plan nodes in section 5.1 plus the seven additions in 5.2:

| KQL | plan |
| --- | --- |
| `where` | `Filter` |
| `project`, `project-away`, `project-keep`, `project-rename`, `project-reorder` | `Project` |
| `extend` | `Project` that keeps its input columns |
| `summarize` | `Aggregate`, grouping keys may be scalar calls such as `bin(t, 5m)` |
| `sort by`, `order by` | `Sort` |
| `take`, `limit` | `Limit` |
| `top` | `TopN` |
| `distinct` | `Distinct` |
| `union` | `SetOp` |
| `join` | `Join`, with the exception in 7.3 |
| `lookup` | `Join`, left outer, with a uniqueness expectation |
| `mv-expand` | the unnest operator from 5.2 |
| `range` | `Values`, or a generator table function |
| `let` | a CTE, so the same binder machinery `WITH` needs |
| `parse`, `parse-where` | `Project` over regex functions, so it needs the `regexp` family and nothing structural |
| `evaluate` | `TableFunction`, which already exists |

Fifteen of the roughly twenty operators that matter are already spelled in the plan or are spelled by work SQL needs anyway. No new types at all, which is the sharpest contrast with document 06.

## 7.3 The five that are not free

**`join` defaults to `innerunique`.** KQL has nine join kinds and the default is not inner join. `innerunique` deduplicates the left side on the join key before joining, so a query written without naming a kind does not mean what the same query means in SQL. This is a tenth `JoinKind` or a binder rewrite into `Distinct` plus `Join`, and either is fine, but it must be decided explicitly because getting it wrong produces right looking answers with wrong row counts. KQL also has no cross join in the SQL sense, which is a restriction rather than a feature and costs nothing.

**`serialize` and the row context functions.** `row_number()`, `prev()` and `next()` need an ordered row context, which is the window operator from section 5.2. So KQL does not need a window operator built for it, it needs the one SQL is already blocked on.

**`make-series`.** Gap filling over a time grid with a step, then aggregation into arrays per group. That is a generated range left joined against the data, then an aggregate, then the list machinery. All parts exist in the plan once 5.2 is done, and the binder work is real.

**`scan`.** A row by row state machine over an ordered stream with declared states and transitions. This is genuinely a new physical operator and it is sequential by nature, so it is the one place a KQL query would not vectorize. It is also rarely used and can be refused for a long time without the language being useless.

**`render`.** Not algebra at all. It is a directive to the client about how to draw the result. It belongs in result metadata, on the channel that carries column names and types, and the correct implementation is to carry it and let the client ignore it. Putting it in the plan would be the first violation of section 5.3.

That is the whole list. One join kind, one dependency on window functions, one binder heavy operator, one sequential operator that can wait, and one thing that is not a query feature.

## 7.4 Pipe syntax in SQL is a different question

There are two pipe shaped things and they land on opposite sides of section 4.2's two seams.

Google published pipe syntax for SQL at VLDB 2024, the paper titled "SQL Has Problems. We Can Fix Them: Pipe Syntax In SQL", and it is SQL: same expressions, same names, same semantics, written in execution order. DuckDB's own v2.0 announcement demonstrates its new `GrammarExtension` by adding BigQuery style pipe syntax, which is exactly the right demonstration because it shows the feature is a grammar and a transformer over the existing AST.

So pipe syntax for SQL is an intra family dialect: shared AST, different grammar, same semantics bundle. KQL is a separate language: its own AST, its own binder, meeting at the plan. They look identical from a distance and they are different pieces of work, and a project that does not separate them ends up with KQL features leaking into the SQL AST.

## 7.5 The two warnings

ClickHouse implemented a `kusto` dialect behind its `dialect` setting, alongside `prql`, and in version 25.1 marked both experimental after parser crash bugs, having shipped them without the fuzzing and conformance coverage its main parser gets. That is the entry rule in section 4.7 and it was written from this example.

Worth adding, as an inference rather than something the changelog says: a dialect implemented as a second parser producing the primary engine's SQL AST inherits every assumption that AST makes about SQL, and the mismatches surface as parser bugs rather than as design discussions. That is section 4.2's two seams argued from the failure side.

Microsoft's own position is the second warning and it points the other way. The published cross language story for KQL is translation: a SQL to KQL cheat sheet, and an Azure Data Explorer feature where a T-SQL query prefixed with `-- explain` returns the equivalent KQL. The Fabric T-SQL endpoint over KQL databases is an emulation layer, not a shared algebra.

Translation between surface languages and multiple front ends over one algebra are different projects. Translation is lossy at every unmatched feature and the loss is visible to the user as a query that does not translate. One algebra has no such failure mode: a feature either exists in the engine or it does not, independently of which language asked. This folder proposes the second and it is worth saying plainly that the largest vendor in this particular language chose the first, because their constraint was two existing engines and ours is one engine that is mostly unwritten.
