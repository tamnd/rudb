# The SQL and DuckDB compatibility folder

Written 14 September 2026, against rudb at `e0f32d2`, rudb-compat at `995f0e8`, and a DuckDB v2.0 alpha built at the same commit the grammar is vendored from, `v2.0-cyanoptera`, `cc7e7bac7f`. Every count in this folder was taken off that binary or out of that tree on that day, and every one of them says where it came from.

## Why this exists

`spec/10-sql-and-types.md` says what the SQL surface is. `spec/12-duckdb-compat.md` says what compatible means across five surfaces. `spec/14-rudb-compat.md` says how a compatibility claim is earned. All three are right and none of them is a document you can implement from, because all three were written to say what the answer will be and none of them was written to say which hundred things to do next or in what order.

This folder is the implementable version, and it exists now because the way the work is currently being done does not scale.

**The measurement that started it.** The pinned binary answers 1159 distinct function names over 3245 overload rows. rudb's function table is 65 entries in `crates/rudb-functions/src/signature.rs` lines 243 to 529, of which 59 are scalar and 6 are aggregate, and 0 are window. That is 5.6 percent of the names. The last four weeks of work added `date_part` fractions and `age`, which is two names, each taking a day of measurement against the binary and a pull request of its own. At that rate the function table alone is twenty years of work. The problem is not that the work is hard, it is that it is being done one function at a time when the binary will answer questions about a hundred at a time and nothing in the repository is set up to ask it that way.

**The second measurement.** The grammar in `crates/rudb-parse/grammar/statements/common.gram` reaches 36 statements. `crates/rudb-parse/src/transform.rs` has 7 arms, at lines 283 to 292, and the other 29 fall through to `unsupported`. The binder refuses subqueries, window functions and `WITH` by its own module doc at `crates/rudb-bind/src/lib.rs` lines 16 and 17. In DuckDB's own corpus that is `WithClause` at 1244 records and `SubqueryExpression` at 805. Those are not one hundred small tasks, they are about eight large ones, and doing them in the right order changes the number by tens of points at a time while doing them in the wrong order changes it by nothing.

**The third measurement, and the one that reframes the whole folder.** The pinned binary has a setting called `current_dialect`, whose value is `duckdb`, described as "The SQL dialect used by the parser". It has a table function `duckdb_dialects()` returning one row, `duckdb`. It has `dialect_compatibility_mode`, described as "Enable SQL dialect compatibility for a certain engine (e.g. `SET dialect_compatibility_mode='spark'`)". It has `active_grammar_extensions`, `allow_parser_override_extension` and a table function `duckdb_grammar_extensions()`. `SET current_dialect='cypher'` answers `Invalid Input Error: Dialect "cypher" is not installed`, which is the error of a registry with nothing in it rather than the error of a feature that does not exist.

So the second query language is not a thing we bolt on later at the cost of the compatibility work. It is part of the compatibility work. DuckDB v2.0 replaced its own parser with a PEG parser in August 2026 and kept every stage below the AST unchanged, which is the same seam we would need, and it shipped the dialect registry in the same release. Building the seam is how we answer `duckdb_dialects()` correctly. Cypher and Kusto are then a dialect each, registered in the same table, rather than a fork of the front end.

## The goal this folder is written against

100 percent DuckDB compatibility, at ten times the performance and a tenth of the memory. Three numbers, not one, and the third is the one nobody publishes.

The first is what documents 01 through 08 and 11 are about. The other two are the project's four axes from `spec/02-the-goal.md` and they would normally live in `spec/15-rudb-bench.md` and be measured at the end. They are in this folder too, in section 1.7 and section 9.7, for one reason: this project has already ordered a year of work by how many corpus records each feature would unlock, and `spec/engine/00-README.md` exists because the result was a database that parses a great deal of SQL and executes all of it through a nested loop join. Doing that again with a better checklist would not be better.

So the harness records time, CPU seconds and peak resident set for both engines on every record it runs, and every feature that lands carries its three ratios. The benchmark suite is still where public numbers come from. This is the early warning that tells you which change did it, on the day it lands.

## What is in here

`01-what-compatible-means.md` turns the five surfaces of `spec/12-duckdb-compat.md` into four countable levels with a named denominator each, and states what is deliberately not counted.

`02-the-surface-and-the-gap.md` is the inventory. What the pinned binary has, what rudb has, and the arithmetic between them, per function type, per data type, per statement, per setting and per introspection table. This is the document the plan in 12 is derived from.

`03-the-front-end-today.md` is what rudb's front end actually is: a hand written tokenizer, a vendored grammar compiled to a flat table, a generic PEG interpreter, a transformer, an arena AST, a binder and a bound plan. It says which of those layers already has no SQL in it and which ones do.

`04-the-dialect-seam.md` is the design. What a dialect is, where the boundary goes, what gets parameterised rather than forked, and how `current_dialect` and `duckdb_dialects()` get answered honestly by a database that supports exactly one dialect today.

`05-the-plan-ir.md` is what the shared plan has to grow to host three languages, why Substrait is an export format rather than the internal one, and the invariant that no pass below the binder may ever ask which language the query was written in.

`06-cypher-and-gql.md` is the graph front end. What a path pattern needs that relational algebra does not have, the two ways it has been done, and why SQL/PGQ and Cypher are the same work in a different order.

`07-kusto-and-the-pipe-languages.md` is KQL and the pipe shaped languages. The operator set, the parts that lower to relational algebra for free, the parts that do not, and ClickHouse's experience of shipping one of these and then downgrading it to experimental.

`08-errors-and-messages.md` treats the error surface as a compatibility surface with its own number, because it is measurable and because the last four differentials in this project all ended at a missing block of error text rather than a wrong answer.

`09-the-harness.md` is what rudb-compat is today, measured, and what it has to become. The four sources of queries, the reducer, the per-pass bisector, the two corpora with opposite rules, and the time, CPU and memory numbers it records for both engines on every record it runs.

`10-generation-and-fuzzing.md` is where the queries come from once the written ones run out. Metamorphic testing, structure aware fuzzing over our own AST, the reduction loop, and the published techniques that found real bugs in databases larger than this one.

`11-the-number.md` is the published metric. How weighted function coverage is computed, what a failure is, what the denominator is, the three resource ratios beside it, and why the correctness page is allowed to go down while a resource ratio going down is a regression.

`12-the-order-of-work.md` is the plan. Batches rather than functions, ordered by what each is worth against the corpus, with the measurement that justifies the order.

`13-checklist.md` is the milestone issue.

## How to read this if you are short of time

Read `02-the-surface-and-the-gap.md` and `12-the-order-of-work.md`.

The first one is the honest size of the problem, and the reason to read it before anything else is that most of the 1159 names are not 1159 problems. They are about forty families, and a family closes in one change once the machinery under it exists. The document says which families and how big each one is, and that is the number that decides whether this project is two years or twenty.

The second one is what to do on Monday, and it is arranged so that the first six weeks of it are worth more than the rest of the year. Nothing in it is a function.

If you have time for a third, read `04-the-dialect-seam.md`, because it is the one decision in this folder that is expensive to change later and cheap to make now, and because the measurement in it means it is not a speculative feature.

## A note on sources

Counts off the pinned binary were taken by running the statement on server2 against `v2.0.0-dev84237`, `cc7e7bac7f`, on 14 September 2026, and the statement is given next to the count. Counts out of the rudb tree name the file and, where it is a single fact rather than a whole file, the line. Numbers from the upstream corpus are from the rudb-compat README at `995f0e8` and are labelled as that rather than as a fresh run.

Where a claim about another system is quoted it names the project, the file or the document, and the date it was checked. Where a claim is an inference from those sources rather than something a source states, it says so in the sentence. The rule is the one `spec/01-research-2026.md` sets and it is worth repeating here because this folder cites more outside work than any other in the tree.

This folder does not repeat `spec/10-sql-and-types.md` or `spec/12-duckdb-compat.md`. It assumes both, cites both, and where it disagrees with either it says so in the text rather than quietly.
