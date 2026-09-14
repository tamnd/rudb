# What compatible means, in numbers

`spec/12-duckdb-compat.md` names five surfaces and four levels. `rudb-compat levels` prints the four levels and says none of them has been measured yet, which is true and is the thing this folder is written to end. This document gives each level a denominator that a machine can count, so that the sentence "we are at 41 percent of level two" has exactly one meaning.

## 1.1 The four levels and their denominators

**Level one, data.** A DuckDB file opens in rudb and every value in it reads back identically, and a file rudb writes opens in DuckDB. The denominator is the set of storage format versions we claim, times the set of compression methods the writer can choose, times the modification states a file can be in. It is not a query count. This folder does not cover it beyond saying that it is measured separately and that `spec/05-storage.md` owns it.

**Level two, query.** A statement accepted by DuckDB is accepted by rudb and returns the same answer, and a statement DuckDB refuses is refused by rudb with the same class of error. This is what this folder is about and it has three denominators that get published separately rather than averaged, because averaging them hides which one is moving.

The first is **statement coverage**, over the 36 alternatives of the `Statement` rule in `crates/rudb-parse/grammar/statements/common.gram`. That is the cheapest number to move and the least meaningful on its own, since `SELECT` is one of the 36 and is most of the work.

The second is **function coverage**, weighted. The denominator is the 1159 distinct names in `duckdb_functions()` on the pinned binary, weighted by how often each appears in the real query corpus, per the rule in `spec/10-sql-and-types.md` section 10.7. The unweighted number is published next to it because the weighted one flatters us and a single number that only flatters is not a measurement.

The third is **corpus pass rate**, over DuckDB's own `sqllogictest` files. At `995f0e8` that is 4096 files, 13868 records passed and 50925 failed, 21.4 percent of what was attempted, with 14314 skipped of which 13392 the file turned off itself. The skip count matters as much as the pass rate, because a corpus where a third of the records never ran is a corpus that can go up without anything improving.

**Level three, API.** The C API, struct layouts, entry points and lifetimes. Counted against DuckDB's versioned ABI description rather than against a guess, per `spec/14-rudb-compat.md` section 14.6. Out of scope here.

**Level four, ecosystem.** Extensions load and their own tests pass. The pinned binary lists 31 rows in `duckdb_extensions()`, of which 6 are installed and loaded in a default build: autocomplete, core_functions, icu, json, parquet and shell. That 6 is the honest near term denominator and the 31 is the long one. Out of scope here except where an extension defines part of the SQL surface, which `core_functions` and `json` both do.

## 1.2 The rule that makes the number mean something

`spec/10-sql-and-types.md` section 10.7 says a function that is implemented but differs from DuckDB on any tested input counts as not implemented. Keep it, and read it strictly: `age` counts as implemented only while the differential over its generated inputs is empty, and it goes back to not implemented the day one input disagrees. The number is allowed to go down. A coverage number that can only go up is a count of pull requests wearing a percentage sign.

The same rule applies per level. A statement counts as supported when every form of it in the corpus either works or fails the same way DuckDB fails, not when the parser stops refusing it.

## 1.3 What an answer is

Two answers are the same when the rows match as a set, or as a sequence when the statement has a top level `ORDER BY`, and when the column names and column types match too. rudb-compat already does exactly this in `src/compare.rs`, comparing column count, names, types, row count and every cell, and deciding sorted versus as written from a real parse of the statement rather than from a string search for the words. Keep that. Comparing a hash or a row count is the single most common way a differential harness reports success it has not earned.

Cells are compared as the text each engine printed. That is rudb-compat's decision in `src/engine.rs` and it is the right one: it keeps the harness out of the business of owning a float printer and a decimal printer of its own, and it makes formatting differences visible, which they should be, because a user who pipes our output into a script is depending on the formatting.

Floating point comparison is exact. A tolerance is available and using it puts a written reason on the test. This is already the project rule in the rudb-compat CONTRIBUTING and it is repeated here because tolerance is how aggregation order bugs hide.

## 1.4 Errors are answers

A statement that errors on DuckDB must error on rudb. rudb-compat splits an error into a kind, the text before the first colon, and a message, and matches on kind by default with `--strict-messages` raising the bar to the headline. That is the right two level design and document 08 takes it further, because the kind alone is too coarse to be the published number: `Binder Error` and `Catalog Error` are merged into one reason in `src/conform.rs` on the argument that from the outside they are the same complaint, which is true for triage and false for a user reading a message.

## 1.5 What is deliberately not counted

**Performance, as a pass condition.** A statement that returns the right rows fifty times slower is still a correctness pass. It is not a milestone pass, and section 1.7 is how that distinction is kept without letting either number hide the other.

**DuckDB's bugs, as bugs.** Where DuckDB is wrong, matching it is what this suite rewards, because a user migrating depends on the behaviour they have. Where we know DuckDB is wrong we record it as a known difference with a note, and we still match it. Correctness as distinct from compatibility is `spec/16-testing.md`.

A difference that turns out to be an upstream bug is still worth the work of reproducing, because a known difference with no reproduction is a rumour in the same way an unprovenanced number is. It gets reduced by the same reducer as everything else, written up with the statement, the two answers and the pinned commit, and filed in our own fork at `tamnd/duckdb`. Not upstream. We are reimplementing their engine and reading their tests, and a stream of bug reports from that position is not a contribution anybody asked for. The fork is where they are recorded, and if one of them ever matters enough to report it is a deliberate decision rather than a side effect of running a harness.

**Undefined order.** A statement with no `ORDER BY` has no defined row order, so we compare sorted and we do not claim the orders match. What we do not do is silently sort a statement that has an `ORDER BY` inside a subquery and none at the top, which is exactly the case rudb-compat's `row_order` gets right today.

**Timings, plans and progress output.** `EXPLAIN` output is compared structurally where it is compared at all, and never as text, because matching another optimizer's plan text is matching its optimizer.

**Anything that depends on a clock, a hostname or a temp path.** Those are excluded by name in the corpus rather than by a fuzzy filter, so that the exclusion list is readable and can be argued with.

## 1.6 Where the dialect registry fits

A fifth thing is measurable and is not in the five surfaces: the pinned binary reports `current_dialect` as `duckdb`, lists its dialects in `duckdb_dialects()`, and refuses an unknown one with `Invalid Input Error: Dialect "cypher" is not installed`. `dialect_compatibility_mode` accepts `spark` and reports it back, while an unrecognised value answers `Not implemented Error: Enum value: unrecognized value "nope" for enum "DialectCompatibilityMode"` with a candidate list containing only `NONE`.

Answering those three correctly is level two work, not a future feature, and it is the reason document 04 is in this folder rather than in a separate one about other query languages. A database that claims DuckDB compatibility and has no dialect registry has a hole in the surface exactly the size of the seam every other front end would plug into.

## 1.7 The resource axis, recorded here and gated per milestone

The project goal is 100 percent compatibility at ten times the speed and a tenth of the memory. Two of those three numbers are not compatibility numbers, and the temptation is to leave them to `spec/15-rudb-bench.md` and find out at the end that the feature we shipped to close a corpus gap is the feature that made the engine slow. That has a name in this project already: `spec/engine/00-README.md` exists because the M2 work was ordered by how many records each missing feature would unlock, and the result was a database that parses a great deal of SQL and executes all of it through a nested loop join.

So the harness records the resource numbers itself, on the same run, for both engines, on every record it executes. Wall clock, CPU seconds, and peak resident set, per statement, per engine, from the same child process isolation the harness already has. That costs almost nothing because both engines are already being run on every record, which is the whole point of doing it here rather than in a second suite.

What it is not: a pass condition. A record passes on the answer. A slow right answer is a pass, because the alternative is a harness where a correctness regression and a performance regression are the same red mark and neither gets diagnosed.

What it is: three ratios published beside the correctness numbers, per record, per feature, and per milestone. Time ratio, CPU ratio and peak memory ratio, each as rudb over DuckDB, each reported as a median over at least five runs with the interquartile range, never a minimum, per the ten reporting rules in `spec/15-rudb-bench.md` section 15.1. The goal is 0.1 or better on all three. The milestone gate is weaker than the goal and is stated per milestone, because a feature landing at parity is progress and a feature landing at ten times slower than DuckDB is a design that has to be looked at now rather than after five more land on top of it.

The per record granularity is what makes this worth doing. A benchmark suite tells you the engine got slower. A per record resource table tells you which of the 4096 files got slower and therefore which change did it, which is the same argument section 9.6 makes for the pass bisector and the same argument section 9.5 makes for the reducer. The instrument is only useful if it points at something.

Three things are deliberately excluded from the ratios so they measure what they claim. Records that fail on either side, since timing a failure is timing an error path. Records under a floor of a few milliseconds on both engines, where process startup dominates and the ratio is noise. And anything in the corpus that is testing an error message or a `PRAGMA`, since those are setup rather than work. The floor and the exclusion list are written down in the harness rather than applied by feel.
