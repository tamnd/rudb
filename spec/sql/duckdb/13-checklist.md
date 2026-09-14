# The checklist

Nine milestones, one per batch in document 12. Each block below is meant to be pasted into a milestone issue as it opens, with one pull request per checked item. The measurement next to a milestone is what has to be true for it to close, not what somebody thinks it is worth.

Baseline for every number here: rudb `e0f32d2`, rudb-compat `995f0e8`, DuckDB `v2.0.0-dev84237` at `cc7e7bac7f`, measured 14 September 2026.

## Milestone 1, the force multipliers

- [ ] Signature driven differential over `duckdb_functions()`, with per type boundary sets, reporting pass or fail per name and per overload row
- [ ] Tree aware reducer behind `rudb-compat reduce`, using delta debugging over the arena AST
- [ ] Skip counts by reason, split into harness gaps and real gaps
- [ ] `disabled_optimizers` setting over the six entries in `PASSES`, and automatic per pass bisection on a failing record
- [ ] `rudb-compat report` writing the eleven number page with full provenance
- [ ] Real query corpus, first thousand queries, with a per function histogram
- [ ] `loop`, `foreach` and `require` in the sqllogictest reader, which are the three largest harness gaps
- [ ] `CALL sqlsmith()` pointed at both engines, which is a generated query source with no generator work on our side
- [x] Fuzz target over tokenize and match on raw bytes
- [ ] Structure aware fuzz target over the AST using `arbitrary`
- [x] Plan print and plan parse round trip as a property test over generated plans
- [ ] Grammar driven statement generation, walking the same 1088 rule table in the other direction, with weighted alternatives, a recursion bound and a real catalog
- [x] TLP as an oracle that runs on rudb alone with no DuckDB binary present, in the WHERE form
- [x] The other two TLP forms, over an aggregate and over GROUP BY with HAVING
- [x] NoREC as an oracle that runs on rudb alone with no DuckDB binary present
- [ ] `cargo-llvm-cov` as generator feedback, not as a published number
- [ ] Wall clock, CPU seconds and peak resident set for both engines per record, out of the child process the harness already forks
- [ ] The three ratios on the report page at corpus, feature and worst twenty granularity, as medians with the interquartile range
- [ ] The timing exclusions written down: failures on either side, records under the noise floor, and setup records
- [ ] Seeded, replayable runs, with the six provenance fields recorded per run

Closes when `rudb-compat levels` stops saying that nothing has been measured, and when the loop of generate, run both, compare, reduce, hash and file runs with no person in it.

## Milestone 2, introspection, session and the dialect seam

- [ ] `duckdb_functions`, `duckdb_settings`, `duckdb_types`, `duckdb_keywords`
- [ ] `duckdb_tables`, `duckdb_columns`, `duckdb_schemas`, `duckdb_views`, `duckdb_databases`
- [ ] `duckdb_extensions`, `duckdb_optimizers`
- [ ] `information_schema` and the `pragma_*` function family
- [ ] `current_setting()`, and settings readable as values
- [ ] Session context: `now()`, `current_date`, `current_timestamp`, `current_schema`, `current_user`
- [ ] Session time zone, and the one argument `age`
- [ ] The semantics bundle, holding the nineteen meaning changing settings, consumed by the binder and read by nothing below it
- [ ] Build check that no crate above `rudb-plan` in the layer ranks names the dialect type or the settings type
- [ ] Dialect registry with one entry, `current_dialect` resolved through it, and the measured not installed error
- [ ] `duckdb_dialects()`, `duckdb_grammar_extensions()`, `dialect_compatibility_mode`

Closes when the harness can compute the function coverage denominator from both engines as a query.

## Milestone 3, the three holes in SELECT

- [ ] Source spans from tokenizer through transform, binder and every optimizer pass
- [ ] Dependent join node in the plan
- [ ] Subquery unnesting rewrite in the optimizer, per Neumann and Kemper
- [ ] Scalar, `EXISTS`, `IN` and `ANY` or `ALL` subquery expressions in the binder
- [ ] `WITH`, non recursive, inlined and materialized
- [ ] Window operator with partition, order and frame, covering `ROWS`, `RANGE` and `GROUPS` with `EXCLUDE`
- [ ] The 13 window function names
- [ ] `crates/rudb-bind/src/lib.rs` line 17 no longer refuses anything

Closes when the upstream corpus pass rate has moved by more than the sum of every other milestone to date.

## Milestone 4, the composite types

- [ ] `LIST` vector layout, kernels and cast arms
- [ ] Lambda binder
- [ ] `list_transform`, `list_filter`, `list_reduce` and the higher order family
- [ ] The rest of the 118 list and array names
- [ ] `STRUCT` layout, kernels and the 13 struct names
- [ ] `MAP` layout, kernels and the 14 map names
- [ ] `UNNEST` plan operator and statement syntax
- [ ] No path reaches `crates/rudb-kernels/src/cast.rs` line 505 with an internal error for a type the type system can name

Closes when 145 of the 587 real names pass the milestone 1 differential.

## Milestone 5, the statements

- [ ] Multiple statements in one script
- [ ] `PRAGMA` and `CALL`
- [ ] `UPDATE`
- [ ] `DELETE`
- [ ] `COPY`
- [ ] `PREPARE`, `EXECUTE` and `DEALLOCATE`
- [ ] `BEGIN`, `COMMIT`, `ROLLBACK`
- [ ] `ATTACH` and `DETACH`
- [ ] `ALTER`
- [ ] `TRUNCATE`, `ANALYZE`, `VACUUM`, `CHECKPOINT`
- [ ] `EXPORT` and `IMPORT`

Closes at statement coverage above 75 percent of the 36 grammar alternatives.

## Milestone 6, the error surface

- [ ] Error kind enum fixed to DuckDB's exception type list, with a test that nothing outside it can be produced
- [ ] Not implemented errors carry a kind DuckDB never produces
- [ ] Headline text for every error the corpora produce
- [ ] Location block with `LINE n:`, the truncation window and the caret column
- [ ] `errors_as_json`, and harness comparison in both modes
- [ ] Suggestion line presence and absence
- [ ] Five error levels on the report page
- [ ] Triage merge of `Binder Error` and `Catalog Error` kept out of the level one comparison

Closes when all five error levels are on the page with a real value.

## Milestone 7, the families

- [ ] Aggregate state larger than a scalar
- [ ] Aggregates from 6 names to 78, including the nine `regr_*`, the quantiles, `histogram`, `string_agg` and the `approx_*` family
- [ ] `ORDER BY` inside an aggregate, and `WITHIN GROUP`
- [ ] The 84 date and time names
- [ ] The 76 string names
- [ ] The 67 operators spelled as punctuation
- [ ] The 56 numeric and math names
- [ ] The 36 json names
- [ ] The 8 regexp names
- [ ] The 62 PostgreSQL shims
- [ ] Collation, as one feature, then the 282 collation names
- [ ] Every family ordered within the milestone by the weights from milestone 1

Closes when weighted function coverage is above 90 percent and unweighted is above 70.

## Milestone 8, the rest of SQL

- [ ] Fixpoint operator
- [ ] `WITH RECURSIVE`
- [ ] Pivot and unpivot
- [ ] `ENUM`
- [ ] `VARINT` and `BIGNUM`
- [ ] `TIME_NS` and `TIMESTAMPTZ_NS`
- [ ] JSON type tag mechanism
- [ ] Extension boundary, with `icu` owning collations and time zones as it does upstream
- [ ] Remaining `LogicalType` gaps refused cleanly rather than reaching a type error from the wrong layer

## Milestone 9, the second dialect

Entry rule first, per section 4.7. Do not open this milestone until every box above is checked.

- [ ] Grammar extension composition, with the vendored grammar and its sha256 manifest untouched
- [ ] Rule name resolution across composed tables in the matcher
- [ ] Tokenizer per language over the shared `Token` type
- [ ] Second dialect registered, with its own corpus in the harness
- [ ] Fuzz target for the second dialect's front end
- [ ] Published pass rate for the second dialect on the same page as SQL
- [ ] `duckdb_dialects()` returning two rows, truthfully

## Standing rules

These apply to every milestone and are the reason the checklists above are sizes rather than function names.

Every feature that lands carries its three resource ratios against DuckDB on the records it newly makes passable, recorded by the harness rather than by a person. The goal is 0.1 on time, CPU and peak memory. The per milestone gate is in `12-the-order-of-work.md` and is weaker than the goal on purpose.

No single function pull request, unless the function closes a family or unblocks a milestone item.

No number on the report page that the harness did not compute, and no single headline compatibility percentage anywhere.

No failure reaching a human unreduced.

No dialect in the registry without a corpus, a fuzz target and a published pass rate.

The pin moves in one change across the grammar, the binary and the corpus, with both sets of numbers published for one release.
