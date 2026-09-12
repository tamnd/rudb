# rudb-compat: how a compatibility claim is earned

`rudb-compat` is a separate repository containing the apparatus that turns "compatible with DuckDB" from an assertion into a number that a machine computes. It is separate because it depends on DuckDB and on `rudb` simultaneously, because it should be runnable by someone who trusts neither, and because a compatibility suite that lives inside the implementation it tests is a suite whose failures are easy to explain away.

## 14.1 The principle

**No compatibility claim appears anywhere without a test that produced it.** Not in the README, not in a release note, not in a talk. The four levels in document 12.8 each have a suite, each suite produces a percentage and a failure list, and both are published on every commit.

**The comparison is always against a real DuckDB binary of a named version**, run in the same process or a subprocess on the same data on the same machine. Not against documentation, not against a remembered behaviour, and not against a previous run.

## 14.2 Differential query execution

The core loop: take a query and a database, run it on DuckDB, run it on `rudb`, compare.

**Comparison is on the full result set, not on a hash and not on a row count.** Values, types, column names, and the order when the query has an `ORDER BY`. When the query has no `ORDER BY`, results are sorted before comparison, since order is not guaranteed.

**Floating point comparison is exact by default.** A tolerance mode exists and using it requires the test to declare why, because a silently tolerant comparison hides real bugs in aggregation order and in the compiled-versus-interpreted agreement that document 8.4 makes a hard rule.

**Errors are results.** A query that errors on DuckDB must error on `rudb`, with a matching error code and, where document 12.5 requires it, a matching message. A query that errors on one and succeeds on the other is a failure regardless of which way round.

**Every failure is automatically reduced.** A failing query is shrunk by removing clauses, simplifying expressions and reducing the data until a minimal reproduction is found. Without this the failure list from a fuzzer is unusable, because a 40-line generated query that produces a wrong answer tells you nothing about why.

**Every failure is automatically bisected against the optimizer passes** per document 9.1, so the report says which pass introduced the difference. This is the single highest-value piece of tooling in the whole harness, because most wrong answers come from a rewrite and finding out which one by hand takes an afternoon each.

## 14.3 Where the queries come from

Five sources, in increasing order of how much they find.

**DuckDB's own test suite.** Their `sqllogictest` corpus is tens of thousands of queries with expected results, written by the people who know where the edges are. Running it is the single highest-value first step and it is what M2 does. Their test files also encode expected error messages, which is where document 12.5's message-matching requirement gets its actual list.

**The benchmark suites.** TPC-H, TPC-DS, JOB, CEB, ClickBench, H2O. Modest in count and they exercise real plan shapes.

**A corpus of real queries** scraped from public repositories, notebooks and documentation. This is what tells us which functions actually get used, which is what document 10.7's weighting is computed from.

**Generated queries.** A grammar-based generator producing random valid SQL over a random schema, in the spirit of SQLancer and SQLsmith. This is where the volume is and where most of the wrong answers will come from.

**Metamorphic queries.** Transformations that must preserve the result: a predicate rewritten to a logically equivalent form, a join reordered, an aggregation expressed two ways, a query wrapped in a subquery. These find bugs that a random generator does not, because they produce pairs of queries whose relationship is known even when the correct answer is not. SQLancer's TLP and NoREC techniques are the published versions of this idea and they have found real bugs in every major database they have been pointed at.

## 14.4 Function-level differential testing

Separate from query-level, because it is much denser and finds different things.

**For every function in `duckdb_functions()`, generate inputs and compare outputs.** Inputs are drawn from a per-type generator that weights edge cases heavily: null, zero, one, minimum and maximum of the type, values around the type's boundaries, empty string, single character, very long string, invalid UTF-8, strings with embedded nulls, dates at year boundaries and around leap days, timestamps at daylight-saving transitions and at the epoch, decimals at maximum precision, infinities and NaN, empty lists and deeply nested structures.

**This is where the compatibility percentage in document 10.7 comes from**, and the rule from that section applies: a function that differs on any tested input counts as not implemented.

## 14.5 Storage format testing

**Round trip in both directions.** Create a database in DuckDB with a wide variety of types, encodings and modifications, open it in `rudb`, verify every value. Create the same in `rudb`, open in DuckDB, verify. Repeat for every storage version we claim to support.

**Data generated to hit every compression path.** A table whose columns are specifically constructed so that DuckDB's compression selector chooses each of its methods, so the reader is tested on real instances of `DICT_FSST`, `ALPRD`, `ROARING`, `CHIMP` and the rest rather than only on the common ones.

**Modification patterns**, not just fresh loads: a database with deletes, with updates, with an unclean WAL, with multiple checkpoints, with an ART index, with attached secondary databases.

**Corpus of real files.** Databases produced by every DuckDB minor version we support, kept as test fixtures. This is the only way to catch a format detail that the current version no longer writes but that old files contain.

## 14.6 C API conformance

**Struct layouts, sizes, offsets and enum values checked against a reference generated from the real header**, per document 12.3. A mismatch here is silent memory corruption and it must be a compile-time or test-time failure.

**A set of C programs that exercise every entry point**, compiled once against `libduckdb` and once against `librudb`, run, and their outputs compared. Including the error paths, including the lifetime and ownership rules, run under AddressSanitizer so that a lifetime mistake is a failure rather than a latent bug.

**The generator reads DuckDB's versioned ABI YAML**, so a new DuckDB version produces a new set of conformance tests automatically and the delta is visible immediately.

## 14.7 Extension compatibility

**The published per-extension status table** from document 12.4 is generated: for each extension in the community and core repositories, attempt to load it, run its own test suite if it has one, and record loaded, loaded-with-failures, or failed-to-load with the reason.

This is the least predictable surface and the table is expected to be patchy for a long time. Publishing it patchy is better than not publishing it.

## 14.8 Continuous operation

**The full suite runs nightly.** A fast subset runs on every commit and gates merges.

**The fuzzers run continuously** on dedicated machines, with a corpus that persists across runs and a coverage-guided mutator. New crashes and new wrong answers are filed automatically with their reduced reproduction attached.

**Every DuckDB release triggers a full re-run against the new version**, and the report is the delta: which functions are new, which behaviours changed, what our coverage number moved to. This is how the treadmill in document 12.7 is kept visible rather than allowed to become a slow surprise.

**The published artifact is a single page** with the four compatibility levels, their current percentages, the DuckDB version they were computed against, the date, and links to the failure lists. It updates automatically and nobody is allowed to edit it by hand.

## 14.9 What this suite cannot do

It compares against DuckDB's behaviour, so where DuckDB has a bug, matching it is what the suite rewards. That is mostly correct, since a user migrating depends on the behaviour they have rather than on the behaviour that is right, but it means the suite is not a correctness suite. Document 16 covers correctness separately and the two are not substitutes.

It cannot test what it does not generate. A grammar-based generator explores the space its grammar describes, and the bugs that matter most are often in the corners the grammar author did not think of. This is a known limitation of the technique and the mitigation is the real-query corpus, which contains shapes nobody would have generated.

It cannot measure performance compatibility. A query that returns the right answer 50 times slower than DuckDB is a passing test and a failed product. That is document 15's job and the two suites should be read together.

## 14.10 The divergence ledger

Section 14.9 says the suite rewards matching a DuckDB bug and section 02.4 says bug-for-bug compatibility is not claimed on undefined behaviour, error message text, `EXPLAIN` output or order-dependent floating point. Between those two there is a third case, which is a place where DuckDB is plainly and reproducibly wrong about a defined answer. This section is the list of those, it is the only place a divergence is allowed to be recorded, and a divergence that is not in it is a bug in `rudb`.

**An entry is only an entry if it carries a reproduction.** A minimal query and the data to run it on, the exact DuckDB version it was seen on, both answers, and the argument for which one is right that does not appeal to our own implementation. Usually that argument is DuckDB disagreeing with itself, since a second formulation of the same question in the same binary is the cheapest evidence there is and the hardest to wave away.

**An entry says why we are not matching.** The default is to match, because a user migrating depends on the behaviour they have. Not matching has to be argued, and the argument has to be about something other than taste. A wrong answer that a user could act on is the usual one.

**An entry names where it is tracked.** An issue here and, where the behaviour is not deliberate upstream, a report there. An entry with no upstream report is an entry nobody has finished.

**An entry is temporary by default.** When upstream fixes it, the entry is deleted and the query goes back into the ordinary comparison. The ledger getting shorter is the healthy direction.

### 14.10.1 `SUM` over a `BIGINT` column read from parquet

On `v2.0.0-dev84237 (Development Version) cc7e7bac7f`, the ungrouped sum of a `BIGINT` column taken straight off a parquet scan drops carries out of the high word of its 128 bit accumulator. Over the ClickBench file it is short by `282 * 2^64`, which moves `AVG(UserID)` in the fifth significant figure and is what makes ClickBench query 4 disagree.

The reduction is two statements over the public file. `COPY (SELECT UserID AS x FROM read_parquet('hits.parquet') LIMIT 1000001) TO 'repro.parquet' (FORMAT PARQUET)` and then `SELECT SUM(x), SUM(x::HUGEINT) FROM read_parquet('repro.parquet')`, which differ by `6 * 2^64` in a 1.3 MB file of one column.

The evidence that we are right is DuckDB against itself. `SUM(x::HUGEINT)`, `SUM(x::DECIMAL(38,0))`, the same column summed after being read into a table, and the sum of its own per-`CounterID` group sums all give our answer and all disagree with its ungrouped parquet sum. It is deterministic across thread counts and row group layouts, so it is neither a race nor the parallel combine, and it only appears when the column arrives dictionary encoded, so it is about the vector the scan hands over rather than the values in it.

We do not match it because reproducing it means writing a broken carry into the aggregate on purpose, keeping it there until upstream fixes it, and remembering to take it out. A silently wrong sum is also the one kind of wrong answer a user cannot see, which is the opposite of what the end to end claim is for. Tracked in issue #332.
