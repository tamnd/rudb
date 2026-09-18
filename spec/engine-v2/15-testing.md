# Testing

A registry of swappable implementations multiplies the configuration space. The test strategy has to collapse it back down, and it does so with one idea.

## 1. The oracle

For every seam, one implementation is the reference. It is chosen for being obviously correct: the nested loop join, the `HashMap` aggregate, the comparator sort, the decode-then-compare kernel, the tree-walking expression evaluator. It is never deleted.

The oracle test is one loop:

``` for query in corpus:
    expected = run(query, Policy::Reference)
    for config in configurations:
        assert run(query, config) == expected
```

Everything else in this document is a detail of that loop, what the corpus is, what `==` means, and which configurations get run when.

The property that makes this worth building: **adding an implementation of a paper adds no tests.** A researcher who writes a new hash table and registers it gets the entire corpus run against it, differentially, on the next `cargo test`. That is [`04-modularity.md`](04-modularity.md) section 7 step 3, and it is the single largest reason the modularity design is worth its cost.

## 2. What equality means

Three relaxations, each declared rather than assumed.

**Row order** is unspecified without `ORDER BY`, so comparison sorts both sides unless the query has one. With one, order is compared exactly, including stability.

**Floats** are compared exactly when every strategy in the configuration declares `Determinism::Exact`, and within a relative tolerance otherwise. `Strategy::deterministic()` returning `PerThreadCount` is what triggers the relaxed comparison, and `agg.parallel=radix-partitioned` is the strategy that does. Declaring it is what keeps the comparison strict everywhere else, which matters because a blanket float tolerance hides real bugs.

**Nothing else.** Not null placement, not decimal scale, not string collation, not type inference. Those are compatibility surface and they have one right answer, which is DuckDB's.

## 3. The corpus

Four sources, and the fourth is the one that finds the bugs.

**The suites**: ClickBench's 43, TPC-H's 22, TPC-DS, JOB, CEB, H2O. These are what `rudb-bench` already knows about.

**The compatibility corpus**: `rudb-compat`, which is where DuckDB's own behaviour is captured as query-and-answer pairs. This is the largest and it is the definition of what 100% compatible means.

**Regression queries**: every bug ever fixed, as a query. `crates/rudb/src/tests.rs` is 2,109 lines and is where these live.

**Generated queries**: a grammar-driven generator producing queries over generated schemas, run differentially against DuckDB itself rather than against our own reference. This is the only test that finds bugs nobody thought of, and it is how an engine claiming compatibility discovers where it is not.

## 4. Which configurations, and when

The configuration space is the product of twenty-seven seams and is not enumerable. Three tiers.

**Every commit, on `laptop`:** the reference configuration, the default configuration, and a rotating sample of eight random valid configurations seeded by the commit hash. Over a few hundred commits the sample covers the space, and the seed makes any failure reproducible. Plus the smoke suite, for the measurement path.

**Nightly, on `server2`:** every single-seam variation from default, that is, for each seam, default with only that seam changed, for each implementation. Twenty-seven seams at three or four implementations each is about eighty configurations, times the corpus. This is the tier that catches "this implementation is correct alone and wrong in combination", in its one-at-a-time form.

**Weekly, on `server3`:** pairwise coverage over the seams, which is a few hundred configurations and catches interactions. Plus the full ClickBench and TPC-H SF100 runs for the ledger.

A researcher's own implementation gets tier one automatically and can be promoted with a flag.

## 5. Larger than memory, deterministically

The problem: testing spilling needs data larger than memory, and a test that needs a hundred gigabytes runs nowhere.

The solution is already in the tree. `rudb-io/src/sim.rs` is 945 lines of deterministic simulator. It can report a small device, inject latency, fail a write, and reorder completions. Combined with a low `memory_limit`, a query over ten megabytes of data spills exactly as a query over a hundred gigabytes does, with the same code paths, in milliseconds.

The tests that matter here are the recursive ones. A hash aggregate partition that does not fit even after partitioning re-partitions on the next bits down; the test builds a column whose hash collides pathologically and asserts the depth bound is hit and the bailout is taken and the answer is right. A join partition that does not fit alone falls back to a nested loop; same shape of test. These paths are the ones that never run in a benchmark and always run in production.

`spill.policy=fail` is the control: the same query at the same limit must either succeed with spilling or fail without it, and a query that succeeds under `fail` is a query the spill test did not actually exercise.

## 6. Property tests per seam

About three hundred lines total, generic over the registry, so they cover implementations that do not exist yet.

`hash.table`: insert a multiset, probe it, get each match exactly once. Grow past every resize boundary. Collide everything into one bucket.

`join`: every one of the eight join kinds against the nested loop, on inputs with nulls, duplicates on both sides, empty sides, and one side entirely null. Chunk boundaries placed adversarially inside a multi-match run, which is the resumable-probe bug from [`11-operators.md`](11-operators.md) section 5.

`sort`: sorted output, stability, all types, all null orderings.

`kernel.*`: encoded path equals decoded path, for every form, every operator, every type, including the boundary values of the bit-packing range and the sentinel cases of the dictionary.

`agg.state`: `init`, `update`, `combine`, `serialize`, `deserialize`, `finalize` in every legal order, with the associativity and commutativity that parallel aggregation assumes asserted explicitly.

`buffer`: allocate, pin, release, shrink, under a limit, with random interleaving, asserting the limit is never exceeded and nothing pinned is ever taken.

## 7. Fuzzing

Three targets, all differential.

SQL text into the parser and binder, asserting no panic and no unwrap. `rudb-parse` is 16,736 lines and is the largest attack surface.

Generated queries over generated data, against DuckDB. The oracle is the other engine.

Bytes into the Parquet reader and the native format reader, asserting no panic, no out-of-bounds, and no allocation proportional to a value read from the file. A format reader is the one part of an analytical engine that reads hostile input.

## 8. What is not tested this way

Performance. A test suite that asserts a timing is a test suite that fails on a loaded machine, and the project already has the right mechanism for performance: `rudb-bench`, distributions rather than points, and a regression gate that fires only when two distributions do not overlap.

The one exception is complexity, which is testable without timing. A top-k operator's memory must be O(k) and not O(distinct); the test asserts on the buffer manager's high-water mark, not on the clock. Those assertions are worth having because they catch the regression where an operator quietly stops using the fast path, which a timing test on a laptop would never notice.
