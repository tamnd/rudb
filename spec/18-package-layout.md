# Package layout

Document 00 says modular via crates so that modern research can be absorbed. That phrase is easy to write and it only means something if the boundaries are drawn where the research actually arrives. This document draws them, states the dependency rules, and sets the stability tiers.

The failure mode being avoided is a crate tree that looks modular and is not: forty crates that all depend on each other, where changing an encoding means touching nine of them. The test of a boundary is whether a new idea from a paper can be implemented behind it without changing anything above it.

## 18.1 The tree

**Foundation.**

`rudb-common`, the types, values, error types, arena allocator, hashing, and the small utilities everything needs. Depends on nothing in the workspace. Kept small on purpose, because a common crate is where a codebase's cyclic dependencies go to hide.

`rudb-io`, files, direct I/O, io_uring, the thread pool backend, object storage, and the interception shim from document 16.5. Depends on `rudb-common`.

**Data representation.**

`rudb-vector`, vectors, physical forms, validity, selection vectors, string representation, and the type-specific buffers. This is the widest interface in the system per document 7.1 and it is the crate with the strongest stability requirement inside the workspace.

`rudb-encoding`, every encoding from document 06: the kernels, the cascade machinery, the cost model, the multi-column detection. **This is the most important boundary in the tree.** A new encoding from a paper is a new module here plus a row in the candidate table, and nothing above this crate changes. If adding ALP's successor requires editing the execution engine, this boundary was drawn wrong.

`rudb-kernels`, the generated cross product of operator, physical form and type from document 7.3, plus the SIMD dispatch. Generated at build time from a table. Depends on `rudb-vector` and `rudb-encoding`.

**Storage.**

`rudb-storage`, blocks, row groups, column chunks, the buffer manager, statistics, the free list, the header. Depends on `rudb-io`, `rudb-vector`, `rudb-encoding`.

`rudb-txn`, MVCC, the WAL, checkpointing, versions.

`rudb-catalog`, schemas, tables, views, sequences, constraints, dependency tracking, and the global dictionary and symbol table objects.

**Front end.**

`rudb-parse`, lexer, parser, AST, textual round trip. Depends only on `rudb-common`, which means it can be used standalone as a SQL parser and that is a genuinely useful artifact in its own right.

`rudb-bind`, name resolution, type resolution, overload resolution, subquery binding, and the logical plan.

`rudb-functions`, the scalar and aggregate function library from document 10.4. **This is the second most important boundary**, because it is the biggest and the most incrementally growable part of the system. A function is a registration plus an implementation and touches nothing else.

**Optimization and planning.**

`rudb-plan`, the logical and physical plan representations, their textual forms, and Substrait conversion.

`rudb-opt`, the rewrite passes, cardinality estimation, join ordering, RPT, layout adaptation. Each pass is a module with a uniform interface, which is what makes document 9.1's per-pass disabling and document 14.2's automatic bisection possible.

**Execution.**

`rudb-exec`, operators, morsels, the scheduler, hash tables, sorting, spilling.

`rudb-ir`, the expression IR from document 8.5.

`rudb-jit`, tiers 1 through 3: fusion, Cranelift lowering, the code cache. **Behind a feature flag that defaults on and that can be turned off**, so that the whole engine builds and passes its tests with no JIT at all. That is not a hypothetical configuration; it is what runs under Miri and what runs on a platform Cranelift does not target.

**Formats.**

`rudb-parquet`, `rudb-arrow`, `rudb-csv`, `rudb-json`, `rudb-iceberg`, `rudb-delta`. Each independent, each depending only on `rudb-vector` and the foundation crates. None of them depends on the execution engine, which means each can be used standalone and each can be tested standalone.

`rudb-duckdb-format`, reading and writing DuckDB's storage format per document 12.1. Isolated deliberately, because it is the crate most likely to need a version-specific fork and because it is the one whose churn is driven entirely by someone else's release schedule.

**Top level.**

`rudb`, the embedding API, connections, prepared statements, configuration, and the Rust-native interface.

`rudb-c-api`, the `libduckdb`-compatible C surface, plus a native C surface.

`rudb-cli`, the shell.

`rudb-extension`, the host side of DuckDB's extension ABI.

**Tools.**

`xtask`, the build and benchmark and codegen driver, in the workspace, not a shell script.

## 18.2 Dependency rules

**Dependencies point downward only, and it is checked in CI by a graph tool, not by convention.** A cycle is a build failure.

**No crate depends on `rudb-exec` except `rudb` and the tools.** In particular no format crate does, no encoding crate does, and no storage crate does. This is what keeps the format readers usable standalone and what keeps the encoding layer from acquiring an execution dependency by accident, which is the specific way this tree would rot.

**`rudb-vector` is depended on by almost everything and depends on almost nothing.** Its interface is specified before the operators are written and changed only by RFC, per document 7.1.

**Feature flags are for optional functionality, never for correctness variants.** No feature flag changes a query's answer. A flag that turns off the JIT changes how the answer is computed and not what it is, and document 16.3 proves that.

**Compile time is a tracked metric with a budget.** Ten minutes for a clean release build of the whole workspace, measured in CI, and a change that pushes past it is a change that needs to justify itself. The kernel generator in document 7.3 is the main risk here and its table is the throttle.

## 18.3 Stability tiers

Three tiers, because "we might change this" and "we will never change this" are both useless to a user without a boundary between them.

**Tier 1, stable and semantically versioned.** The `rudb` crate's public API, the C API, the SQL surface, the storage format, and the four compatibility levels. Breaking changes require a major version and a migration path. The storage format additionally requires that an old file always opens in a new version, forever.

**Tier 2, public but versioned with the workspace.** `rudb-parse`, `rudb-plan`, `rudb-arrow`, `rudb-parquet`. Useful standalone, published to crates.io, may break at a minor version with a changelog entry. These are the crates someone builds a different tool on and they are worth publishing for that reason.

**Tier 3, internal.** Everything else. Published to crates.io so that the workspace builds from a registry, documented as internal, no stability promise at all. Anyone depending on `rudb-exec`'s internals has been told what they are doing.

## 18.4 How research gets absorbed

The claim in document 00 is that the crate structure lets modern work be adopted without a rewrite. Concretely, for the kinds of work document 01 surveys.

**A new encoding.** A module in `rudb-encoding`, a row in the candidate table, kernels generated. Nothing above changes. This is the case the tree is most optimized for because it is the case that recurs most often.

**A new operator algorithm**, a different hash table or a different join strategy. A module in `rudb-exec` behind the existing operator interface, plus a runtime switch. Nothing else changes.

**A new optimizer rewrite.** A pass module in `rudb-opt` with the uniform interface, automatically getting per-pass disabling and automatic bisection.

**A new execution model**, which is the hardest case and the one where a clean boundary is worth the most. A GPU backend, a different compilation strategy, a different vectorization width: this consumes a physical plan and produces results, which is why document 13.6 makes the physical plan serializable to Substrait. Sirius is the existence proof that this shape works.

**A new file format.** A crate alongside `rudb-parquet`, depending only on `rudb-vector`.

**What the tree does not make easy**, and it is worth being honest about it: changing the vector size, changing the physical form set, or changing the string representation. Those are in `rudb-vector` and they touch everything. That is the price of having a wide fast interface at the bottom, it is the right price, and it is exactly why document 00 settles the vector size at 1024 up front rather than leaving it to be discovered.
