# Ecosystem: files, formats, lakehouse, clients

A database that only reads its own format is a database nobody adopts. DuckDB's success is at least as much about being the easiest way to query a Parquet file as it is about its engine. This document covers the formats we read and write, the lakehouse surface, and the language bindings.

## 13.1 Parquet

**Read and write natively, in the core, not as an extension.**

Reading Parquet fast is a distinct engineering problem from reading our own format and it does not get to be an afterthought. Document 03.2's board shows what it costs: Polars, DataFusion and Velox all query the same 14.78 GB Parquet file and post 45.35, 45.57 and 81.51 seconds against DuckDB's 26.25 on its native format. Roughly 1.7x is the price of Parquet on this workload, and a good chunk of that price is avoidable with a good reader.

**What a good reader does.** Predicate pushdown to row group and page statistics. Page-level skipping using the offset index, so a selective filter reads pages rather than column chunks. Late materialization, so that filter columns are read and evaluated before projection columns are touched. Parallel decode across row groups and across columns. Dictionary pages exploited directly as a dictionary-encoded vector rather than decoded, which is exactly the mechanism in document 6.7 applied to someone else's format and is the largest single win available in a Parquet reader. Bloom filters where present.

**Writing produces standards-compliant files with good statistics**, including the offset index and column-level bloom filters, because a file we write should be fast for anyone else to read.

**ALP and FSST are being standardized into Parquet in 2026**, which is directly relevant: the encodings document 06 builds on are converging with the interchange format, so a Parquet file written in a few years will carry the same encodings our native format uses and the reader can hand them to operators encoded.

**Caching.** A remote Parquet file that is queried repeatedly is cached locally, with metadata cached separately and more aggressively since the footer is small and is read on every query.

## 13.2 Arrow

**Zero-copy in both directions where the layouts permit, and they mostly do.**

Our flat vectors, validity bitmaps, string views and nested layouts are chosen to match Arrow's so that handing a result to Arrow is a pointer transfer plus a schema conversion. `StringView` in particular is the same 16-byte structure as our string representation, which is not a coincidence.

**The Arrow C Data Interface and the C Stream Interface** are implemented, which is how Arrow-consuming tools attach without a dependency on our library.

**Encoded vectors are the exception.** Arrow has no representation for an FSST-compressed or bit-packed run, so those decode at the boundary. Arrow's dictionary array does map onto our dictionary vectors, so the most valuable case is zero-copy.

**ADBC** is implemented as the standard database connectivity interface, since it is the direction the Arrow ecosystem is going and it is a small amount of work on top of the C API.

## 13.3 CSV and JSON

**CSV matters more than its technical interest suggests.** It is how data actually arrives. DuckDB's CSV reader has accumulated years of handling for dialect sniffing, type inference, malformed rows, mixed quoting, BOM handling, multi-file globbing and error recovery with `ignore_errors` and `rejects_table`. Matching that behaviour is a compatibility requirement and matching its speed is a separate one, since a parallel CSV parser that handles quoted newlines correctly is a genuinely tricky piece of code.

**JSON** with the same structure: reading newline-delimited and array-of-object files, inferring a schema, and the full set of JSON scalar functions and operators including `->` and `->>`. DuckDB's JSON type is a logical alias for `VARCHAR` with functions over it, and its `VARIANT` type in v2.0 is the better representation going forward, so the JSON reader targets `VARIANT` where the shape is stable.

## 13.4 Lakehouse

**Iceberg read at v2 and v3.** Manifest and manifest list parsing, snapshot selection, partition pruning from partition specs, schema evolution, and both copy-on-write and merge-on-read with positional and equality delete files. Reading Iceberg is table stakes in 2026.

**Iceberg write is scheduled later and is a separate claim**, because writing correct Iceberg with concurrent commits and catalog interaction is substantially more work than reading.

**Delta Lake read**, via the `delta-kernel` route rather than a from-scratch implementation, because the kernel exists specifically so that engines do not each reimplement the protocol.

**DuckLake.** DuckDB's own lakehouse format, which puts the catalog in a SQL database and the data in Parquet. Supported because DuckDB compatibility implies it and because the design is sound.

**Object storage** per document 5.7: S3, GCS, Azure, with credential chains, ranged reads, request coalescing and local caching. This is `httpfs`-equivalent functionality and it is in the core rather than an extension for the performance reasons in document 12.4.

## 13.5 Other databases

`postgres_scanner`, `mysql_scanner` and `sqlite_scanner` equivalents, because DuckDB users use them heavily and because they are the path by which data gets into an analytical engine in the first place. These can be actual DuckDB extensions loaded through the ABI in document 12.4 rather than reimplementations, which is a good test of whether that ABI implementation is real.

## 13.6 Substrait

The physical plan serializes to and from Substrait.

**This is the extension point for anything that wants to execute a plan differently**, and the concrete precedent is Sirius, a GPU engine that attaches to DuckDB as a Substrait-consuming extension and reports large speedups on TPC-H without a single line of GPU code in DuckDB's core. That is exactly the right shape: the accelerator lives outside, the interface is a plan, and the core stays a CPU engine.

It is also how a query gets handed to another system, how a plan gets inspected by an external tool, and how a plan gets stored for later execution.

## 13.7 Language clients

**Rust is the native API and is first-class.** It is not a wrapper over the C API. It exposes typed results, borrowed vectors for zero-copy access, an async interface, and the ability to register a Rust closure as a scalar or table function without going through FFI. This is the API that makes the project worth writing in Rust for anyone embedding it, and it should be good enough that a Rust user prefers it to `duckdb-rs`.

**Python is the highest-priority binding by usage.** DataFrame integration with pandas, Polars and Arrow; the `df()` and `arrow()` result methods; the relational API; user-defined functions; and the DB-API surface. Built on the C API for compatibility, with a native fast path for the data transfer, since going through the C API for a hundred-million-row result would give up the zero-copy story.

**Everything else comes through the C API**: R, Java via JDBC, Go, Node, C#, WASM. WASM is worth a note because DuckDB-WASM is a large part of DuckDB's reach and because a Rust codebase compiles to WASM with less friction than a C++ one, so this may end up being an area where we are simply better rather than merely compatible.

**A command line shell** with the same conveniences: readline, completion, `.mode` output formats, progress bars, dot commands matching DuckDB's, and the ability to open a DuckDB file directly.

## 13.8 What is not in the ecosystem plan

**No ORM integrations, no BI connectors, no cloud service.** Those are downstream of being a database people use and they are not on the critical path to being one.

**No native extension SDK beyond the DuckDB ABI**, per document 4.10. One extension mechanism.

**No support for reading ClickHouse's MergeTree format or any other engine's native format.** Parquet, Arrow, Iceberg, Delta, DuckLake and DuckDB's own format is already a large surface, and the marginal user who has data locked in a fourth engine's native format is better served by that engine exporting Parquet.
