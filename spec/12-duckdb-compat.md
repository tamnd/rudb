# DuckDB compatibility

Axis 1 says 100 percent compatible with DuckDB. That phrase means nothing until it is decomposed into surfaces, each of which is separately claimable and separately measurable. This document does the decomposition. Document 14 specifies the harness that verifies each claim; this one specifies what the claims are.

The reason this is a well-posed problem now and would not have been eighteen months ago is that DuckDB v2.0 froze things that used to move: a stable C extension ABI specified in versioned YAML files, storage format v2.0, and a documented client-server wire protocol. Before that, targeting compatibility meant chasing a moving target with no written specification. It is still partly that, and section 12.7 is honest about it.

## 12.1 Surface 1: storage format

**The claim: `rudb` opens any DuckDB database file, reads it at native speed, writes it back, and DuckDB opens the result.**

DuckDB's on-disk format is a 4 KiB header pair, each with a uint64 checksum, the magic bytes `DUCK`, and a uint64 storage version. Versions 64 through 68 are in the field, with v2.0 corresponding to storage format 2.0 and DICT_FSST as a default string compression. The format is documented at a level sufficient to implement and the source is the authority for the rest.

**Read is a full implementation of their format.** All compression methods including `RLE`, `BITPACKING`, `DICTIONARY`, `FSST`, `DICT_FSST`, `ALP`, `ALPRD`, `CHIMP`, `PATAS`, `ROARING` and uncompressed, all types, row group metadata, the block manager, deletion bitmaps and the WAL.

**Write is the same format at the same version**, chosen by the compatibility level of the target. Writing back a file DuckDB reads is what makes migration reversible, and reversible migration is what makes people willing to try.

**`ATTACH 'file.duckdb' AS db` reads in place without conversion.** This is the mode that matters most in practice: a user with a large existing database points at it and runs queries. Performance in this mode is bounded by their format, not ours, and we should say so rather than let anyone infer that our benchmark numbers apply to attached DuckDB files. Document 15's reporting rules require attached-mode numbers to be labelled as such.

**Conversion to native format is a `COPY FROM DATABASE` or a `CREATE TABLE AS`.** After conversion, the numbers in document 03's targets apply.

**Cost and risk.** Reading their format is a few engineer-months and is mostly mechanical. The risk is not difficulty, it is that the format changes: DuckDB has bumped storage versions repeatedly and will again. Section 12.7 covers the treadmill.

## 12.2 Surface 2: SQL dialect

**The claim: any query that DuckDB accepts, `rudb` accepts and produces the same result.**

This is document 10 and it is the largest surface. It is measured by weighted function coverage and weighted statement coverage per document 10.7, verified by differential execution per document 14.

**It will not reach 100 percent and the number should be published rather than rounded.** A published 99.4 percent weighted coverage with a list of what is missing is a stronger and more useful claim than an unqualified 100 percent that a user disproves in an afternoon.

## 12.3 Surface 3: the C API

**The claim: a program compiled against `duckdb.h` links against `librudb` and works.**

DuckDB's C API is the foundation of every language binding, so implementing it gets Python, R, Java, Go, Node, Rust and the rest for free in the sense that those bindings are mostly wrappers over it. This is the highest-leverage compatibility surface per unit of effort in the entire project.

**We ship a `libduckdb`-compatible shared library**: same symbol names, same struct layouts, same enum values, same semantics including the ownership and lifetime rules for every returned pointer. Getting a struct layout wrong produces silent memory corruption in a caller, so this surface gets its own ABI test suite that checks sizes and offsets against a reference generated from a real DuckDB header.

**The v2.0 stable ABI is specified in versioned YAML in the DuckDB repository**, which is a genuine gift: it means the surface has a machine-readable definition, and `rudb-compat` generates both the Rust FFI declarations and the conformance tests from it rather than transcribing by hand.

**Panics never cross the boundary.** Every entry point catches and converts to the API's error convention. This is enforced by the generator, not by discipline.

**The Rust-native API is separate and is the primary one.** It is not a wrapper over the C API and it does not inherit the C API's design constraints. Document 13.7.

## 12.4 Surface 4: extensions

**The claim: a DuckDB extension binary loads into `rudb` and works.**

DuckDB v2.0's C extension ABI is versioned and stable, which is what makes this possible at all. An extension is a shared library exporting an init function that receives a struct of function pointers into the host and registers scalar functions, table functions, replacement scans, storage extensions and optimizer extensions.

**We implement the host side of that struct.** Every function pointer an extension can call, with matching semantics.

**This is where a real limit is, and it is worth stating plainly.** Extensions that use only the stable C ABI will work. Extensions that link against DuckDB's C++ internals will not, and several important ones historically have. The set that works is empirically determined by testing against the actual extension repository, and `rudb-compat` publishes a per-extension status table rather than a blanket claim. The extensions that matter most are `httpfs`, `parquet`, `json`, `icu`, `fts`, `spatial`, `iceberg`, `delta`, `postgres_scanner`, `mysql_scanner` and `sqlite_scanner`, and the first four are close to mandatory for the engine to be useful at all.

**Some of those we implement natively rather than loading.** Parquet, JSON and httpfs are in the core because they are on the performance path and because a DuckDB extension implementation of them would be bounded by DuckDB's data structures. The extension of the same name is then a no-op that reports itself installed, which is a small compatibility lie that exists so that a script running `INSTALL parquet; LOAD parquet;` works. This is documented, not hidden.

## 12.5 Surface 5: behaviour

The parts that are not syntax or API but that tests and tools depend on anyway.

**Error messages.** A test asserting on an error string is common. We match DuckDB's message text for the errors that appear in their test suite and in common usage, and we do not attempt to match all of them. The stable interface is the error code; the message is best-effort and that is stated.

**Result metadata.** Column names for expressions, which DuckDB derives by specific rules that tools depend on. Column types, including the exact decimal precision and scale of a computed expression. Result ordering for queries without `ORDER BY`, which is not guaranteed by SQL and which some tests depend on anyway, and where we make no promise.

**`PRAGMA` and setting names, defaults and effects**, since scripts set them.

**Catalog table contents**, meaning `duckdb_tables()`, `duckdb_columns()`, `duckdb_functions()` and the `information_schema` views, because tools introspect through them.

**Explicitly not matched: `EXPLAIN` output, profiling output format, and internal function behaviour that is not documented.** Matching explain text would freeze our optimizer to their operator vocabulary and that is too high a price.

## 12.6 Surface 6: the wire protocol

DuckDB v2.0 introduced Quack, a client-server protocol, which turns DuckDB into something that can be reached over a socket. Implementing the server side means existing DuckDB clients connect to `rudb`.

**This is scheduled at M10 and it is the lowest priority of the six surfaces**, because the embedded use case is the primary one and the protocol is new enough that its client ecosystem is small. It is in the plan because it is cheap once everything else exists and because it is the natural path to a server deployment mode.

## 12.7 The treadmill

**This is the second of the two project killers named in document 00, and it deserves a direct statement.**

DuckDB ships releases regularly and each one adds functions, syntax, sometimes types, and occasionally a storage version. Compatibility is not a state that is reached, it is a rate that must be sustained. A project that reaches 99 percent coverage against v2.0 and then spends a year on performance work is at some lower number against v2.3, and the number goes down by itself.

**What makes it survivable.** The rate of change is measurable: `rudb-compat` tracks each DuckDB release and reports the delta in the coverage number, so the maintenance cost is visible rather than discovered. Most releases add functions, which are individually cheap. Storage version bumps are the expensive ones and they are infrequent. And the differential harness means a new DuckDB release produces a list of specific failures rather than a vague sense of falling behind.

**What makes it dangerous.** If the sustaining cost exceeds roughly one engineer full time, the project has a permanent tax that competes directly with the performance work that is the entire reason for its existence. Document 02.7 sets this as a kill criterion: if after 1.0 the compatibility maintenance is consuming more than a third of capacity, the correct response is to freeze the compatibility target at a named DuckDB version and say so publicly, rather than to quietly fall behind while still claiming compatibility.

## 12.8 The compatibility levels

Rather than one binary claim, four named levels, each independently verified and published.

**Level 0, data compatible.** Reads and writes DuckDB files at full fidelity. The minimum useful claim and the one that makes migration reversible.

**Level 1, query compatible.** Level 0 plus the SQL dialect at a published weighted coverage. This is what most users mean when they say compatible.

**Level 2, API compatible.** Level 1 plus the C API, so existing programs and language bindings work unmodified.

**Level 3, ecosystem compatible.** Level 2 plus extensions loading and the wire protocol, with a per-extension status table.

**Each level has a published status and a test suite that produces it automatically.** A user reads the table and knows exactly what they get. That is a more useful and more honest artifact than the phrase "100 percent compatible", and it is what document 00's axis 1 should be read as meaning.
