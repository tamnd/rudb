# Aggregation, sort and windows

The operators that TPC-H, ClickBench and TPC-DS spend their time in once the joins are handled: grouped and ungrouped aggregation, `DISTINCT` and set operations, sort, top-N, window functions and grouping sets. The organizing rule is the same for all of them. **Generated code does the per-tuple work: the key encoding, the hash, the compare, and the state update. Precompiled runtime code does the data structure work: growth, partitioning, merging, sorting and spilling.** The line between the two is the one document 03 §3.6 draws. This document says where it falls for each operator.

## 11.1 Where the time is, per suite

- **JOB** has no `GROUP BY`. Its only aggregate is an ungrouped `MIN`, which document 10 §10.3 either folds per relation or keeps in registers (§11.3).
- **TPC-H** is aggregation-heavy: Q1 is a four-group aggregate over all of `lineitem`, and Q13, Q17 and Q18 aggregate over a join on the join key. Kersten et al. found the compiled engine 74% faster than the vectorized one on Q1, and the vectorized one 32% faster on Q9 (https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf). Aggregation is where compilation already wins, while joins are where it has to be taught to (document 10). The C8 gate is TPC-H SF1 at no more than 1/10 of DuckDB's instructions.
- **ClickBench** is mostly `GROUP BY … ORDER BY count DESC LIMIT 10` over one wide table, with high-cardinality keys (strings, user ids) and `COUNT(DISTINCT …)`. Aggregation strategy and top-N dominate it (document 02 §2.4, C9).
- **TPC-DS** adds windows, `ROLLUP` and set operations to all of the above (C10).

**Bespoke engines point the same way.** An LLM-synthesized engine reached 11.17x over DuckDB on TPC-H SF20 single-threaded, and "inline fused aggregation" was its most used technique, in 97.4% of queries (https://arxiv.org/abs/2603.02001). Fused aggregation is not the novel part of this document. Doing it without knowing the workload in advance is.

## 11.2 The aggregate state layout

**The layout of a group's state is fixed at plan time and generated per query. Its offsets are constants in the code.** One group row is:

```
[ key columns | aggregate states, in declaration order | NULL-tracking bits | (padding to 8) ]
```

Six rules, in the order they save bytes or instructions:

1. **Only the inline aggregates of document 03 §3.4 get inline state:** `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`, `ANY_VALUE`, `BOOL_AND`, `BOOL_OR` and `COUNT(DISTINCT)`. Anything else gets an opaque slot that the first engine's `update` and `combine` fill through the runtime ABI. An opaque slot is correct and slower, and the router logs it.
2. **Shared subexpressions share state.** `AVG(x)` is a `SUM(x)` and a `COUNT(x)`, and when `SUM(x)` and `COUNT(*)` are also present the planner deduplicates them. The final `AVG` division happens once per group in the output pipeline, never per tuple.
3. **Accumulators are narrowed by exact facts and widened by guards.** When the input's type is `INTEGER` but its certified range and the morsel's row count bound the sum inside `i64`, the state is an `i64` with no overflow check. When only zone-map bounds exist, the worker checks its headroom at morsel start (bound so far + rows × max|v| against `i64::MAX`) and widens its table to the declared `i128` variant when the check fails, so no morsel is rerun (document 09). A scalar `SUM` may instead accumulate a morsel-local partial with a checked add and fold it after a morsel-end check. With no bound at all, the planner does not narrow. `DECIMAL` sums are `i128` as document 12 specifies. `SUM(DOUBLE)` is an `f64` added in input order, with no reassociation (document 03 §3.8).
4. **NULL tracking is specialized away when the input column is certified non-null.** Otherwise one bit per nullable aggregate records "seen a non-null value", which is what `MIN`, `MAX`, `SUM` and `AVG` need in order to return `NULL` for an all-null group.
5. **`MIN` and `MAX` on strings hold a 16-byte string header.** The header's inline prefix decides most comparisons. When a new extreme is found, the generated code calls `rt_str_retain(arena, s)`, which copies the string into the thread's aggregate arena, because generated code never constructs strings. A new extreme is rare after the first few morsels, so the call is off the hot path.
6. **Hot state goes first.** States that every update touches are placed next to the key, so an update touches one cache line when it can.

## 11.3 Ungrouped aggregation: states in registers

**An ungrouped aggregate keeps its states in registers for the whole morsel and writes them to the thread's state slot once at the end.** There is no memory traffic per tuple. The update is fused into the pipeline body with no call, which is the Q1 and Q6 shape and JOB's final `MIN`.

```
; illustrative QIR; document 06 owns the syntax
entry:      s_sum = const.i128 0 ; s_cnt = const.i64 0 ; s_min = load state.min
loop body:  v     = load.dec64 col_extprice, i
            s_sum = add.i128 s_sum, (sext v)
            s_cnt = add.i64  s_cnt, 1
            s_min = smin.i64 s_min, v
exit:       combine_local state.sum, s_sum     ; per-thread slot, no atomics
            store state.cnt, (add (load state.cnt) s_cnt)
            store state.min, s_min
```

With several aggregates the register pressure rises. The `direct` backend spills its register-resident states to the thread-local slot when it runs out of registers (document 08). It never changes the order of floating-point additions to reduce pressure. The final combine across threads is precompiled and runs in thread-index order, so a single-threaded run is deterministic. In a multi-threaded run a float `SUM` depends on which morsels each thread took, exactly as it does in DuckDB, and document 15's harness compares float aggregates under the tolerance DuckDB's own tests use.

## 11.4 Grouped aggregation: the strategies and the rule that picks one

**Five strategies. The planner picks from facts where the facts are exact, and the runtime picks between the hash strategies from what it observes. Only the key and update bodies are generated. The drivers are precompiled.** The strategies:

| Strategy | Precondition | Per-tuple work |
|---|---|---|
| dense array | key is a dictionary code, or an integer with a certified range, and the product of the key domains ≤ 2^16 | `slot = mixed_radix(codes)`, update in place |
| perfect hash on dictionaries | key columns are dictionary coded with a global dictionary or a per-row-group remap, and the product of the domains ≤ 2^20 | one remap lookup per key column, then as for dense |
| thread-local, then partitioned | general; the default | hash, probe the L2-sized local table, update; overflow goes to partitions |
| global ticketed | general; chosen at runtime when observed group counts favour it | hash, fetch a ticket from the shared table, update the ticket's slot |
| groupjoin | the group key equals the join key of a join whose build side is unique on it | update the state stored in the join table's entry (§11.6) |

**Dense and perfect-hash aggregation are where the facts pay directly.** TPC-H Q1 groups on `l_returnflag` and `l_linestatus`, which are two dictionary-coded single characters. Their domain product is a handful of slots, so the aggregation is an array of that many state rows with no hashing and no comparing. The array fits in registers or L1. A per-row-group dictionary costs one remap array lookup per key column, built by the scan kernel when the row group is opened. The planner relies on the domain sizes only if `../stats/` certifies them. Otherwise it attaches a guard: when a code falls outside the domain the morsel is deoptimized to the hash strategy.

**Thread-local, then partitioned, is the default.** This is the morsel-driven design that DuckDB and DataFusion also use (https://db.in.tum.de/~leis/papers/morsels.pdf):

1. Each thread aggregates into a local open-addressing table sized to fit in L2, with the key, the hash salt and the states inline in the group row.
2. When the table fills, the precompiled driver flushes it into the thread's partition buffers by high hash bits.
3. After the last morsel, one task per partition merges the thread-local fragments with the generated `combine` body, then runs the generated finalize.

In generated code, an insert that finds the table full returns `Status::NeedMemory(slot)` with the table's slot id (document 05 section 5.4.1). For a thread-local aggregation table the runtime's answer is a flush into the partition buffers, not a larger table. The runtime flushes and re-enters the morsel at the tuple that stopped, which is the restartable-morsel contract in document 03 §3.7.

**Global ticketing is evaluated at C8, not assumed.** The idea (https://arxiv.org/abs/2505.04153) is a shared, concurrent linear-probing table that hands each group a dense ticket, with states in arrays indexed by ticket and ticket ranges handed out per thread to avoid contention. The paper reports 1.78x over partitioned aggregation at low cardinality with 48 threads, and the notes record a 34.7x figure for atomics at high cardinality. It also assumes perfect cardinality estimates, which we will not have. The switch rule is our design:

- After a thread's first four morsels, the runtime reads the local table's distinct-group count and flush count.
- **Few groups and no flush:** stay thread-local and skip partitioning entirely. The final merge is a small combine.
- **Many groups:** partition, unless the C8 measurement shows ticketing wins at that group count on both target machines, in which case switch to ticketed.

Both drivers call the same generated hash, compare and update bodies, so switching costs no compilation.

**Eager aggregation below joins** is a document 04 decision. The generated code is the same update body, run in a pipeline further down. The rule is the classical one: push a partial aggregate below a join when the grouping includes the join key, or when the aggregate is duplicate-insensitive (document 10 §10.8). `SUM` and `COUNT` pushed below an n:m join carry a multiplicity column, and the upper aggregate multiplies by it.

## 11.5 The generated bodies

Per aggregation the generator emits four functions. None of them allocates.

| Body | Called by | Contents |
|---|---|---|
| `hash_key` | the build side of every hash strategy | fused CRC32 over the key columns (document 10 §10.5) |
| `eq_key` | the probe loop of the local or global table | column-by-column compare; string keys compare the header before dereferencing |
| `update` | the pipeline body, inline | all aggregates of one group, straight-line, NULL checks removed where facts allow |
| `combine` | the merge driver | state-by-state merge: `+` for sums and counts, `min`/`max`, OR of seen bits |

`update` is inlined into the pipeline function. The other three are separate QIR functions that the precompiled drivers call through the state's function table, which keeps them callable from all tiers. This is the split the research notes recommend: generate only the update body, and keep both hash strategies precompiled (research-notes E §4.8).

What the inlined part looks like, for `SELECT l_orderkey, SUM(l_quantity), COUNT(*) FROM lineitem GROUP BY l_orderkey` on the thread-local strategy, with `l_orderkey` certified non-null and within `u32`:

```
; illustrative QIR; document 06 owns the syntax
body(i):   k     = load.u32 col_orderkey, i
           h     = crc32c.u32 SEED, k
           h     = mul.u64 h, MULT                     ; fold
           slot  = and.u64 (shr h, SHIFT), MASK        ; SHIFT, MASK from the table header
probe:     g     = addr local.rows, slot, ROW_BYTES    ; ROW_BYTES = 24, a constant
           gk    = load.u32 g, +0
           br    eq gk, k             -> hit
           br    eq gk, EMPTY_KEY     -> insert        ; EMPTY_KEY outside the certified range
           slot  = and.u64 (add slot, 1), MASK
           br    probe
insert:    br    ge local.count, local.limit -> flush  ; returns Status::NeedMemory(slot) at tuple i
           store.u32 g, +0, k ; store.i64 g, +8, 0 ; store.i64 g, +16, 0
           local.count = add local.count, 1
hit:       q     = load.i64 col_quantity_dec, i        ; DECIMAL(15,2) as scaled i64
           s     = load.i64 g, +8
           s     = add.checked.i64 s, q      -> deopt  ; guard: sum fits i64
           store.i64 g, +8, s
           c     = load.i64 g, +16 ; store.i64 g, +16, (add c, 1)
```

Four facts made this code short:

- The key is a `u32`, so hashing is a single CRC32.
- The key is certified non-null, so there is no NULL key path.
- The key range leaves room for an empty sentinel, so there is no separate occupancy byte.
- The sum is guarded rather than widened, so the state is 8 bytes and not 16.

Each fact has a guard or a certification behind it. When one fails, the morsel reruns on a variant generated without that fact (document 09).

## 11.6 Groupjoin

**When a join is followed by a `GROUP BY` on the join key, and the build side is unique on that key, the join table's entry carries the aggregate state.** The probe updates the state in place instead of emitting a join row. This is the groupjoin of Fent and Neumann (https://vldb.org/pvldb/vol14/p2383-fent.pdf), which applies to about one query in eight in TPC-H and TPC-DS. TPC-H Q13, for example, counts orders per customer with a left outer join.

- **Entry layout.** The document 10 §10.5 entry layout gains the state fields. A group with no probe match finalizes to the empty aggregate, which is correct for left-outer groupjoins: `COUNT` is 0 and `SUM` is `NULL`.
- **Parallel updates without atomics.** The probe side is partitioned by the same hash bits that partition the table, and each partition is updated by one task. The paper's own contention-free scheme was read only as a [snippet]. If it proves better, it replaces ours in C8.
- **With a `../graph/` link the hash table disappears.** When the key is a parent row id reached through a link, the groupjoin becomes a dense array indexed by parent row id. `../graph/05-execution.md` §5.6 gives the argument and the memory cost for Q18.

## 11.7 `COUNT(DISTINCT)` and `DISTINCT`

**A query with one distinct aggregate is rewritten into two aggregations.** The first groups by `(g, x)` to deduplicate. The second counts per `g`, and the query's other aggregates ride along as partial states in the first stage. A query with several distinct aggregates over different columns gets one deduplication table per distinct column, joined back on `g`. The compiler's contribution is the key encoding: a dictionary-coded or range-certified `x` makes the first stage a dense or perfect-hash aggregation (§11.4), and `(g, x)` is packed into one 64-bit key when the domains allow it.

**When the domain of `x` is dense and small per group, the deduplication table is a bitmap per group.** `COUNT(DISTINCT x)` is then a population count at finalize. The rule is the key bitmap rule of document 10 §10.4. Approximate distinct counting is never used for `COUNT(DISTINCT)`. `approx_count_distinct` is a separate function, and it runs as an opaque aggregate through the first engine.

**`SELECT DISTINCT` is aggregation with no aggregates.** The table is a key set. For a `DISTINCT` directly under `LIMIT` with no `ORDER BY`, the pipeline stops as soon as it has the limit's count of new keys.

## 11.8 Sort

**Generated key encoding, precompiled sort kernels.** DuckDB 1.4 already made this split (https://duckdb.org/2025/09/24/sorting-again):

- a normalized key built by `create_sort_key`;
- fixed-size key structs templated at compile time;
- vergesort for presorted runs, then ska sort (an MSD radix sort on the first 64-bit word), then pdqsort as a fallback;
- a parallel k-way merge path.

The rewrite measured 2.7x on 1B random integers, 10.4x on 1B ascending integers and 3.4x on TPC-H SF100 `lineitem` (600M rows), with 6.5x scaling at 8 threads against the old sort's 3.5x. It is about 30% slower single-threaded because of the in-place radix sort, and the gap disappears at two or more threads. The SF100 figure is 274 s before and 81 s after.

**Sort is where we expect parity-plus, not 10x, and the advantages are specific.** There are four.

1. **The encoder is fused into the producing pipeline.** The normalized key is written directly from registers by the pipeline that produces the rows, not by a separate pass over materialized chunks.
2. **Keys are compressed by facts.** A column certified in `[lo, hi]` is encoded as `v − lo` in `⌈log2(hi − lo + 1)⌉` bits, and several such columns are packed into one word. A three-column TPC-H `ORDER BY` over a date, a flag and a small integer can then fit in 8 bytes, which is one radix pass-set and one static compare. Static-size compares beat dynamic ones: 25% faster on average below 16 bytes, and static `memcpy` 55% faster on one CPU (https://duckdb.org/pdf/ICDE2023-kuiper-muehleisen-sorting.pdf).
3. **Only key and row id are sorted.** For payloads wider than 16 bytes, the sort carries a row id and the payload is gathered after sorting. This is the late materialization of document 10 §10.8, applied to sort.
4. **The kernels are monomorphized over key widths of 8, 16, 24 and 32 bytes plus row id**, precompiled in `rudb-qc-rt`, and selected per sort by the encoder's width. Wider keys use the 32-byte kernel on a prefix, with a generated tie-break.

**The encoding rules are DuckDB's, because the order must be DuckDB's:**

- a prefix byte for `NULL` placement per column, honoring `NULLS FIRST` or `NULLS LAST` and DuckDB's default;
- `DESC` by bit inversion;
- signed integers with the sign bit flipped;
- floats with DuckDB's total order, including `-0.0` and `NaN` placement;
- strings as a fixed prefix plus a "truncated" flag, with a generated tie-break comparator on the full strings when prefixes tie;
- collations by a `vcall` to the collation function before encoding.

Document 12 is the authority on each type's order, and document 15 diffs sorted output against the first engine byte for byte, ties included when the query's order is total.

**Stability.** SQL does not require it and DuckDB does not promise it. Where the `ORDER BY` is not total, we promise nothing beyond DuckDB. The differential harness compares such results as multisets within each group of equal keys.

## 11.9 Top-N

**`ORDER BY … LIMIT k` with a small `k` is a per-thread heap of `k` normalized keys, guarded by a register-held threshold.** The pipeline body encodes only the key's first word and compares it with the current k-th best word held in a register. Most tuples fail that one compare and are dropped before the rest of the key is encoded or any payload is touched. A tuple that passes is encoded in full and pushed into the heap by a precompiled heap kernel of the key's width, which updates the threshold. At the end, the per-thread heaps are merged in thread-index order.

- The heap path is used for `k` up to 4,096. Above that, a full sort with a limit on the merge wins. The value is our starting point and C9 tunes it on ClickBench.
- **The threshold is also a dynamic filter.** When the leading sort column has zone maps, the scan skips row groups whose range cannot beat the current threshold. This follows the zone-map rule of document 10 §10.4, so it can only ever reduce what is read.
- ClickBench's `GROUP BY … ORDER BY COUNT(*) DESC LIMIT 10` queries run top-N over the finalized aggregation, fused into the finalize task so the groups are never materialized as a sorted run.

## 11.10 Windows

**The primary sources we read are thin here, and this section says so.** The research notes did not verify any recent window-function paper. What follows is the standard structure described in Leis et al., "Efficient Processing of Window Functions in Analytical SQL Queries" (VLDB 2015, not re-read for this spec) and in DuckDB's own design. It is marked [background]. Windows arrive at C10, and until then the router sends queries with windows to the first engine.

**The split between generated and precompiled code:**

| Step | Generated | Precompiled |
|---|---|---|
| sort by `(PARTITION BY, ORDER BY)` | the §11.8 encoder, with a partition-boundary bit | the §11.8 kernels |
| partition and peer boundaries | a compare of key prefixes on adjacent rows | |
| frame bounds for `ROWS` and `RANGE` | offset arithmetic when the bounds are constants | binary search for `RANGE` over a value, `GROUPS` frames |
| ranking functions (`ROW_NUMBER`, `RANK`, `DENSE_RANK`, `NTILE`, `PERCENT_RANK`, `CUME_DIST`) | counters driven by the boundary flags | |
| `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, `NTH_VALUE` | index arithmetic and a gather | |
| running aggregates (`UNBOUNDED PRECEDING` to `CURRENT ROW`) | the §11.5 `update` body, reset at partition boundaries | |
| sliding invertible aggregates (`SUM`, `COUNT`, `AVG` over a moving frame) | `update` and an inverse, with an add-and-remove loop | |
| sliding non-invertible aggregates (`MIN`, `MAX`, and all floating-point sums) | the `combine` body | a segment tree built and queried per partition |

**Floating-point sums are treated as non-invertible.** Subtracting a value that left the frame gives a different result from recomputing, and DuckDB's result is what we must match. So a sliding `SUM(DOUBLE)` goes through the segment tree, and C10 checks that the segment tree's combine order matches DuckDB's for the frames TPC-DS uses. If it cannot be matched, the frame is recomputed per row. That is slower and exact, and it is the fallback we accept.

**Partitions are processed in parallel, one task per partition range.** A single huge partition is split by its sort order with the segment tree shared read-only.

## 11.11 Grouping sets, `ROLLUP`, `CUBE`

**The finest grouping set is aggregated from the input, and every coarser set is re-aggregated from the finest result.** The finest result is usually orders of magnitude smaller than the input. This is valid for every decomposable aggregate, meaning every inline aggregate except `COUNT(DISTINCT)`. A query with a distinct aggregate under grouping sets aggregates each set from the input, using the §11.7 rewrite per set.

The coarser sets reuse the generated `combine` body as their `update`, so a `ROLLUP` over four columns generates one key encoder and one combine and runs it four times over shrinking inputs. `GROUPING()` is a constant per set, emitted as a column. The keys a set rolls up are `NULL` in the output, and the NULL-tracking bits of §11.2 keep a rolled-up `NULL` distinct from a data `NULL` for `GROUPING()`. TPC-DS uses `ROLLUP` in several queries, which puts this at C10.

## 11.12 Set operations

- **`UNION ALL`** is not an operator in generated code. Both inputs feed the same sink as two pipelines.
- **`UNION`** is `UNION ALL` followed by `DISTINCT` (§11.7).
- **`INTERSECT`, `EXCEPT` and their `ALL` forms** are one aggregation keyed on the full row with two counters, `c_left` and `c_right`. Each input pipeline increments its own counter. Finalize emits:
  - `INTERSECT`: rows with both counters non-zero, once;
  - `INTERSECT ALL`: `min(c_left, c_right)` copies;
  - `EXCEPT`: rows with `c_left > 0` and `c_right = 0`, once;
  - `EXCEPT ALL`: `max(c_left − c_right, 0)` copies.
- **Set operations compare `NULL` equal to `NULL`,** so the generated `eq_key` for a set operation uses `IS NOT DISTINCT FROM` semantics. That is one flag in the key translator, and the verifier checks it is set for these tables and not for join tables.

## 11.13 Spilling and memory

**Generated code never spills.** It returns `NeedMemory(slot)` as a status (document 05 section 5.4.1), and the runtime decides whether that means grow, flush or spill:

- For aggregation, the partitions are already the spill unit. The runtime writes cold partitions through the buffer manager and merges them one at a time after the input is exhausted.
- For sort, runs are spilled and merged by the precompiled merge-path kernel.
- For windows and grouping sets, the input sort spills like any sort.

Memory for the local tables, heaps and key buffers is reserved at plan time from facts (document 03 §3.7), and an out-of-budget reservation is a planning decision, not a runtime surprise. The cost of spilling is document 13's to specify. This document only guarantees that no spill path runs through generated code, so that no tier has to be tested for it separately.

## 11.14 What C8, C9 and C10 measure

- **C8** is TPC-H coverage, aggregation strategies, sort and groupjoin. Its gate is instructions on TPC-H SF1 at no more than one tenth of DuckDB's, counted with hardware counters on the same machine. Instructions are chosen over time because they isolate the code the compiler emits from memory effects that document 10's staging handles separately. C8 also publishes:
  - the global-ticketing against partitioned comparison of §11.4, at group counts from 4 to 10^8 on both target machines;
  - a per-query table of which strategy each aggregation used, and which facts justified it.
- **C9** is the partial-acceptance bridge and ClickBench. The aggregation-heavy and top-N-heavy queries are where §11.4, §11.7 and §11.9 are measured. The top-N heap limit and the dynamic zone-map filter are tuned there.
- **C10** is TPC-DS, with at least 90 of 99 queries compiled. It brings windows, `ROLLUP`, grouping sets and set operations. Its gate for this document is a correctness one first: every window query diffs clean against the first engine, including float sums over sliding frames (§11.10), before any speed is reported.

Every strategy in this document has a switch in the style of document 10 §10.13, for example `SET qc_agg_strategy = 'auto' | 'partitioned' | 'ticketed' | 'dense'` and `SET qc_sort_key_pack = on | off`. Document 17's ablations can then attribute each strategy's share.

## What we should take from this document

Aggregation is where compilation already wins. Kersten measured 74% on Q1, and fused inline aggregation shows up in 97% of the bespoke engine's queries. The work here is not to invent a new algorithm but to keep the per-tuple update in registers and to let exact facts turn hash tables into arrays. That covers dense and perfect-hash keys from certified domains and dictionaries, and narrowed accumulators with a deopt guard for overflow.

The generated and precompiled split is uniform across the operators. Generated code covers the key encoding, the hash, the compare, `update` and `combine`. Precompiled code covers the drivers, partitioning, ticketing, merge, sort kernels, heaps, segment trees and spilling. Switching strategies at runtime (local, partitioned, ticketed) never needs a recompile, because every driver calls the same bodies.

Sort should reach parity with DuckDB 1.4's already-good design, and beat it through four specific advantages: the encoder fused into the producing pipeline, keys packed by certified ranges, sorting only key and row id, and width-monomorphized kernels. It is not where the 10x comes from, and it is honest to say so.

The numbers here that are our own are starting values for C8, C9 and C10 to tune: the dense-array domain limit of 2^16, the perfect-hash limit of 2^20, switching after four morsels, and the top-N heap limit of 4,096. Global ticketing is adopted only if C8 measures it winning, because its paper assumed perfect estimates.

Windows are the weakest-sourced part of this folder. They arrive at C10, and the segment-tree order for float sums is the one semantics question that must be settled against DuckDB before they ship.
