# Where the queries come from

The 4096 upstream files test what DuckDB's own developers thought to test. That is a large and useful set and it is not a sample of anything. Every generator below exists to make the phrase "what was tried" in section 9.10 much larger than a person can write.

## 10.1 The generator that fixes the arithmetic in the README

The README's opening measurement is that two function names shipped in four weeks, each taking a day of measurement against the pinned binary, and that at that rate the function table is twenty years. That is not a statement about how hard functions are. It is a statement about a manual process.

The pinned binary answers `SELECT * FROM duckdb_functions()` with 3245 rows carrying the name, the parameter types, the return type and whether it varies. That is a machine readable specification of the entire scalar surface. For each row, generate calls: one value per parameter from a per type boundary set, then the cross product capped, then nulls in every position, then the wrong types to capture the error. Run both engines. Record agreement per overload.

That is one program. It answers, in one run, the question a day of hand measurement answers for one function, and it answers it for all 3245 rows including the ones nobody would think to check. The boundary sets are the only judgement in it: for `INTEGER` the minimum, the maximum, zero, one, minus one; for `VARCHAR` the empty string, one containing a null byte, one that is not valid UTF-8, a long one, one with combining characters; for `DOUBLE` zero, negative zero, infinity, negative infinity, NaN, the subnormal boundary; for `DECIMAL` the width and scale extremes; for `TIMESTAMP` the epoch, the infinities DuckDB supports, and the leap day.

`spec/14-rudb-compat.md` section 14.4 already describes this. It has not been built. It is the single highest value item in this folder and document 12 puts it in the first batch.

It also produces the thing section 1.2 needs, which is a function level pass or fail that can go down. A function counts as implemented while its row in this run is clean, and the day a boundary value disagrees it stops counting, automatically, without anybody deciding.

## 10.2 Generating statements from a grammar we already have

Most projects that generate SQL write the generator by hand. SQLsmith reads the target's catalog and builds a random query from a hand written model of SQL's shape, which is why it is good and why it took years to cover what it covers.

We have the grammar as data: 1088 rules in a flat table compiled from the vendored PEG files, and section 3.2 pointed out that the matcher does not know what any of them mean. A generator walks the same table in the other direction, choosing an alternative at each choice point and emitting tokens.

The catch is well known and worth stating so nobody is surprised: uniform generation from a grammar produces syntactically valid nonsense, deep recursion and identifiers that refer to nothing. The fixes are all mechanical. Weight the alternatives, bound the recursion depth, and hand the generator a catalog so that wherever the grammar wants an identifier it gets a real table or column name of a usable type. What comes out is still not a realistic query, and it does not need to be: its job is to reach grammar corners the corpus does not, and a parser difference does not care whether the query is sensible.

This generator finds different things from the one in 10.1. It finds statements we refuse and DuckDB accepts, which is the `Syntax` reason in the harness and is the level two statement number in section 1.1.

## 10.3 Fuzzing our own code, which is a different job

The two generators above look for differences. A fuzzer looks for crashes, panics, assertion failures, hangs and memory growth, and it is a separate activity with separate tooling and a separate success criterion.

`cargo-fuzz` over libFuzzer is the default, with `afl.rs` as the second engine since the two find different things. The target that matters most is not a byte string, it is a structured one: `arbitrary` with a derive on the AST node types gives a fuzzer that produces well formed trees, which spends its budget on the binder and the optimizer rather than on the tokenizer. The same approach is documented for SQL by deriving the generator from a parser's AST types, and our AST is arena based with `u32` indices and no boxes, which is unusually friendly to that.

Three targets, in order: tokenize plus match on raw bytes, transform plus bind on a generated AST, and plan print plus plan parse round trip on a generated plan, which is a property test rather than a crash hunt and is nearly free because the round trip already exists as a test.

The failure condition is any panic that is not a deliberate not implemented path, any hang past the harness deadline, and any memory growth past the two gigabyte limit the harness already sets.

Upstream's own generator is available too. DuckDB ships `CALL sqlsmith()` and runs a public fuzzer repository against itself, so the same generated statements can be pointed at both engines with no generator work on our side at all. That is the cheapest of the four and it should be running first even though it is listed third.

## 10.4 Metamorphic testing, and what it is actually for here

Four published oracles find wrong answers in a database with no reference to compare against, by transforming a query into another query whose answer must be related in a known way.

PQS, pivoted query synthesis, OSDI 2020: pick a row, construct a query that must return it, check that it does. NoREC, non optimizing reference engine construction, ESEC/FSE 2020: rewrite a query so the optimizer cannot use an index or a pushdown, and compare against the optimized form. TLP, ternary logic partitioning, OOPSLA 2020: split a query on predicate `p` into the three queries for `p`, `NOT p` and `p IS NULL`, and require the union to equal the original. CERT, ICSE 2024, does the same trick for cardinality estimates. All four are from the SQLancer line of work and all four found real bugs in shipping databases.

The obvious objection is that we have an oracle, since DuckDB is right there, so why generate a second one. Two reasons, and they are good ones.

TLP is a direct test of three valued logic, which is where a young engine is wrong and where a difference against DuckDB tells you that something is wrong without telling you what. A TLP failure localises to the predicate. It is also runnable on our engine alone, in CI, with no DuckDB binary present, which makes it the right oracle for a fast pre merge check where the full differential is too slow.

NoREC is a direct test of the optimizer against itself and pairs exactly with the pass bisector in section 9.6. If disabling a pass changes an answer, that is the same signal NoREC is built to produce, and the two together mean an optimizer bug names its own pass.

## 10.4.1 What TLP came out as, and what the first run said

It is `rudb-compat tlp`, it generates a predicate over a table fixed in the source, and it runs the query four times: with no predicate, with the predicate, with its negation, and with the case where the predicate is neither true nor false because something in it was NULL. Two decisions in it are worth keeping written down because both went against the paper.

The three parts are run as three queries and added up in the harness rather than written as one union the way the paper does it. It is the same property and it fails in fewer places: a bug in `UNION ALL` would break every case in a run and say nothing about any predicate, which is the one outcome an oracle exists to avoid. The single statement form is printed beside every failure, so nothing is lost for the person who has to reproduce it.

The table is a constant rather than something generated from the seed. A run then replays from the seed alone, and the rows get chosen for the job instead of sampled. What the job needs is rows on both sides of every boundary a generated predicate might draw, and one row per column that is NULL in that column and ordinary in every other, because that is the row a predicate on one column is unknown about while the rest of the expression around it is well defined.

The first run is twenty thousand predicates against rudb with nothing found and nothing refused, and sixteen thousand of those divided the table rather than putting every row in one part. That second number is printed for a reason. A predicate that puts every row in one part passed without testing anything, so a generator that drifted into writing those would produce a run that is green and empty, and the two numbers beside each other is what makes that visible.

Nothing found is a smaller claim than it looks and the page should say so. It bounds the predicate surface the generator reaches, which is comparisons, IS NULL, BETWEEN, IN, IS DISTINCT FROM, LIKE, a bare boolean column, AND, OR, NOT, a nested IS NULL, and terms wrapped in the functions that return NULL for a NULL argument. That is a narrower surface than sqlsmith reaches and deliberately so, since the oracle is about three valued logic rather than grammar coverage, and it says nothing about the features the milestone is still missing.

Zero refusals means every form the generator writes is one rudb implements. The same predicates were put to the pinned binary, which holds on all of them and refuses none, and that is the check an oracle with one engine in it cannot do without: a generator writing trivially partitionable SQL would produce exactly the same green run.

rudb passing everything leaves the failure path with no run to exercise it, so it is tested against an engine written for the purpose that answers every partition with the rows where the predicate is true, which is what an engine that treats unknown as false does. An oracle whose failure path never runs is a test of its own generator.

What is left of this box is the two oracles the paper has beside the WHERE one, the aggregate form and the GROUP BY with HAVING form. They come after NoREC rather than before it, because NoREC uses the same generator and pairs with the pass bisector that already exists.

## 10.5 The loop

A generator without reduction produces a pile. The loop is: generate, run both engines, compare, and on a difference immediately reduce with the tree aware reducer from section 9.5, hash the reduced case to deduplicate, and file it if it is new. Nothing in that loop needs a person, and the output of it is a few dozen distinct minimal cases a week rather than a queue nobody opens.

Coverage closes it. `cargo-llvm-cov` over the run says which parts of the binder and the executor the generators are not reaching, and that is the feedback that stops a generator from producing a million variations of the same three code paths. Coverage is a diagnostic for the generators, not a published number, because line coverage of an engine says very little about compatibility.

## 10.6 Where deterministic simulation fits, which is not here

FoundationDB's simulator, TigerBeetle's VOPR and Antithesis are the state of the art for finding bugs in distributed and storage systems, by running the whole system deterministically against injected faults and replaying any failure exactly.

That is the right tool for storage, recovery and concurrency, and those are other documents. It is the wrong tool for SQL surface compatibility, where the interesting input is the query rather than the schedule. The part that carries over is the discipline from section 9.8: every generated run records a seed, and any finding can be replayed exactly from it. A generator whose failures cannot be replayed is a generator whose failures do not get fixed.

## 10.7 What enough looks like, honestly

SQLite is the only database that can make a strong claim here, and the claim is specific: its TH3 suite reaches full branch and MC/DC coverage of the library, and it has on the order of 590 times as much test code as library code. It also took decades, the suite is proprietary, and the claim is about correctness rather than about matching another engine.

No project publishes a defensible single number for compatibility with another engine, and section 11 is about why and about what to publish instead. So "enough" here is not a coverage target, it is a process property: every generator runs continuously, every finding is reduced automatically, and the published numbers move because of what the generators found rather than because of what somebody chose to work on.
