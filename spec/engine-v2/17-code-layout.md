# Code layout

Twenty-nine crates exist. This is what v2 needs them to become, and the lints that keep them that way.

## 1. What is there

```
16736  rudb-parse      SQL text to AST. DuckDB's grammar.
 9658  rudb-kernels    Scalar, aggregate, compare, cast.
 6714  rudb-exec       Operators, expressions, keys, spill, sort, topn.
 6386  rudb            Database, catalog wiring, 2109 lines of tests.
 5867  rudb-parquet    Thrift, metadata, pages, values, hybrid encodings.
 4606  rudb-opt        Filter, columns, fold, limit, nulls, topn, transitive.
 4587  rudb-encoding   The M1 encoder laboratory, including sketch.rs.
 4277  rudb-bind       AST to logical plan, name and type resolution.
 4192  rudb-plan       The logical plan.
 3862  rudb-vector     Vector, chunk, validity, selection, string, layout.
 3549  rudb-io         File and Filesystem traits, pool, 945-line simulator.
 2877  rudb-common     Types, errors, result.
 2723  rudb-cli        Shell, args, formatting.
 2627  rudb-compress   Snappy and friends.
 2483  rudb-regex      Parser and matcher.
 2044  rudb-functions  Signatures and overload resolution.
 1490  rudb-csv
 1095  rudb-catalog
  939  rudb-arrow
  254  rudb-storage    MemoryTable, and nothing else.
    9  rudb-txn, rudb-json, rudb-jit, rudb-ir, rudb-iceberg,
       rudb-duckdb-format, rudb-delta, rudb-c-api, rudb-extension
```

Zero external dependencies, edition 2024, Rust 1.85, everything pinned at one version, workspace lints on.

## 2. What changes

**`rudb-vector` becomes the data model.** `Vector` becomes `Column`, `Body` becomes `Form`, and the encoded forms from [`05-data-model.md`](05-data-model.md) land here. It gains `FormSet` and the `FormAware` trait. It does not gain any kernel.

**`rudb-exec` splits.** It is 6,714 lines holding operators, expressions, key encoding, spilling, sort and top-N, and under [`04-modularity.md`](04-modularity.md) each of those is a different seam owner. It becomes:

``` rudb-seam        Strategy, Registry, Provenance, Policy, Context.
                 Depends on rudb-common only. Every seam trait that
                 is not tied to one crate lives here.
rudb-buffer      BufferManager, Budget, MemoryOwner, eviction strategies. rudb-pipeline    Source, Stream, Sink, Progress, Blocked, Chunk plumbing,
                 the serial driver, the instrumentation shim.
rudb-sched       The parallel scheduler, morsel queue, Exchange, transports. rudb-expr        Program, ExprCompiler and its strategies. rudb-hash        Key encoding, hash functions, every hash table. rudb-agg         AggregateFunction, the three grouping shapes, parallel merge. rudb-join        Every join, including the nested-loop reference. rudb-sort        Normalised keys, radix, merge, top-k, heavy hitters. rudb-window rudb-exec        What is left: the operator catalogue that wires the above
                 into pipelines. Should end up small.
```

**`rudb-storage` becomes the storage engine.** 254 lines of `MemoryTable` becomes the block-page-tile format from [`06-storage.md`](06-storage.md), the write path, the block directory, and statistics. `MemoryTable` survives as a reference table implementation for tests.

**`rudb-encoding` becomes a library rather than a laboratory.** Its chooser becomes `storage.encoder` strategies; its `sketch.rs` becomes the statistics source [`12-optimizer.md`](12-optimizer.md) depends on. This is the crate with the most existing value and the least existing integration.

**`rudb-metrics` is new.** The document schema from [`14-metrics.md`](14-metrics.md), its serialisation, the `EXPLAIN ANALYZE` renderer, and the compatibility corpus of old documents.

**`rudb-jit` stays nine lines** until somebody registers an `ExprCompiler` in it.

## 3. The dependency rules

Enforced by `xtask lint deps`, which fails the build.

**Seam traits live in the lowest crate that needs them.** `CompareKernel` in `rudb-kernels`, `HashTable` in `rudb-hash`, `MemoryOwner` in `rudb-buffer`. `rudb-seam` holds only the machinery.

**The planner depends on registries, never on implementations.** `rudb-opt` may ask `rudb-hash`'s registry which tables are applicable; it may not name `unchained`.

**No implementation of a seam depends on another implementation of the same seam.** Shared helpers move up one level.

**An implementation crate exports no plan types.** If `rudb-hash` needs the planner to know something, it says it through `Context` and `applicable()`. This is the rule that stops the seam mechanism from growing into a parallel architecture.

**Nothing below `rudb-pipeline` knows about threads.** `rudb-kernels`, `rudb-vector`, `rudb-hash`, `rudb-sort` are all single-threaded code that happens to be `Send`.

**Everything that holds query state depends on `rudb-buffer`.** That is how the no-allocation-outside-`Budget` rule is checkable.

## 4. The lints

Six, all in `xtask`, all failing the build. Each exists because a specific mistake in this design is invisible without it.

`xtask lint rowloop`, no `Scalar` or `Value` constructed inside a loop over a column length. This is v1's finding, the one it called the single most valuable thing found in six months, and it is the cheapest lint in the set.

`xtask lint seams`, no `#[seam]` trait method takes only scalar parameters, and no `#[seam]` trait object is invoked from a function containing a row-count loop. [`04-modularity.md`](04-modularity.md) section 3.

`xtask lint memory`, no unbounded `Vec`, `HashMap`, `Box` or `String` field on a type implementing `Sink` or used as `Local`, unless annotated `#[bounded(N)]`. [`07-memory.md`](07-memory.md) section 3.

`xtask lint pointerfree`, no reference or raw pointer in any type reachable from a spillable page. [`07-memory.md`](07-memory.md) section 6, and it is what makes F11 cheap.

`xtask lint deps`, section 3 above.

`xtask lint flatten`, every `Column::flatten` call site is registered in a list with a reason, so that the list is reviewable and a new one requires a deliberate line. v1 had this for its one legitimate caller and it is worth keeping as the set grows.

## 5. What stays out

No external dependencies. This is an existing project rule and every part of this design respects it: no `criterion`, so the microbenchmark driver is hand-written on `rudb-bench`'s `measure.rs`; no `io_uring` or `xNVMe`, so the I/O pool is standard-library threads; no `arrow-rs`, which the data model in [`05-data-model.md`](05-data-model.md) would not fit anyway.

The cost is real and worth naming: the project cannot borrow anybody's kernels, anybody's Parquet reader, or anybody's SIMD library. The benefit is that every number in the ledger is attributable to code in this repository, which for a project whose entire claim is a performance claim is worth more than it costs.
