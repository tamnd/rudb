# SQL surface and type system

This is the largest body of unglamorous work in the project and it is the one most likely to be underestimated. The performance work has a clear shape and a measurable end. The SQL surface does not: DuckDB ships hundreds of scalar functions, dozens of table functions, a type system with parametric nesting, and a set of syntax extensions that its users rely on heavily. Document 00 says the compatibility surface has no bottom, and this document is where that becomes concrete.

## 10.1 The type system

**Numeric.** `BOOLEAN`, `TINYINT`, `SMALLINT`, `INTEGER`, `BIGINT`, `HUGEINT` at 128 bits, and the unsigned counterparts through `UHUGEINT`. `FLOAT` and `DOUBLE`. `DECIMAL(p, s)` with p up to 38, physically stored as the narrowest integer that holds p digits, which is the standard representation and the one DuckDB uses.

**Temporal.** `DATE`, `TIME`, `TIMETZ`, `TIMESTAMP` at microsecond precision, `TIMESTAMP_S`, `TIMESTAMP_MS`, `TIMESTAMP_NS`, `TIMESTAMPTZ`, and `INTERVAL` as the months-days-micros triple. The triple representation matters for compatibility because interval arithmetic with months is not associative with days and DuckDB's specific behaviour is what tests assert on.

**String and binary.** `VARCHAR` with the 16-byte inline representation from document 7.1, `BLOB`, `BIT`, `UUID`, `VARINT`.

**Nested.** `LIST(T)`, `STRUCT(...)`, `MAP(K, V)`, `UNION(...)`, `ARRAY(T, n)` for fixed length, and `ENUM`.

**`VARIANT`.** DuckDB v2.0 promotes VARIANT to a first-class type with shredded execution, which means a variant column with a stable observed shape is physically stored as if it were typed and only the exceptions go in a generic representation. This is a substantial feature and it is a good one. It is scheduled at M7 and it is the type most likely to be incomplete at 1.0.

**Nested types are stored columnar all the way down.** A `LIST(STRUCT(a INT, b VARCHAR))` is stored as offsets plus two child column chunks, each independently encoded. Every encoding in document 06 applies to a child column. There is no row-oriented fallback for nested data, because that is where every engine that has one loses.

**Null is a per-value property at every nesting level**, which means a struct can be null, a struct field can be null independently, and a list can contain nulls and be null itself. Getting this exactly right is fiddly and it is tested exhaustively.

## 10.2 Casting and type resolution

The implicit cast lattice and the function overload resolution rules are a compatibility surface in their own right, and they are the kind of thing where a small difference produces a different answer rather than an error.

**The rule we follow is DuckDB's, exactly.** Its implicit cast graph, its integer promotion behaviour, its rules for resolving an overload when multiple candidates match, its `UNION` type resolution, its behaviour when a `CASE` has branches of different types. These are documented incompletely, which means the real specification is DuckDB's source and its test suite, and document 14's differential harness is how we discover the parts nobody wrote down.

**Overflow behaviour.** DuckDB errors on integer overflow rather than wrapping. `TRY_CAST` returns null instead of erroring. Both are matched.

**Decimal arithmetic result types.** The precision and scale of `a * b` and `a / b` follow specific rules, differ between systems, and are exactly the kind of thing that silently produces a different number. Matched to DuckDB.

## 10.3 Statements

`SELECT` with the full clause set including `QUALIFY`, `GROUP BY ALL`, `ORDER BY ALL`, `GROUPING SETS`, `ROLLUP`, `CUBE`, `WINDOW`, `LATERAL`, `TABLESAMPLE`, `USING SAMPLE`, and DuckDB's `SELECT * EXCLUDE (...)` and `SELECT * REPLACE (...)`.

`INSERT` with `ON CONFLICT`, `UPDATE`, `DELETE`, `MERGE`, `INSERT ... RETURNING`.

`CREATE`/`ALTER`/`DROP` for tables, views, schemas, sequences, indexes, types, macros and now triggers, which DuckDB v2.0 added. `CREATE TABLE AS`, `CREATE OR REPLACE`, `CREATE IF NOT EXISTS`.

`ATTACH` and `DETACH` for multi-database, including attaching a DuckDB file per document 12.1.

`COPY` to and from CSV, Parquet, JSON and Arrow IPC, with the full option set. `COPY` is one of the most heavily used surfaces in DuckDB and its CSV reader in particular has years of accumulated handling for malformed input, type sniffing, and dialect detection that users depend on. This is scheduled its own milestone slot for a reason.

`PRAGMA` and `SET` for configuration, `EXPLAIN`, `DESCRIBE`, `SUMMARIZE`.

`PREPARE`/`EXECUTE`, transactions with `BEGIN`/`COMMIT`/`ROLLBACK` and savepoints.

**Recursive CTEs**, including DuckDB's `USING KEY` variant.

**Table functions**: `read_csv`, `read_parquet`, `read_json`, `range`, `generate_series`, `glob`, `unnest`, `parquet_metadata`, `duckdb_tables` and the rest of the catalog functions, which tools query directly.

**Friendly SQL.** DuckDB's dialect extensions, which are a large part of why people use it: trailing commas, `FROM`-first syntax, list comprehensions, lambda functions in `list_transform` and friends, the `->>` JSON operators, string slicing with `[a:b]`, `COLUMNS(*)` expressions with regex, `PIVOT` and `UNPIVOT`, `ASOF` joins, positional joins, and now `NEAREST` joins in v2.0. These are not optional; they are a large fraction of what a DuckDB user's queries contain.

## 10.4 Functions

DuckDB ships on the order of a thousand scalar functions, a few hundred aggregates, and several dozen table functions. That count is the actual scope of this document.

**The strategy is coverage-driven and evidence-driven.** Extract the full function list from a DuckDB build via `duckdb_functions()`, rank by frequency in a corpus of real queries drawn from public repositories, the DuckDB test suite, TPC-H, TPC-DS, JOB and ClickBench, and implement in that order. Track coverage as a published percentage weighted by corpus frequency, not as a raw count, because a raw count makes implementing a hundred obscure date-part variants look like more progress than implementing `regexp_replace`.

**Every function is differentially tested against DuckDB** on generated inputs including edge cases: nulls, empty strings, extreme values, invalid UTF-8, leap seconds, timezone boundaries, and the boundary of every integer type. This is document 14.3 and it is where most of the compatibility bugs will be found.

**Aggregate functions need both a scalar and a windowed implementation**, plus combine and finalize for parallel execution, plus for many of them an inverse for sliding windows. That is four to five implementations per aggregate and it is why the aggregate count is a smaller number than the scalar count but a comparable amount of work.

## 10.5 Regular expressions

Called out separately because on ClickBench it is 24.7 percent of DuckDB's total time in a single query, and because getting it wrong is a compatibility problem as well as a performance one.

**Compatibility requires RE2 semantics**, because that is what DuckDB uses and its behaviour on capture groups, on alternation, on greediness, and on what it rejects is what queries depend on.

**Performance requires more than RE2 gives by default.** The plan is a compiled DFA with lazy construction, literal prefix and required-substring extraction used as a prefilter, and vectorized substring search for the prefilter. On the ClickBench query 28 shape, `REGEXP_REPLACE(Referer, '^https?://(?:www\.)?([^/]+)/.*$', '\1')`, the anchored literal prefix means most of the work is a prefix comparison and the full engine runs only on candidates. Combined with running against FSST-compressed bytes where the pattern permits, per document 6.7, this is where the query's time goes from 6.5 seconds to something acceptable.

**The Rust `regex` crate gives most of this** and is a mature, well-tested implementation with the right complexity guarantees. The gaps against RE2 semantics are small and known, and closing them is a bounded piece of work.

## 10.6 What is deliberately excluded

**No procedural language.** No PL/pgSQL, no stored procedure bodies beyond DuckDB's macro system. DuckDB does not have one either.

**No user-defined types beyond `ENUM` and `STRUCT` composition.**

**No security features.** No users, no roles, no `GRANT`, no row-level security. This is an embedded engine and the file's permissions are the security model, which is what DuckDB does.

**No full-text search in the core.** DuckDB's FTS is an extension and it stays one.

**Non-goal for 1.0: complete `VARIANT` shredding, complete trigger support, and the long tail of geospatial and vector-similarity functions**, all of which live in extensions and are covered by document 12.4's extension ABI rather than by the core.

## 10.7 How the surface gets measured

A single published number: **weighted function coverage**, defined as the fraction of function invocations in the reference corpus that we implement with verified matching behaviour. Plus a second number, **statement coverage**, defined the same way over syntactic constructs.

Both are computed by a tool in `rudb-compat`, both are published per commit, and neither is allowed to be estimated by hand. Document 14 specifies the tool.

**A function that is implemented but differs from DuckDB on any tested input counts as not implemented.** This is a deliberately harsh rule and it is the only one that makes the number mean anything.
