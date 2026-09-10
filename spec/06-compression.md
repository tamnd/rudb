# Compression and encoded execution

This is the document the project's thesis lives in. Document 02.2 claims the order of magnitude is in physical layout rather than in code quality, and document 03.5 sets a disk budget that requires better than 10x over DuckDB. Both claims reduce to this: what the encodings are, how they cascade, how they span columns, and above all what an operator is allowed to do with an encoded vector without decoding it.

## 6.1 What a compressed database actually buys

Three separate things, and they are usually conflated.

**Less disk.** Directly axis 4, and on a cloud deployment directly money.

**Less I/O, therefore less time.** A scan that reads 1 GB instead of 6 GB is six times less work at the storage layer regardless of how fast the CPU is. On the `LIKE '%google%'` queries in document 03.4 this is most of the observed 14x to 18x gap between DuckDB and Umbra.

**Less CPU, if and only if the operator can work on the encoded form.** This is the one that is usually not delivered. A format that compresses well but decodes at the scan boundary has bought the first two and paid CPU for the privilege. Document 03.2's Vortex rows are the measured demonstration of what that costs: a genuinely good compressed format, plugged into two engines, made both of them 1.6x and 2.0x slower. Any design in this space must have an explicit answer for why it does not land there, and section 6.7 is ours.

## 6.2 The encoding set

Single-column encodings, all of which operate on units of 1024 values so that they compose with the vector size.

| Encoding | Applies to | Notes |
|---|---|---|
| `CONSTANT` | any | whole vector is one value |
| `BITPACK` | integers | FastLanes layout, 1 to 64 bits, data-parallel unpack |
| `FOR` | integers | frame of reference, subtract min then bit-pack |
| `DELTA` | integers, timestamps | delta then FOR then bit-pack |
| `RLE` | any | value array plus run length array, both then encoded |
| `DICT` | any | code array plus dictionary, codes then bit-packed |
| `FSST` | strings | 255-symbol table, byte-level, random access preserved |
| `DICT_FSST` | strings | dictionary whose entries are FSST-compressed |
| `ALP` | floats | adaptive lossless floating point, exact |
| `ALP-RD` | floats | real-double variant for high-entropy mantissas |
| `ROARING` | booleans, validity | bitmap with run and array containers |
| `SPARSE` | any | mostly-one-value with an exception list |

**The FastLanes bit-packing layout is not the obvious one.** A naive bit-packer writes values in order and requires a sequential dependency to unpack. FastLanes uses a permuted layout, the "unified transposed layout", chosen so that unpacking is fully data-parallel with no dependencies between lanes, which means the same kernel vectorizes on AVX-512, on NEON, and on a GPU's warp. The cost is that values come out in a permuted order within the vector, which is fine because every operator downstream is order-agnostic within a vector, and the permutation is undone only when a vector's values must be materialized in row order. Adopting this layout is a settled decision and it is the reason the vector size is 1024.

**ALP is the right float encoding and it is not a compromise.** It finds a decimal representation of a double where one exists, encodes the resulting integers with FOR and bit-packing, and keeps exceptions in a patch list. It is exact, it is fast in both directions, and Parquet is standardizing it alongside FSST in 2026, which means the format we build and the format the ecosystem converges on will agree.

**FSST's property that matters most is not its ratio.** It compresses text about 2x, which is worse than a general-purpose compressor. What it gives that a general-purpose compressor does not is random access to any string without decompressing its neighbours, and the ability to run a substring search against the compressed bytes by compressing the needle with the same symbol table. That second property is what turns `URL LIKE '%google%'` from a decompress-and-scan into a scan, and it is worth more on this workload than any ratio improvement.

## 6.3 Cascading

An encoding tree, not a single encoding. `DELTA` produces integers, which `FOR` shifts, which `BITPACK` packs. `DICT` produces codes, which `BITPACK` packs, and a dictionary whose entries are strings which `FSST` compresses. `RLE` produces two arrays each of which gets its own tree.

The tree is stored in the column chunk header as a small serialized structure, and the decoder is a fold over it. FastLanes calls this "expression encoding" and its contribution is showing that the cascade is where the ratios actually are: single encodings leave a lot on the table and two or three levels of cascade capture most of what a general compressor would find, while keeping random access and vectorized decode.

**The candidate set is hand-written per type, not searched generally.** For a 32-bit integer column the candidates are roughly: constant; FOR+bitpack; delta+FOR+bitpack; RLE with each array encoded; dict+bitpack; sparse+exception list. Six candidates, each evaluated on a sample, is cheap. General search over the space of trees is not and its marginal benefit is small. This is a deliberate limit and it is revisited only if measurement says the hand-written set is leaving more than a few percent.

**Sampling is per column chunk and the sample is not the first N rows.** A systematic sample across the chunk, because column data is frequently sorted or clustered and the first 1024 rows of a sorted column look constant. This is a small detail that produces a large mistake when got wrong.

## 6.4 Multi-column compression

This is the mechanism that has no equivalent in DuckDB and it is where a meaningful share of the axis-4 target comes from. FastLanes describes it as MCC and the idea is that columns are not independent, so encoding them independently discards real redundancy.

Three forms are in scope.

**Shared dictionaries.** Two or more columns drawn from the same value universe share one dictionary. On ClickBench `hits` the obvious pair is `URL` and `Referer`, both of which are URLs and which overlap heavily. Sharing means the union of values is stored once rather than each column's set separately, and both columns become code arrays over the same table. This also makes a join or comparison between them a code comparison.

**Shared FSST symbol tables.** Cheaper than a shared dictionary and applies more broadly. `URL`, `Referer`, `Title` and the various referer-derived columns all share a symbol table trained on a sample of all of them. Storing one 255-symbol table instead of six is trivially small, but the real effect is that a symbol table trained on the union compresses each column better than one trained on that column alone would, because there is more evidence per symbol.

**Correlation encodings.** Column B stored as a function of column A. The two useful shapes here are a functional dependency, where B is determined by A and is stored as a per-dictionary-entry lookup rather than a per-row value, and a numeric correlation, where B minus f(A) is small and gets FOR-encoded. On `hits` the functional dependency case is real: `URLHash` is determined by `URL`, `RefererHash` by `Referer`, and various region and country columns are determined by each other.

**Detection is at write time by sampling pairs, and the pair space is pruned.** Testing all 105 choose 2 pairs is 5,460 tests per row group, which is affordable once at load but not per row group. The design is to detect candidate relationships once over a global sample at table level, record them in the catalog as hints, and have each row group check only the hinted pairs. If a hint stops holding for a row group, that row group encodes independently and the correctness is unaffected.

## 6.5 Global dictionaries

A dictionary scoped to a column, or a group of columns, across the whole table rather than per row group.

**Why this is the highest-value single decision in the format.** Per-row-group dictionaries store `URL` values once per row group they appear in. With 814 row groups and a heavy-tailed URL distribution, common URLs are stored hundreds of times. A global dictionary stores each once. Beyond the disk saving, a global dictionary makes codes comparable across row groups, which means a hash aggregation on `URL` can hash a `u32` rather than a string, and two row groups' partial results combine without any string comparison at all.

**Construction is incremental and append-friendly.** New values encountered during a load are appended to the dictionary and get new codes. Codes are never reassigned during a load because that would invalidate already-written row groups.

**Code width growth is the hard part.** A dictionary that outgrows 16 bits must not force a rewrite of every row group written so far. The design is that code width is per column chunk, not per dictionary: a chunk written when the dictionary had 60,000 entries uses 16-bit codes and a chunk written later uses 20-bit codes, and both index the same dictionary. Decode is uniform because the chunk header says the width. This is the mechanism that makes global dictionaries workable in a database rather than only in a one-shot file writer.

**When a dictionary is not worth it, the writer says so.** A column with 90 million distinct values in 100 million rows gets no benefit and the memory cost of the dictionary during load is real. The write path measures distinct count with a HyperLogLog sketch on a sample and falls back to FSST-only for high-cardinality string columns. `WatchID` is the ClickBench example and it is the reason query 32 is hard.

**Deletes and updates do not shrink the dictionary.** An entry whose last reference is deleted stays until the table is rewritten by `VACUUM` or a background recompaction, which counts references per row group and rebuilds. This is a garbage collection problem and treating it as one is correct; trying to reference count at row granularity would put an atomic on the delete path for no benefit.

**The load-time memory cost is a real constraint and it is stated in document 19 as open question five.** A global dictionary for `URL` on this dataset could be hundreds of megabytes of live hash table during the load. That is fine on a 32 GiB machine and not fine as a general rule, so the dictionary builder must be able to spill, which means the load path has a spilling hash table dependency on document 07.8. If it turns out that the spilling dictionary builder makes loads unacceptably slow, the fallback is per-partition dictionaries over groups of row groups, which captures most of the benefit at some of the cost.

## 6.6 Recomputation rules

A column stored as a rule for deriving it rather than as data. Two forms.

**Derived columns.** `URLHash = hash(URL)`. Stored as a rule plus a verification checksum. On this dataset `URLHash` and `RefererHash` are 1.6 GB of `BIGINT` that carry no information not already in two string columns. A scan of `URLHash` decodes `URL` and hashes it. A scan that touches only `URLHash` therefore pays more CPU and less I/O than it would have, and a scan that touches both pays almost nothing extra.

**The correctness constraint is absolute.** A recomputed value must be bit-identical to the value that was written, on this machine and on every other machine, forever. That rules out anything involving floating point reassociation, anything involving a hash function that might be tuned later, and anything involving locale. The rule set is therefore small and explicitly versioned: the rule identifier encodes the exact function and the exact version, and a rule version is never changed in place. If a hash function is improved, new writes use rule version 2 and old data keeps rule version 1, and both implementations stay in the binary forever. That is a genuine long-term maintenance cost and it is the main argument against this feature.

**Detection is by sampling.** During load, for each pair of columns where one is fixed-width and the other is a candidate input, test a sample against the rule set. This is cheap because the rule set is small. It is also conservative: a rule is only applied if it holds on every sampled row and then on a full verification pass, and the full pass is what the load-time cost is.

**The cost model gates application.** The rule is applied when the estimated I/O saved exceeds the estimated CPU added, weighted by observed or expected access frequency for the derived column. A column that is in the table and never queried is the ideal candidate. A column that is a join key on the hot path is not. Absent workload information, the default is to apply only when the derived column is not a key of any constraint and the recomputation is a single pass over an already-needed column.

**This is the feature most likely to be cut**, and document 19 open question four says so. Its disk contribution on this dataset is around 1.6 GB out of a 20.46 GB starting point, which matters at a 2.05 GB target and would not matter at a 5 GB one.

## 6.7 Encoded execution

Everything above buys disk and I/O. This section is what buys CPU, and it is the part that distinguishes this design from a good format bolted to a normal engine.

**The contract.** A scan may hand an operator a vector in an encoded physical form. Every operator must accept every physical form. An operator either has a specialized path for a form or calls `decode` and takes the general path. The general path is always available, so correctness never depends on a specialization existing, and adding a specialization is a pure performance change that the differential harness in document 14 validates by construction: the same query with specializations disabled must produce identical results.

**The specializations that pay, in order of measured value on this workload.**

*Dictionary codes as group keys.* `GROUP BY URL` over a global dictionary hashes and compares `u32` codes. The hash table's key is 4 bytes instead of a 16-byte string reference plus a pointer chase plus a `memcmp`. Only the surviving groups are decoded, and for a `LIMIT 10` that is ten strings. This is queries 33 and 34 and a large part of 12, 13, 16, 17 and 18.

*Dictionary codes as join keys.* Same argument. When both sides share a dictionary, which they do when they are the same column of the same table or two columns sharing a dictionary per 6.4, the join is an integer join.

*Predicate transformation into the encoded domain.* `WHERE x > 1000` on a FOR-encoded column with base 900 becomes `WHERE packed > 100` evaluated on the bit-packed representation directly. On a dictionary-encoded column, an equality predicate becomes a single code comparison after one dictionary probe, and a range predicate becomes a precomputed code bitmap when the dictionary is order-preserving or a code set otherwise. On an FSST column, `LIKE '%needle%'` becomes a search for the FSST-compressed needle in the compressed bytes, valid whenever the needle compresses to a symbol sequence with no ambiguous boundary, with a verification pass on hits.

*RLE run arithmetic.* `SUM(x)` over an RLE run of value v and length n is `v * n`, one multiply for n rows. `COUNT(*) WHERE x = v` over RLE is a sum of matching run lengths. On the 48 near-constant `SMALLINT` columns in `hits` this turns a 100-million-element scan into a few thousand run operations. This is why query 29, ninety sums over a 16-bit column, is near its floor on both engines and why it stays there for us.

*Constant vector propagation.* A constant vector through an expression tree produces a constant vector, evaluated once. Trivial and it fires constantly on real data.

*Bitpacked comparison without unpacking.* For widths that divide evenly into a lane, a comparison against a constant can be done on the packed words with a mask and a compare, producing the result bitmap without materializing the values. This is a narrower win than the others and it is a tier-2 item.

**The rule that keeps this from becoming a combinatorial disaster.** Number of operators times number of physical forms times number of types is not a tractable amount of code to write by hand. The answer is that specialization is generated, not written: a macro-driven kernel generator produces the cross product for the cases that are worth it, from a table that says which operator-form-type combinations get a specialized path. The table is the thing humans edit. Everything else is generated and tested identically. Document 07.3 and document 16.2 cover the generator and its testing.

**Where this design fails and how we will know.** If specialized paths are rare enough that most vectors take the decode-and-general path, we have paid the format's complexity and gotten Vortex's result. The measurable guard is a per-query counter of vectors processed encoded versus decoded, reported by `EXPLAIN ANALYZE` and asserted in the benchmark harness. Milestone M3's exit criterion is a specific number on that counter for the ClickBench set, and it is the earliest point at which the project's central thesis is falsifiable. Document 17 sets it.

## 6.8 What we are not doing

**No general-purpose block compressor in the hot path.** No zstd on data blocks. It compresses better and it destroys random access and encoded execution, which are the entire point. Zstd is available for cold archival row groups and for the WAL, where neither property matters.

**No lossy anything.** Not for floats, not for sketches used as stored values. ALP is exact and that is why it is the choice.

**No user-visible encoding annotations at first.** No `LowCardinality(String)`, no `CODEC(...)`. The system decides. A `PRAGMA` to force an encoding exists for testing and debugging and is not documented as a user feature until there is evidence that the automatic decision is wrong often enough to need an override. Making the encoding a schema decision means an imported table never gets it, which is the exact gap this design is trying to close.
