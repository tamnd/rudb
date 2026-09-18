# 2. The data

## 2.1 The scale factors, and that there are three of them

`src/suite.rs` already says why: SF10 fits in cache on a large machine and measures the engine, SF100 is the standard comparison point, SF1000 exceeds memory on most machines and measures spilling, which is where a lot of engines quietly fall over and where a harness that only ran SF100 would have said everything was fine.

Two are added to that list and both are small. **SF1** is the qualification scale, because the TPC-H specification's validation output is defined at SF1 and document 04 uses it as a third-party correctness check that does not come from DuckDB. **SF0.01** already exists in practice, `src/suite.rs`'s module comment says every engine was asked whether it accepts the query text against a real SF 0.01 corpus, and it becomes a named scale so that the correctness suite can run in seconds on every commit.

Row counts, which are fixed by the specification and are not measured:

| table | SF0.01 | SF1 | SF10 | SF100 | SF1000 |
| --- | --- | --- | --- | --- | --- |
| `lineitem` | ~60,000 | 6,001,215 | ~60,000,000 | ~600,000,000 | ~6,000,000,000 |
| `orders` | 15,000 | 1,500,000 | 15,000,000 | 150,000,000 | 1,500,000,000 |
| `partsupp` | 8,000 | 800,000 | 8,000,000 | 80,000,000 | 800,000,000 |
| `part` | 2,000 | 200,000 | 2,000,000 | 20,000,000 | 200,000,000 |
| `customer` | 1,500 | 150,000 | 1,500,000 | 15,000,000 | 150,000,000 |
| `supplier` | 100 | 10,000 | 100,000 | 1,000,000 | 10,000,000 |
| `nation` | 25 | 25 | 25 | 25 | 25 |
| `region` | 5 | 5 | 5 | 5 | 5 |

`lineitem` is between one and seven line items per order and so is not exactly a multiple; the exact count is whatever `dbgen` produced and is recorded per corpus rather than assumed. `nation` and `region` do not scale, which is the reason TPC-H's dimension filters are so selective and is worth remembering when reading any result from it.

## 2.2 Provenance

The generator is the official `dbgen` from the TPC-H tools distribution, built from source, with its version recorded in the corpus manifest. It is the source of truth for two reasons: it is the generator the specification defines, and it is the only one that supports the `-C` and `-S` chunked generation that SF1000 needs on a machine that cannot hold a terabyte of text.

DuckDB's `tpch` extension, invoked as `CALL dbgen(sf = n)`, is permitted up to SF10 and is recorded as a **different provenance** in the manifest rather than treated as equivalent. It is the same generator compiled into a different program, and it very probably produces identical data, but "very probably" is not a property a corpus should have when the number that comes out of it is a headline. Any result whose corpus was generated this way carries the provenance in its report, exactly as `src/data.rs` already carries the sampling note on a reduced ClickBench run.

Conversion from `dbgen`'s pipe-delimited `.tbl` files to Parquet is done by DuckDB, for the same reason `src/data.rs` already uses DuckDB to cut a smaller ClickBench file: the output is a Parquet file with nothing of DuckDB in it, every engine reads the same one, and the alternative is a Parquet writer this project would have to prove correct first. The conversion is recorded with the DuckDB version that did it.

## 2.3 The schema, written down once

`dbgen` emits text. Somebody has to decide what type each column is, and if that decision is made independently per engine the engines are not running the same benchmark.

The rule: the column types are the ones the TPC-H specification names, mapped to DuckDB's spelling, and every engine gets the Parquet file that mapping produced. Concretely, the identifiers are `INTEGER` and `BIGINT`, the money columns are `DECIMAL(15,2)`, the dates are `DATE`, and `CHAR(n)` and `VARCHAR(n)` are both `VARCHAR` because Parquet has no fixed-width string and pretending otherwise would have every engine pad differently.

`DECIMAL(15,2)` rather than `DOUBLE` is not a detail. TPC-H's arithmetic is money arithmetic, the specification's validation output is exact, and an engine that reads the money columns as `DOUBLE` is running an easier benchmark. rudb's `DECIMAL` handling has two open wrong-answer issues against it, #504 on `trunc` and #503 on float division, and TPC-H is where that class of bug becomes a wrong benchmark rather than a failing unit test.

No column is nullable in the generated data. The Parquet files are written with the columns marked non-null, because a nullable column an engine has to check per row is a cost the specification does not ask for.

## 2.4 The four properties that have to be recorded, not assumed

`../../graph/` depends on physical properties of this data. Every one of them is verified at corpus preparation time and written into the manifest, so that a later result cannot quietly depend on a property a regenerated corpus does not have.

**`lineitem` is in `l_orderkey` order.** `dbgen` emits it that way, which is what makes the `lineitem → orders` forward link monotone and 106 MB instead of 2.1 GB, per `../../graph/03-the-file-format.md` section 3.4. The check is one pass asserting non-decreasing.

**`o_orderkey` is sparse.** The specification requires order keys to be sparsely populated across their range specifically so that implementations cannot assume density, and about a quarter of the key space is used. This is the single most useful fact in this document for the graph layer: it means `orders` does **not** get an identity key map, it gets the dense-offset form, and the threshold in `../../graph/02-the-data-model.md` section 2.2 was set at one in eight precisely so that a quarter-dense key space lands on the bitmap form rather than the sorted one. Verify and record the observed density.

**`c_custkey`, `p_partkey`, `s_suppkey` are dense and ascending.** These get identity key maps and cost twenty four bytes each. Verify.

**`partsupp` is in `ps_partkey` order** and has exactly four suppliers per part, which makes `partsupp → part` monotone with a constant degree and is the second free link.

## 2.5 The forms each corpus exists in

Four, and a run names which one it used.

**Parquet**, which is the neutral form every engine can read and the one a comparison is made on when the point is the engine rather than the format.

**The engine's own loaded form**, meaning DuckDB's database file, ClickHouse's `MergeTree`, and rudb's native file. This is what the official ClickBench protocol does and it is what a user would actually have, and `../../storage-v3/03-benchmark-contract.md` already requires the four-way comparison, DuckDB Parquet, rudb Parquet, DuckDB native, rudb native, for exactly this reason. TPC-H inherits it.

**The rudb native form with graph sections**, which is the configuration `../../graph/09-measurement.md` section 9.8 requires be named rather than described.

**The rudb native form without them**, which is the control for the differential test and is also the product for a user whose data has no declared keys.

Load time, load CPU, peak load RSS and on-disk bytes are reported for each, per `../../15-rudb-bench.md`. For TPC-H specifically the on-disk number is more interesting than it is for ClickBench, because eight tables with foreign keys is where a format's per-table overhead and its index space show up, and because the ten percent budget in `../../graph/03-the-file-format.md` section 3.7 is a claim this measurement checks.

## 2.6 No sampling, and the one exception

`src/data.rs` already refuses `--rows` for a multi-table suite and its reasoning is correct and should not be weakened. Taking a fraction of each table independently keeps almost none of the rows that join.

The exception that is not an exception: a *smaller scale factor* is a different corpus generated by `dbgen -s`, with referential integrity intact, and it is the supported way to run a smaller TPC-H. The harness should say that in the refusal message, which it nearly does already, and the `--scale` flag of document 07 section 7.2 is what it should point at.

A consistent subset, taking a fraction of `orders` and then the `lineitem` rows that belong to them, is technically possible and is not supported, because the result is not TPC-H at any scale factor and a number from it would need a paragraph of explanation attached forever.

## 2.7 What this costs to hold

SF100 as `.tbl` text is on the order of a hundred gigabytes; as Parquet with the default codec it is substantially less and the exact figure is recorded rather than guessed. SF1000 is ten times that and is why `-C` and `-S` chunking is required rather than optional. The corpora live under `$RUDB_BENCH_DATA` and nothing downloads or generates them implicitly, per `src/data.rs`'s existing rule that a harness which fetched seventy gigabytes because somebody typed a suite name is a harness people run once.

Generation is an explicit subcommand, `rudb-bench generate tpch --scale 100`, which prints what it is about to write and how much space it needs before it writes anything.
