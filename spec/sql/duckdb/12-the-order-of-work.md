# The order of work

Nine batches. Nothing in the first two is a function, and that is the point of the whole folder. Every batch says what it is worth against a measured number, and where a figure is an estimate rather than a count it says so.

The ordering rule is one sentence: build the things that measure before the things that are measured, then the things that unblock families before the families, then the families.

## Batch 0, stop doing it the other way

Not work, a decision, and it belongs at the top because everything else depends on it.

The last four weeks shipped two function names, each with a day of hand measurement against the pinned binary and a pull request of its own. That approach ends here. No more single function pull requests, because section 10.1's generator measures 3245 overload rows in one run and a day of hand measurement covers one.

The exception is a function that closes a family or is needed by a batch below.

## Batch 1, the force multipliers

**The signature driven differential.** Section 10.1. Read `duckdb_functions()`, generate calls per overload from per type boundary sets, run both engines, record per name. This is the item that turns twenty years into a schedule.

**The tree aware reducer.** Section 9.5. `reduce` currently prints that it is not built yet. Delta debugging over the arena AST. The rule it earns: no failure reaches a human unreduced.

**Skip counts by reason.** Section 9.3. Split the 13392 self skipped records into harness gaps and real gaps. A day of work, and until it is done the 21.4 percent pass rate cannot be interpreted.

**The pass bisector.** Section 9.6. A `disabled_optimizers` setting over the six entries in `PASSES`, plus the re run on failure. Upstream already has the same setting so both sides can be bisected.

**`report`.** Section 11.2. The page with eleven numbers and their provenance.

**The real query corpus.** Section 9.2. The thing that supplies the weights, without which every percentage in this folder is the unweighted one. Sources are DuckDB's documentation examples, the benchmark suites, the issue tracker and tutorials.

**Resource recording on every record.** Section 9.7. Wall clock, CPU seconds and peak resident set for both engines out of the child process the harness already forks, with the three ratios on the report page. This belongs in the first batch and not the last, because its whole value is telling you which change made the engine slower on the day the change lands.

**The generators and the fuzz targets.** Document 10. `CALL sqlsmith()` pointed at both engines first, because it costs no generator work at all. Then the first fuzz target over tokenize and match, then the structure aware one over the AST, then grammar driven statement generation off the same 1088 rule table the matcher walks. Then TLP and NoREC as oracles that run on rudb alone, which is what makes a fast pre merge check possible without a DuckDB binary present.

Worth: no direct movement in any published number, and every number below is uninterpretable without it.

## Batch 2, introspection and the session

**The catalog tables.** `duckdb_functions`, `duckdb_settings`, `duckdb_keywords`, `duckdb_types`, `duckdb_tables`, `duckdb_columns`, `duckdb_schemas`, `duckdb_views`, `duckdb_extensions`, `duckdb_optimizers`, `duckdb_databases`, `duckdb_dialects`, `duckdb_grammar_extensions`, the `pragma_*` family and `information_schema`. `crates/rudb-catalog/src/lib.rs` has none of them and the data is already there, the table function mechanism is already there, and `rudb_strategies` already proves a table function backed by an in memory list works. `duckdb_keywords` is nearly free because `crates/rudb-parse/src/generated/keywords.rs` is compiled from the same five vendored `.list` files, so the classification is correct by construction.

**`current_setting()` and settings as values.** `crates/rudb/src/settings.rs` accepts `SET` for a constant and its own doc says these are missing.

**The session.** `now()`, `current_date`, `current_timestamp`, `current_schema`, `current_user`, a session time zone. This is why the one argument form of `age` could not ship with the two argument form, and it blocks the `current_*` part of the 62 PostgreSQL shims and the time zone half of the 84 date and time names.

**The dialect registry with one entry.** Document 04. `current_dialect` resolved through a real lookup so the not installed error comes out of a registry, `duckdb_dialects()` over that registry, `dialect_compatibility_mode` matching the measured behaviour, `duckdb_grammar_extensions()` empty.

**The semantics bundle.** Section 4.3. Take the nineteen meaning changing settings out of the binder's constants and into one struct the binder is handed.

Worth: the 62 shim family becomes reachable, a large part of what tools do on connect starts working, and the property at the end of section 2.6 arrives, which is that the harness can compute the coverage denominator from our side as a query rather than from a spreadsheet.

## Batch 3, the three holes in SELECT

`crates/rudb-bind/src/lib.rs` line 17 refuses subqueries, window functions and `WITH`. In the upstream corpus those are 805 and 1244 records and the third has no count because nothing gets far enough to record one.

**Source spans first.** Section 5.2 and section 8.5. Retrofitting them after the optimizer grows is a rewrite of every pass, and they are what the caret in an error message needs.

**The dependent join and subquery unnesting.** Neumann and Kemper, BTW 2015, which is what DuckDB implements. The operator goes in the plan and the rewrite goes in the optimizer, and the per outer row evaluation shortcut does not get built even temporarily.

**`WITH`, non recursive.** Inline or materialize. The recursive form waits for batch 8.

**The window operator.** Partition, order, frame with `ROWS`, `RANGE` and `GROUPS`, and the 13 window function names.

Worth: the single largest corpus movement available. The estimate is tens of points on the corpus pass rate, and it is an estimate because a record blocked on two of the three only unblocks when both land.

## Batch 4, the composite types

Section 2.4 found the sharpest defect in the tree: `LIST`, `STRUCT`, `MAP`, `UNION` and `ARRAY` exist as `LogicalType` variants with no vector layout, no kernel and no cast arm, so a query naming one gets an internal error at `crates/rudb-kernels/src/cast.rs` line 505 instead of a refusal.

**The `LIST` physical layout and its kernels.** Then the lambda binder, which gives `list_transform`, `list_filter` and `list_reduce` and the rest of the higher order family.

**`STRUCT` and `MAP`.** Then `UNNEST`, which is the plan operator from section 5.2 and is also what KQL's `mv-expand` and Cypher's `UNWIND` become.

Worth: 118 list names plus 14 map plus 13 struct is 145 of the 587 real names, about ten points unweighted, and it unblocks the structural half of the 36 json names. Section 2.2 called this out as the thing the last month got wrong, in the sentence that shipping `age` moved the number by 0.09 percent.

## Batch 5, the statements

7 of 36 today. In order of corpus value rather than alphabetically: `PRAGMA` and `CALL`, because the corpus uses them constantly for setup rather than as a subject, so they unblock records about other things. Then `UPDATE` and `DELETE`, then `COPY`, then `PREPARE` and `EXECUTE`, then the transaction statements, then `ATTACH` and `DETACH`, then `ALTER`.

Also here: `crates/rudb-bind/src/statement.rs` line 158 refuses a script of more than one statement, and the corpus is full of them.

Worth: statement coverage from 19 percent to the high 70s, and a quantity of corpus records that are blocked on setup rather than on the statement under test, which is why `PRAGMA` and `CALL` lead.

## Batch 6, the error surface

Document 08. The kind taxonomy fixed to DuckDB's exception type list with a test. The headline text for the errors the corpus actually produces. The location block, which is cheap once batch 3 put the spans in. `errors_as_json` and the two mode comparison from section 8.6. The suggestion line's presence or absence, with the exact candidate left for later and its own generated corpus.

Also here: our not implemented errors get a kind DuckDB never produces, so they can never score as a level one pass.

Worth: five numbers that currently do not exist, and it is the surface the last four differentials in this project actually ended at.

## Batch 7, the families

Now, and only now, functions in bulk, ordered by the weights batch 1 produced. The families from section 2.2: date and time at 84, string at 76, the punctuation operators at 67, numeric at 56, json at 36, regexp at 8, and the aggregates from 6 to 78 which needs an aggregate state larger than a scalar before `quantile`, `histogram` and `string_agg` are possible.

Collation is one item and it is 282 names. It is deliberately late, because moving the unweighted number by 24 points with one feature is the most tempting way to make the page lie, and section 1.2's rule means it only counts when the collations actually collate.

Worth: this is where the function percentage moves, and by then it moves in families of dozens rather than one name at a time.

## Batch 8, the rest of SQL

The fixpoint operator and `WITH RECURSIVE`. `WITHIN GROUP` and `ORDER BY` inside an aggregate, neither of which exists anywhere in the AST, plan or executor. Pivot and unpivot. The missing types from section 2.4: `ENUM`, `VARINT` and `BIGNUM`, `TIME_NS`, `TIMESTAMPTZ_NS`, and the JSON tag mechanism. Extension boundaries, since `icu` is where the collations and the time zones live upstream and should live here too.

## Batch 9, the second dialect

Not before section 4.7's entry rule can be met: a corpus in the harness, a fuzz target, and a published pass rate, on the same terms as SQL. Documents 06 and 07 say which language is cheaper, and it is KQL by a wide margin, since it needs one join kind, one dependency on the window work in batch 3, one binder heavy operator and no new types, against the graph language's new type, new physical operator and per row path state.

The seam that makes either possible is built in batch 2 and costs almost nothing there because it is the honest way to answer three questions the pinned binary already answers.

## The resource gate on each batch

The project goal is 100 percent compatibility at ten times the speed and a tenth of the memory, and section 1.7 puts the second and third of those in this harness rather than only in the benchmark suite. So every batch from 2 onwards carries a resource gate as well as a correctness one, over the records that batch newly made passable.

The gates get stricter as the batches go on, for a reason that is not negotiable. An engine missing window functions cannot be measured against DuckDB on records that use them, so early ratios are computed over a small and unrepresentative slice and a strict early gate would be measuring noise. The numbers below are the gate, not the goal, and the goal is 0.1 throughout.

Batch 2 and 3: no gate on the ratio itself, because the passable set is still changing shape too fast for a median to mean anything. The requirement is only that the three ratios are recorded and published for every record from the day batch 1 lands.

Batch 4 and 5: the median time ratio at or below 1.0 on the newly passable records, which is parity. Peak memory at or below 1.0. Anything above 3.0 on a single record is an issue with a reduced case, filed and linked from the batch's milestone, because a single record that far out is a shape problem rather than a tuning problem.

Batch 6 and 7: median time ratio at or below 0.5, median peak memory at or below 0.5, and the worst record no more than 2.0.

Batch 8: median time ratio at or below 0.1, median CPU ratio at or below 0.1, median peak memory at or below 0.1, on the whole corpus rather than on a slice. That is the project goal and batch 8 is where the corpus is finally representative enough to hold it.

Batch 9 inherits batch 8's gate and adds nothing, because a second front end that changes the engine's numbers has been built at the wrong seam.

Where a gate is missed the batch does not stop. The miss is written on the report page, the worst records are reduced and filed, and the next batch carries them. What is not allowed is closing a batch without the numbers, because the failure this ordering exists to prevent is a year of work ordered by corpus records with nobody watching the engine.

## What this ordering refuses to do

It refuses to ship functions one at a time, which is batch 0. It refuses to chase the unweighted percentage through collations, which is why batch 7 is late. It refuses to start a second language before the first one's harness can hold it, which is batch 9. And it refuses to close the SQL gap first and factor the front end later, which section 3.5 argued is the path every project cited in this folder took before it gave up and factored.

## How it gets done

One milestone per batch, one pull request per item, labelled, with the milestone checklist updated and a comment on the milestone issue as each lands. Tests run on server1, server2, server3 or the gaming machine rather than locally. A patch release after a run of merged work, and a minor release when a batch closes. Nothing in that is new, it is the process already in use, and the only change this folder makes to it is the size of the thing inside each pull request.
