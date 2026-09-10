# Layer four: the hash table

This is sub-milestone 2f. It is one layer and one data structure, shared by the aggregate in document 07 and the join in document 08, and it is specified before either of them because building two hash tables is how a database ends up with two sets of hash table bugs and two different answers to what a null key is.

Everything past the simplest query is mostly joins and grouping, both of which are this structure. On TPC-H it is where most of the time goes. On ClickBench the aggregate is where the time goes after the scan. It is the second largest performance item in the engine after the scan, and unlike the scan it is a single self-contained artefact that can be measured on its own.

## 6.1 What exists today

`crates/rudb-exec/src/key.rs` is 173 lines and it defines `Key(Vec<Value>)`, a row of values with `PartialEq` and `Hash` implemented so that two nulls group together and `IS NOT DISTINCT FROM` is the rule rather than `=`. The module doc explains exactly why that type exists and it is right: grouping, `DISTINCT` and the set operations all ask the same question and they must not disagree about it. Floats get the same treatment as in the comparison kernel, two NaNs are one value and negative zero is zero, with the reason written down.

`group.rs` uses it as `HashMap<Key, usize>` from key to slot, with a `Vec<Key>` alongside preserving first arrival order, and builds one `Key` per row per chunk with `keys.iter().map(|column| column.value_at(row)).collect()`.

`join.rs` has no hash table. Every join is a nested loop, and its module doc says plainly that this is the one place where the first implementation is asymptotically worse rather than merely slower, and that a hash join is the single largest performance item in the executor.

## 6.2 The finding

`Key(Vec<Value>)` costs, per row: one `Vec` allocation, one `Value` construction per key column which for a varchar is a `String` allocation and a copy, a `SipHash` of the whole thing through the std default hasher which is a keyed cryptographic-strength hash chosen for HashDoS resistance rather than for speed, a `HashMap` probe, and on insert a full clone of the key so that the `order` vector can hold a copy.

A single-column `GROUP BY` on a `BIGINT` therefore costs two heap allocations, a SipHash round and a clone per row, where it should cost one multiply, one masked load and a compare. That is two orders of magnitude, and it is measurable today by grouping ten million integers.

For a varchar key it is worse, because the `String` is allocated on every row on the probe path even when the key already exists in the table, which is the common case for a low-cardinality group.

This is the same defect as the one in document 03 section 3.2, in a different place, and it has the same cause: `Value` is the only interface the code had. Layer one removes it from the kernels and this layer removes it from the hash path.

## 6.3 One table, two uses

The aggregate wants a map from group key to a payload of aggregate states, updated in place, with the payload being fixed width and known at plan time.

The join wants a map from build key to a list of matching build rows, probed by key, with the payload being the build side's columns and with a per-row match flag for the outer variants.

Those are the same structure with different payloads. What they share is the key encoding, the hash function, the probe loop, the growth policy, the memory accounting and the partitioning, and every one of those is a place where two implementations would drift. What differs is what happens when a key is found, which is a callback or a payload layout rather than a different table.

So `rudb-exec` gets one `HashTable` with a payload width and a payload interpretation supplied by the caller, and the aggregate and the join are two callers. The one place the two genuinely diverge is duplicate keys: the aggregate wants one slot per distinct key and the join wants every build row with that key reachable, so the table supports both a unique mode and a multi mode, and that is a flag rather than a fork.

## 6.4 Key encoding

Keys stop being `Vec<Value>` and become a fixed-width byte sequence computed vectorized, one column at a time over a whole chunk, which is the normalized key form that a sort also needs and that document 09 reuses.

The encoding rule per column: a fixed-width type contributes its bytes in an order that makes unsigned byte comparison agree with SQL comparison, which for a signed integer means flipping the sign bit and for a float means the standard IEEE to sortable-integer transform, with the NaN and negative zero normalization from `key.rs` applied first so that the two definitions cannot drift apart. A nullable column contributes one leading byte for the null flag, and null sorts and groups consistently. A varchar contributes either its bytes when the total key is under the fixed-width budget or a hash plus a reference to the value when it is not.

The vectorized part matters as much as the layout. Encoding a chunk's worth of keys is a loop per column over a thousand rows writing into a strided buffer, not a loop per row over the columns, because the per-column loop is the one that has a known type and a known width and therefore vectorizes.

ClickHouse's aggregation method selection is worth copying and it is one of the reasons they win on ClickBench. They pick a specialized hash table implementation by the shape of the key: a single 8, 16, 32 or 64 bit key, a pair packed into 128 bits, a low-cardinality dictionary key, or a serialized fallback. A single `BIGINT` key never touches the general path at all, and single integer keys are an enormous fraction of real `GROUP BY` clauses.

rudb does the same with four cases, chosen at plan time from the bound types and not at runtime.

A single fixed-width key of 64 bits or less, used directly as the key with no encoding step at all. This is the case that has to be perfect because it is the most common.

Multiple fixed-width keys totalling 128 bits or less, packed into a `u128`.

Any key set whose encoded form is fixed width and fits a chosen budget, held inline in the table.

Everything else, meaning long or variable-length keys, held out of line with the inline part being a hash and a pointer into a key arena, with a full comparison only on hash match.

The low-cardinality case, where the key column is dictionary-encoded by the scan and the dictionary is small, degenerates to an array indexed by code with no hashing at all. On `hits` this applies to several of the ClickBench `GROUP BY` columns and it turns the aggregate into a scatter, which is as fast as this gets. It is only available when the scan can prove the dictionary covers the whole scan, which the native format can and Parquet mostly cannot because its dictionaries are per page.

## 6.5 The hash function

Not SipHash. The std default hasher exists to resist adversarial key collisions in a web server and it costs several times what an analytical hash should.

For fixed-width keys of 64 bits or less, a multiply and xor-shift finalizer, which is a handful of instructions and has good enough avalanche for the upper bits, which are the ones the table uses. For longer keys, a fast non-cryptographic hash over the encoded bytes in the xxh3 family, written in the workspace because of the zero-dependency rule and because it is a few hundred lines.

Hash quality gets a test rather than an assumption: chi-squared over the bucket distribution for adversarial-shaped real key sets, meaning dense sequential integers, integers with a common high bit pattern such as timestamps, aligned pointers, and the actual key columns from `hits`. Sequential integers through a bad multiplier is the classic failure and it is exactly what a synthetic benchmark of `GROUP BY id` produces.

There is no HashDoS defence and that is a deliberate decision recorded here. An embedded analytical database is not accepting keys from an adversary over a network, and paying a cryptographic hash on every row to defend against a threat that does not exist is the tradeoff Rust's standard library makes for a different situation.

## 6.6 The structure

Three candidates, and the survey in document 01 is what decides between them.

Chained, meaning a bucket array of pointers into linked lists, which is what DuckDB's join uses. Simple, handles duplicates naturally, and every probe that misses still costs a pointer chase unless the bucket is empty.

Open addressing with linear probing, which is what DuckDB's aggregate uses, with a salt packed into the unused upper bits of the entry so that a mismatch is detected by a byte comparison in a register rather than by dereferencing and comparing the key. The salt trick is worth a lot and it is not optional.

Unchained, from Birler, Kemper and Neumann at DaMoN 2024. Build in two passes, count keys per bucket, prefix sum, and write tuples into a single contiguous array so that all tuples for a bucket are adjacent. There are no chains and no pointers, the probe reads a directory entry and then reads a contiguous run, and the directory entry carries a tag so most misses are answered without touching the tuple array at all. It requires knowing the build size before writing, which a join build side does know because it is fully materialized before the probe starts.

The decision. The join uses unchained, because the build side is materialized anyway, because the contiguous layout is much better for the probe's cache behaviour, and because the tag directory answers the misses that dominate a selective join. The aggregate uses open addressing with salt, because an aggregate cannot be built in two passes over a known size, it is updated incrementally as chunks arrive, and linear probing with in-place payload update is the right shape for that.

That is two structures again, which section 6.3 said to avoid, so the thing that is shared has to be named precisely. Shared: the key encoding, the hash function, the salt and tag computation, the memory accounting, the growth policy and the partitioning decision. Not shared: the probe loop and the insert loop, which are twenty lines each and are genuinely different algorithms. That split is defensible where a single table with a mode flag pretending the two are the same would not be.

## 6.7 Partitioned or global

The received wisdom from the multi-core join papers of the 2010s is that radix partitioning wins, because each partition's table fits in cache and the probe becomes cache-local.

Global Hash Tables Strike Back, from 2025, is the paper that revisits this on current hardware and finds that a single shared table with atomic insertion often wins, because partitioning costs a full materialization pass over the build side and modern cores hide latency well enough that the cache locality advantage no longer pays for it. The result depends on build size and on core count, so it is a decision rather than a constant.

rudb builds the global path first, because it is simpler, because it needs no extra pass, and because the paper says it is the better default. Partitioning goes in later behind a threshold on build size, and the threshold is measured on `server3` and `server1` rather than chosen, with the measurement being TPC-H Q9 and Q21 at SF100, which are the two queries with the largest build sides.

The global table needs concurrent insertion, which means the insert path is a compare-and-swap on the entry rather than a write. That is a small amount of care and it is done from the start rather than retrofitted, because a table designed for single-threaded insert is a table that gets rewritten at layer eight, which is exactly the pattern document 00 exists to avoid.

## 6.8 Prefetching

A hash probe is a dependent load into a table larger than cache, and the core stalls on it. This is the dominant cost of a large join and no amount of instruction-level tuning around it helps, because the problem is a cache miss and not an instruction count.

The fix is old and well established: process the probe in two passes over a batch. First pass computes the hash for every row in the chunk and issues a software prefetch for the bucket each one will touch. Second pass does the actual probes, by which time the lines have arrived. With a chunk of a thousand rows there are far more outstanding misses than the memory system would otherwise be given, and the effect on a table that does not fit in L3 is large.

This is why the batch interface exists and it is a concrete reason the vectorized design beats a naive compiled tuple-at-a-time loop on exactly this operation, which is the point Kersten et al. make. A compiled engine has to reintroduce batching by hand to get this, and the vectorized engine gets it for free.

The prefetch distance is a tunable and it is measured, not guessed, across the fleet, because it depends on the memory system.

## 6.9 Memory and the spilling seam

The table knows how many bytes it is using and reports it. That is required before layer eight can do anything about it and it costs nothing to maintain.

The table is built so that it can be partitioned by hash after the fact without rehashing, which means the partitioning bits are the top bits of the hash and the table records which bits are in play. That is the seam a spilling implementation needs, because spilling means writing some partitions to disk and keeping the rest, and a table that cannot be split by hash cannot spill without recomputing everything.

Nothing spills at this layer. The seam goes in because it is free now and expensive later.

## 6.10 What layers five and six add

The aggregate in document 07 adds the payload layout for aggregate states, the per-thread partial tables and their merge, and the distinct and ordered aggregate variants.

The join in document 08 adds the eight join kinds and their rules about unmatched rows, the match bitmap for the outer variants, the build-side Bloom filter for early probe rejection, and the semi and anti shapes that do not need a payload at all.

Both are thin on top of this if this is right, and both are a rewrite of this if it is wrong, which is the argument for specifying it separately.

## 6.11 The test gate

The existing `Key` becomes the oracle, exactly as `compare_values` did in layer one. Every table operation is checked against a reference `HashMap<Key, _>` over random data with random nulls, random duplicates and random types, and any disagreement is a defect. That reference is slow and correct and it already encodes the SQL semantics carefully, which is why it is worth keeping rather than deleting.

The null rule gets its own suite because it is the thing most likely to be got wrong twice. Two nulls group together, a null key in a join matches nothing, `DISTINCT` collapses null rows, and `UNION` treats them as one row, and those are four different-looking rules that all follow from one definition. Each one is tested directly against DuckDB's answer through `rudb-compat`.

Float keys: NaN, negative NaN, positive and negative zero, and the requirement that grouping by a float column produces exactly the groups DuckDB produces.

Collisions get forced. A test hash function that returns a constant makes every key collide, and the table must still be correct, just slow. That test finds the probe loop bugs that a good hash function hides for years.

Concurrent insertion is tested with many threads inserting overlapping key sets and asserting the final table equals the single-threaded result, run under a stress loop rather than once.

## 6.12 The benchmark gate

The hash table gets its own microbenchmark suite in `rudb-bench` because it is the one operator that can be benchmarked in isolation meaningfully.

Build and probe throughput in rows per second per core, for each of the four key cases from section 6.4, at build sizes of a thousand, a hundred thousand, ten million and two hundred million, which spans L2 to well past L3 and is where the interesting behaviour is. Probe hit rates of 100, 50, 10 and 1 percent, because a selective join is mostly misses and misses are what the tag directory is for. Key distributions of unique, uniform duplicates and heavily skewed, because skew is what breaks a partitioned build.

Prefetch on and off at every size, which produces the number that justifies section 6.8 and the tuning curve for the distance.

Global against partitioned at every size and at one, four and eight threads, which produces the threshold from section 6.7.

The whole-query gate is the aggregate and join queries, but the join operator does not exist yet at this layer, so the gate at 2f is the aggregate path only: ClickBench Q6 to Q11 and Q28 to Q33 which are `GROUP BY` heavy, and TPC-H Q1 which is an aggregate over a small group set. The target is a factor of ten in CPU seconds against the 2e number on those queries, which sounds large and is not, because it is measured against `Key(Vec<Value>)` with SipHash and two allocations a row.

The number that matters against DuckDB is single-threaded probe throughput on a table larger than L3, because that is the number that cannot be bought with threads and it is the one that says whether the structure is right. The target is to match or beat DuckDB there, and beating it is plausible because the unchained layout is newer than what DuckDB's join uses.

## 6.13 Exit criterion for 2f

**One key encoding and one hash function, four specialized key cases chosen at plan time, an unchained table for the join shape and a salted open-addressed table for the aggregate shape, concurrent insertion, prefetched batch probing, byte accounting and hash-bit partitioning seams present, checked against the `Key` oracle and against DuckDB through the corpus, with the microbenchmark suite committed and CPU seconds on the `GROUP BY` queries down by ten times against 2e.**

Named as deferred: spilling, which has its seam here and its implementation in document 10, and radix partitioning, which is measured here and enabled behind a threshold once the measurement says where the threshold is.
