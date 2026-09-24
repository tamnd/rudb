# Joins

JOB first. How the compiled engine runs a many-way join: reduction compiled into scans, the hash table and the loops that probe it, when to stage and prefetch, how `MIN` outputs let most of the join never be enumerated, link joins over `../graph/`, semi, anti and mark joins, and compiled `LIKE`. Document 04 decides *which* of these a plan uses. This document specifies what each one is and what code it becomes.

## 10.1 What a JOB query costs, and why it is two problems at once

**JOB is probe-bound for the queries that take the time, and compile-bound for the queries that do not.** Both halves come from document 02 §2.2. DuckDB 1.3.2 runs the 113 queries in 55.3 s on one thread [fig] (https://arxiv.org/abs/2511.16455), about 0.49 s per query, and the target is a tenth of that: 5.5 s total, 49 ms per query on average. Umbra's execution is 7.756 s single-threaded, but on 32 threads its compile time (7.592 s) is 89% of end to end `[derived]`. A design that wins only the first half is Umbra, and a design that wins only the second is DuckDB.

Where DuckDB's time goes, from the research notes (`research-notes/C-joins-job.md` §1 to §2):

- **Oversized intermediates.** On 2a the worst join order produces 179x more intermediate tuples than the best, and 1.2x with Robust Predicate Transfer (https://arxiv.org/abs/2502.15181). On 8d, injecting true cardinalities takes DuckDB 0.10.1 from 2.7 s to 0.8 s [fig] (https://arxiv.org/abs/2511.16455).
- **Probes into tables that miss the cache.** `cast_info` has about 36M rows. `movie_keyword`, `movie_info` and `movie_companies` are the other large fact-like tables, and each of them is probed or built per query.
- **Fan-out that the query never needs.** Every JOB output is wrapped in `MIN`. A title with 200 cast entries and 40 keywords produces 8,000 join rows whose only use is to feed a `MIN` that 1 row would have fed equally well.
- **String predicates over large text columns.** Examples are `mc.note LIKE '%(200%)%'` (9b, 15a, 15b, 19b, 22a) and `(mi.info LIKE 'USA:% 199%' OR mi.info LIKE 'USA:% 200%')` (15c). These run over millions of rows before any join can shrink them.

**Each of these has a mechanism in this document, and each mechanism has a measured factor in document 02 §2.9.** Reduction is 1.5x, layout 1.5x, fused pipelines 1.5x, staged probes and the hash table 2x, strings 1.5x. The rest of this document is those five, in the order a query meets them.

## 10.2 One query, all the way down

JOB 6a, because the research notes give it an anchor number:

```sql
SELECT MIN(k.keyword) AS movie_keyword,
       MIN(n.name)    AS actor_name,
       MIN(t.title)   AS marvel_movie
FROM cast_info AS ci, keyword AS k, movie_keyword AS mk, name AS n, title AS t
WHERE k.keyword = 'marvel-cinematic-universe'
  AND n.name LIKE '%Downey%Robert%'
  AND t.production_year > 2010
  AND k.id = mk.keyword_id
  AND t.id = mk.movie_id
  AND t.id = ci.movie_id
  AND ci.movie_id = mk.movie_id
  AND n.id = ci.person_id;
```

Semi-join reduction shrinks `cast_info` from 36M rows to 486 on this query (TreeTracker, https://arxiv.org/abs/2403.01631). That ratio is the whole story of JOB. A binary hash-join plan that does not reduce reads 36M `cast_info` rows and probes with most of them. A plan that reduces touches each of them once, with a test that is one or two instructions, and then does real work on 486.

The join graph has one equivalence class, `movie_id` = {`t.id`, `mk.movie_id`, `ci.movie_id`}, and two plain edges, `k.id = mk.keyword_id` and `n.id = ci.person_id`. As a hypergraph it is alpha-acyclic, like every JOB query. What the compiled engine runs, assuming the facts that the IMDB keys are dense integer ids (certified by `../stats/`):

| Pipeline | Work | Output |
|---|---|---|
| P1 | scan `keyword` with `keyword = '…'`, evaluated once per dictionary entry | exact key bitmap `K` over `k.id` (one bit set) |
| P2 | scan `title` with `production_year > 2010` | exact key bitmap `T` over `t.id` |
| P3 | scan `name` with compiled `LIKE '%Downey%Robert%'` | exact key bitmap `N` over `n.id` |
| P4 | scan `movie_keyword`, filter `K[keyword_id] ∧ T[movie_id]` | key bitmap `M` over `movie_id` |
| P5 | scan `cast_info`, filter `M[movie_id] ∧ N[person_id]` | the 486-tuple survivors, folded into `MIN` state, plus bitmaps back up the tree |
| P6 | top-down: re-filter the survivors of P1 to P3 by the bitmaps from P5, fold `MIN(k.keyword)`, `MIN(n.name)`, `MIN(t.title)` | one row |

No hash table is built. Each scan's filters are bit tests compiled into its batch loop (§10.4), and P5 is where 36M tuples are tested. P6 fetches strings for a handful of row ids (§10.8). Section 10.3 explains why this plan gives the exact answer.

## 10.3 The `MIN` plan: reduce, then fold

**When a query's result is one ungrouped row of `MIN` and `MAX` aggregates, each over a column of one relation, and the join is an alpha-acyclic equi-join with only per-relation filters, the join result is never built.** The planner emits a full semi-join reduction followed by one fold per relation. Document 04 names this plan shape `reduce_fold`. It is the plan shape JOB was not designed for and happens to fit.

**Why it is exact.** After a full reducer (the bottom-up and top-down semi-join passes of Yannakakis' algorithm over a join tree), every surviving tuple of every relation takes part in at least one row of the join result. This is the defining property of full reduction on acyclic queries. `MIN(r.c)` over the join result is the minimum of `r.c` over the tuples of `r` that appear in some result row, because `MIN` ignores how many times a value appears. So it equals `MIN(r.c)` over the reduced `r`. The same holds for `MAX`, and for `BOOL_AND`, `BOOL_OR` and `COUNT(DISTINCT r.c)` restricted to one relation. It does not hold for `COUNT(*)`, `SUM`, `AVG`, or any aggregate over an expression that mixes relations. Those need the join, or the counting variant of Yannakakis, which this document does not specify. It does not hold for `ANY_VALUE`, because DuckDB's choice of value depends on order and we will not promise to reproduce it.

**Preconditions, checked by the A4 verifier and recorded in `EXPLAIN`:**

1. The root is an ungrouped aggregate whose every aggregate qualifies as above.
2. Every join predicate is an equality between columns, and every other predicate references one relation. An `OR` across relations disqualifies the query.
3. The equivalence-class hypergraph is alpha-acyclic. The join tree is built by GYO reduction at plan time, which costs microseconds for 17 relations.
4. The semi-joins are exact. Approximate filters (§10.4) are allowed only as pre-filters in front of an exact test.

**The cost, stated honestly.** Full reduction does two passes over the tree: up to `2(k−1)` semi-joins for `k` relations, where a binary plan does `k−1` joins. SYA reports that classic full Yannakakis is about 5x slower than binary joins on JOB (https://arxiv.org/abs/2411.04042). That is the reason naive Yannakakis lost historically. Our variant differs in three ways that each remove part of that cost:

- There is no third, join phase.
- The top-down pass visits only the relations that own a `MIN` output. On 6a that is three of five.
- Each semi-join is a bit test compiled into a scan that runs anyway, not a separate hash build and probe.

Whether those three are enough is not known. Yannakakis+ reports a mean of 1.42x and a maximum of 14.84x on JOB (https://arxiv.org/abs/2504.03279). SYA reports a worst slowdown of 1.3x (https://arxiv.org/abs/2411.04042). Neither ran compiled bit tests.

**The decision rule.** `reduce_fold` is a candidate that document 04 costs against the binary plan with the same C_mm model, which counts tuples produced. The binary plan also gets the reduction filters of §10.4. C6 in document 18 runs every JOB query both ways under `SET qc_join_plan = 'binary' | 'reduce_fold' | 'auto'`, and the result goes into document 17. If `auto` loses to `binary` on any query by more than measurement noise, that is a planner bug with a query number attached.

**How many JOB queries qualify is measured at C1, not assumed here.** The research notes say all 113 are alpha-acyclic select-join blocks wrapped in `MIN`. Precondition 2 is the one that may exclude some of them.

## 10.4 Reduction, compiled into the scan

**Every reduction filter is a batch-level stage between the precompiled scan kernel and the generated tuple body, and runs at one to three instructions per tuple.** Document 03 §3.7 fixes the shape. The scan kernel hands up batches of 1,024 tuples with a selection vector. Each filter stage refines the selection vector. Then the tuple-at-a-time body runs on the survivors. The filters live at batch level for three reasons: the order can change at runtime without recompiling (the permutable approach, https://www.vldb.org/pvldb/vol14/p101-menon.pdf); they can be dropped; and SIMD selection pays there (8.4x dense and 1.4x end to end on Q6, https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf).

**The filter menu, in cascade order.** The cheapest applicable test goes first. An exact test is always preferred to an approximate one of similar cost.

| Filter | Built from | Exact | Cost per tuple | Used when |
|---|---|---|---|---|
| min-max range | the build side's exact key min and max | no | 2 compares, often none (zone map) | always, and fed to row-group skipping |
| IN-list | build with ≤ 50 distinct keys | yes | SIMD compare over ≤ 50 values | ≤ 50 distinct keys, matching DuckDB 1.2's SIP threshold |
| key bitmap | build keys, when the key domain is dense | yes | 1 shift, 1 load, 1 bit test | the domain range fits the bitmap budget below |
| row-id bitmap | a `../graph/` link and the parent's selection | yes | 1 link read, 1 bit test | a verified relationship with a built link (`../graph/05-execution.md` §5.4) |
| CSBF | build keys | no | about 2.48 cycles (https://people.iiis.tsinghua.edu.cn/~huanchen/publications/rpt+-vldb26.pdf) | the key domain is sparse and the build is large |
| exact key-set probe | the unchained table of §10.5, keys only | yes | one directory load plus a tag test on most misses | an exact semi-join is required and nothing cheaper is exact |

**The key bitmap is the workhorse on JOB, and it is ours rather than the literature's.** IMDB's primary keys are dense serial integers. When `../stats/` certifies that a key column's values lie in `[lo, hi]`, a set of build keys becomes a bitmap of `hi − lo + 1` bits. Membership is then `bits[(k − lo) >> 6] >> ((k − lo) & 63) & 1`: exact, branch-free, and one cache line per 512 keys.

The budget rule for a key bitmap:

- **Use it** when the bitmap fits in the per-core L2, or when it costs at most 64 bits per build key.
- **Never** when it exceeds 64 MiB.

These are starting values, and C6 tunes them on M4 and on c6a.4xlarge. The RPT+ CSBF spends 20 bits per key for an approximate answer. A bitmap that spends up to 64 bits per key for an exact one, with no hashing, is the better trade whenever the domain allows it. We have not seen this comparison published, so it is a prediction for C6 to check.

**Adaptive keep and drop.** Every filter stage keeps two counters in the thread's pipeline state: tuples tested and tuples passed. At each morsel boundary the runtime folds the counters. Once a filter has tested at least 100,000 tuples (the RPT+ sample size), the runtime applies one of two rules:

- **Expensive filters** (CSBF, key-set probe) are kept only if their pass rate is below 0.35, which is RPT+'s τ_sel.
- **Cheap exact filters** (min-max, IN-list, key bitmap, row-id bitmap) are dropped only if their pass rate is above 0.95. They cost about one instruction, so they almost always pay.

The 0.95 is ours and C6 tunes it. For comparison, Parachute's probe-side filter disables itself when it filters out less than 60% after 4,000 rows (https://arxiv.org/abs/2506.13670). In `reduce_fold`, the exact filter on an edge is the semi-join itself, so it is never dropped. Only an approximate pre-filter in front of it can be.

**Ordering.** Filters are ranked by `(1 − pass_rate) / cost_per_tuple`, highest first, and re-ranked at morsel boundaries. The cost is a per-kind constant calibrated once per machine, not measured per query. Before any counters exist, the order is:

1. min-max
2. IN-list
3. local numeric predicates and `IS [NOT] NULL`
4. bitmaps
5. CSBF
6. key-set probes
7. `LIKE`, last

This is the LIP recipe, which cut one SSB query's plan-to-plan spread from 2.1 to 58 s to 1.3 to 7.4 s (https://www.vldb.org/pvldb/vol10/p889-zhu.pdf).

**The zone-map rule.** No reduction filter ever causes a row group to be read that the local predicates alone would have skipped. The scan evaluates zone maps against the local predicates first, then intersects with the min-max ranges of the transferred filters, and never the reverse. RPT+ regressed on JOB templates 8, 10 and 24 exactly where transfer broke row-group skipping. The rule is checked in C6 by counting row groups read with and without reduction on every JOB query. The count with reduction must never be higher.

**Filter accuracy matters twice.** A false positive in a Bloom filter admits a tuple, and it also widens the min-max range that the next filter down the tree is built from. RPT+ measured 4x on JOB 07c from accuracy alone. That is why the menu prefers exact tests, and why the CSBF is configured as RPT+'s: 64-byte blocks, 32-bit sectors, 20 bits per key, k = 7, FPR 6.1e-5.

**Determinism.** Dropping and reordering filters depends on which morsels a thread saw first, so it is timing-dependent. It is allowed for the same reason backend choice is (document 03 §3.8): a filter only removes tuples that the exact join would also remove, so no result depends on whether it ran. The differential harness in document 15 runs with adaptivity off and with it forced on to hold that line.

## 10.5 The hash table

**One join hash table: the unchained table of Birler et al.**, with a per-query generated entry layout. Sources: https://db.in.tum.de/~birler/papers/hashtable.pdf and https://cedardb.com/blog/simple_efficient_hash_tables/.

```
directory:  u64[2^d]        bits 0..47  = address of the slot's first entry
                            bits 48..63 = 16-bit Bloom tag (OR of member tags)
entries:    [Entry; n]      contiguous; all entries of slot s lie in
                            [addr(dir[s]), addr(dir[s+1]))
tag table:  u16[2048]       4 of 16 bits set per entry; 4 KB, L1-resident
```

A probe does one directory load, one AND and compare against the tag in the same word, and on a hit a linear scan of a contiguous range. There is no chain pointer and no second random load for a miss. The published results:

- The FPR of the tag is about 1/169 at load factor 0.65.
- About 2x over Robin Hood open addressing on join-heavy work, validated on 10,312 queries.
- Up to 30% slower on tiny queries, which §10.6 routes elsewhere.

The contiguous range per slot is also what makes Lookup and Expand separable (§10.8).

**Hashing.** CRC32C plus a multiply fold, which is document 03 §3.6's list: one CRC32 for keys up to 32 bits, two chained for 64-bit keys, as in Umbra. CRC32C is an instruction on both AArch64 (`crc32cx`) and x86-64 (`crc32` with a 64-bit operand), so the hash is the same on both ISAs and on the interpreter. The tag index and the tag bits come from hash bits not used for the slot. Hashing is emitted as fused per-key code, about 4 instructions for an `(int32, int32)` key (research-notes E §4.6).

**The entry layout is generated per query.** Document 07 owns the translator, and this document owns the rule. An entry holds three things:

- **The key columns, narrowed by exact facts.** A `movie_id` certified in `[1, 2^31)` is a `u32` even though its SQL type is `BIGINT`.
- **The payload columns the probe side's downstream actually reads.** Under late materialization (§10.8) this is usually a row id, not the payload.
- **A NULL bitmap byte, only if some payload column is nullable.** NULL keys never enter an equi-join table.

There is no hash field for integer keys, because recomputing is cheaper than loading. String keys store the full hash and the 16-byte string header, so most inequalities are decided without dereferencing the string. Entries are packed with no padding beyond natural alignment. A `(u32 key, u32 rid)` entry is 8 bytes, which puts 8 entries in a cache line.

**Build.** Every build pipeline's generated code does exactly one thing: append `(hash, entry)` into a thread-local partition chosen by the high hash bits. The partitions are bump-allocated from memory the runtime reserved at plan time, as document 03 §3.7 requires. The finalize step is precompiled runtime code in `rudb-qc-rt`, run in parallel with one partition per task. It counts per slot, prefix-sums, scatters entries contiguously, and writes the directory words and tags. There are no atomics, because each partition owns a disjoint directory range. Huge pages are used where the OS gives them. The directory size is a power of two chosen from the exact build count after the build, never from the estimate.

**Every build publishes its exact facts to the probe side:** row count, distinct-key count if it is at most 50, key min and max, and table bytes. §10.6 selects on them.

## 10.6 Probe variants, and the rule that picks one

**The variant is chosen at probe-pipeline start from the build's exact facts, and only the chosen variant is generated.** A probe pipeline cannot start before its builds finalize, and document 09's Rule I3 compiles each pipeline lazily when it becomes runnable. So by the time the probe's QIR is generated, the build's row count, distinct keys, key range and table bytes are exact, and there is nothing left to hedge against. Generating one variant rather than three keeps per-query code volume down, which is document 20's Q1 risk. The variant is part of the code cache key (document 09), so a repeated query with the same build facts reuses the code. Research-notes C implication 21 asks that the choice come from runtime facts and not from estimates, and this satisfies it more strictly than carrying several variants would.

| Variant | Selected when (exact facts) | Probe cost |
|---|---|---|
| empty | build count = 0 | inner or semi: the probe pipeline is skipped; anti: passes everything |
| tiny | distinct keys ≤ 50 | SIMD compare of the key against the keys in a small array, payload in a parallel array; the same list is also the IN-list filter of §10.4 |
| dense array | key range ≤ 4 × build count and ≤ 2^24 | `slot = key − lo`; a `u32` offset array as CSR for duplicates, with no hashing or tags |
| fused unchained | table bytes ≤ LLC / 2 / active threads | tuple-at-a-time: hash, directory load, tag test, range scan; relies on OoO (Birler) |
| staged unchained | otherwise | §10.7: hash a batch, prefetch, test tags in groups, then compare |

**The tiny and dense variants exist because the unchained table's partitioned build is a fixed cost that loses up to 30% on tiny queries.** JOB is full of dimension tables with a few to a few hundred rows (`kind_type`, `info_type`, `company_type`, `role_type`). The dense-array variant is the join-side twin of the key bitmap in §10.4: when keys are dense serial ids, the "hash table" is an array indexed by key.

**The LLC threshold is a starting value.** The evidence has two sides:

- Birler et al. found OoO execution enough when the tag sits in the directory word.
- Psaropoulos et al. measured group prefetching at 2.7 to 3.7x over a naive probe for tables that miss the cache (https://doi.org/10.14778/3149193.3149202).
- ROF measured up to 2.2x over pure fusion (http://www.vldb.org/pvldb/vol11/p1-menon.pdf), and warns that boundaries placed from estimates land in the wrong place.

So the boundary is placed from exact sizes at runtime, and C6 sweeps the threshold on both target machines.

## 10.7 The staged probe loop

ROF's design, adapted to morsels. The tuple-at-a-time body is split at the probe into three stages, separated by vectors of tuple indexes that live in cache. Each stage is a loop over a batch of up to 1,024 survivors.

```
; illustrative QIR; document 06 owns the syntax
stage hash:      for i in sel[0..n]:  h[i] = crc_hash(key[sel[i]])
                                       prefetch dir[h[i] >> shift]
stage tag:       for g in groups of G over 0..n:
                   for i in g:        w = load dir[h[i] >> shift]
                                       t = tag_tab[h[i] & 2047]
                                       if (w & t) == t:
                                           cand[m] = i; lo[m] = addr(w); m += 1
                                           prefetch lo[m-1]
stage match:     for j in 0..m:       for e in lo[j] .. addr(dir[slot(cand[j]) + 1]):
                                           if e.key == key[sel[cand[j]]]: emit (cand[j], e)
```

**Group size `G` = 16 by default.** ROF measured its best at 16 even though the CPU allows about 10 outstanding L1 misses. Psaropoulos found group prefetching best at about 10. The value is a per-machine constant set by a calibration microbenchmark in C6, which tries 8, 16 and 32, and it is never chosen per query.

**Multi-join pipelines stage each probe separately.** A JOB probe pipeline that passes through four tables has a stage boundary in front of each table that selected the staged variant, and none in front of the others. The downstream body after the last probe is tuple-at-a-time again. The `emit` of the match stage writes `(index, entry address)` pairs, so Expand (§10.8) can read payload or defer it.

**Semi and anti probes stop at the first match.** A key-set table used only for a semi-join stores keys only, and its match stage returns a bit.

## 10.8 Lookup, Expand, and late materialization

**Lookup and Expand are separate QIR operations** (the diamond split, https://db.in.tum.de/people/sites/birler/papers/diamond.pdf).

- `lookup` returns a match range `[lo, hi)`. An empty range means no match.
- `expand` enumerates the range.

Between them, a pipeline may run further lookups and filters on the probe tuple with the range held unexpanded. The n:m fan-out of `cast_info`, `movie_keyword` and `movie_info` per title is then enumerated only for tuples that survive everything after it. Diamond found the contiguous-match-group table to be the only change that helped noticeably on JOB. The rest of its machinery (Expand3, the ternary operator for diamond patterns) targets cyclic graph queries and is not in C6.

**Late materialization is the default.** Joins carry keys and row ids, never strings. A payload column is fetched by row id at the first operator that reads its value, which for JOB is the final `MIN`. The fetch is a gather through the scan's decode path, per surviving row id. On 6a it touches a few hundred rows of `name` and `title` instead of carrying names through a 36M-row probe. No paper isolates this on JOB (research-notes C §5.1). It is measured on its own in C6 with `SET qc_late_mat = off`.

**`MIN` over fan-out without enumerating it.** When a binary plan is chosen but the output is still `MIN`-only (a query that failed §10.3's precondition 2), the aggregate is pushed below each n:m join whose other side contributes nothing but its `MIN` columns. That side is pre-aggregated per key into one entry, which turns n:m into n:1. This is eager aggregation, legal because `MIN` and `MAX` do not count duplicates. `SUM` and `COUNT` would need the multiplicity carried, which document 11 §11.8 covers.

## 10.9 Link joins over `../graph/`

When `child.fk = parent.pk` has a verified relationship with a built forward link, and the child reaches the join with its row id intact, the join is a gather with no hash table (`../graph/05-execution.md` §5.2). In generated code it is:

```
; per child tuple i, inside the fused body
prid  = load_link  link_col, i            ; bit-packed read, unpacked by the scan kernel
skip  if prid == NO_PARENT                ; inner: drop; left: gather NULL
skip  if !bit_test parent_sel, prid       ; the exact row-id reduction of §10.4
v     = gather parent.col, prid           ; only for columns read downstream (§10.8)
```

This is the index-nested-loop join the JOB paper measured against hash joins. In main memory, hash joins beat it by at most 5x in PostgreSQL and 2x in HyPer (https://www.vldb.org/pvldb/vol9/p204-leis.pdf). A link is not an index probe, though. It is one sequential read of a column the scan decodes anyway, followed by a random gather only for survivors. The planner prefers it whenever the parent-side selection is available as a bitmap. Semi- and anti-joins over a link never touch the parent's columns.

**The `rid` rule from `../graph/` binds the code generator too.** A row id is used only where the physical plan declares it still valid. The QIR verifier in document 06 rejects a `gather` whose row-id operand is not tagged with the table it indexes.

## 10.10 Semi, anti and mark joins, and `NOT IN`

**The mark join is a QIR operation from C1** (research-notes C implication 18). The IN, NOT IN, EXISTS and scalar subqueries that the shared unnesting leaves as mark joins (document 03 §3.2, guarantee 1) run in linear time in the common case. A mark join emits a three-valued column, `TRUE`, `FALSE` or `NULL`, per probe tuple.

For a single nullable attribute, `x IN (SELECT y …)`:

| Condition | Mark |
|---|---|
| some `y = x` | `TRUE` |
| no match, and (`x` is `NULL` and the build is non-empty) or the build contains a `NULL` `y` | `NULL` |
| otherwise | `FALSE` |

The build publishes two exact facts, `has_null_key` and `count`, so the second row costs one load per pipeline, not per tuple. `NOT IN` is `NOT mark`, and a filter keeps only `TRUE`. For several nullable attributes the general problem has no subquadratic algorithm unless SETH-style assumptions fail (https://www.vldb.org/cidrdb/papers/2026/p3-birler.pdf). We implement the paper's linear algorithms for the cases it covers and a counted nested-loop fallback for the rest, and `EXPLAIN` names the fallback.

**The compatibility decision.** The same paper lists DuckDB 1.3.0 among the systems that return wrong results on nullable `IN` and `NOT IN`. Our rule is:

- **Match DuckDB wherever DuckDB agrees with SQL semantics.**
- **Where a DuckDB result is a documented bug, both rudb engines return the SQL-correct answer.** `rudb-compat` records each case as a known divergence, with the query, DuckDB's version and the correct result.
- **Each divergence is re-tested against every new DuckDB release**, and removed when DuckDB fixes it.

The compiled engine never diverges from the first engine. If the first engine is bug-compatible, that is a first-engine bug, found by the tier-against-engine diff in document 15.

## 10.11 Compiled `LIKE`

JOB's string predicates run over millions of rows before any reduction applies, so they sit on the critical path of the scans in §10.4. When reduction removes most of the join cost, they become a larger share of what remains (research-notes C §7.1). This is the C7 work. Document 12 owns collation and UTF-8 semantics, and this section owns the matching plan.

**The pattern is compiled at code-generation time into a matcher plan.** The design is "Teach Your DBMS to LIKE Strings" (https://arxiv.org/abs/2608.23307), which measured a compiled filter at 13.3x over DuckDB, and ADMS 2023, which measured 2.5x over interpreted LIKE in Umbra (https://db.in.tum.de/~riedl/papers/like-codegen.pdf).

1. **Split on `%`.** Leading and trailing `_` become fixed skips. The longest `_`-free piece anchors the search.
2. **Pre-checks in generated code, inside the 16-byte string header:** minimum length, the literal prefix against the 4-byte inline prefix (all 12 bytes when the string is inlined), and the literal suffix when the string is inlined. For `'USA:% 199%'` the prefix check rejects most rows without touching string memory.
3. **Segment search in precompiled, monomorphized runtime kernels,** with parameters precomputed into the pipeline's constant pool:
   - short segments: SIMD first-and-last-byte compare;
   - medium segments: Boyer-Moore;
   - long segments: Two-Way.

   This follows document 03 §3.6: comparisons that stay in the header are generated, and anything longer is a call.
4. **`OR` of `LIKE`s over one column is fused.** 15c and 19a share a prefix (`'USA:'`, or none), so one pre-check is followed by one multi-segment search. For more than 8 patterns an Aho-Corasick automaton replaces them all. The same paper measured Aho-Corasick at up to 30.6x over DuckDB v1.4.4 on wildcard joins.
5. **Dictionary columns evaluate the pattern once per entry.** Low-cardinality columns (`keyword.keyword`, `kind_type.kind`, `info_type.info`, `company_type.kind`) evaluate the pattern once per dictionary entry into a code bitmap. The filter then becomes the one-instruction bit test of §10.4. High-cardinality columns like `movie_info.info` gain little from this (research-notes C §7.6), and matching in the FSST domain (2.5 to 17x over decompress-then-match [snippet], DaMoN 2026) is evaluated at C7 only if storage keeps those columns in FSST.

**The honest gap.** The 13.3x was measured with the whole matcher generated. Ours generates only the pre-checks and calls kernels for the search. C7 measures both splits on JOB's patterns. If generating the segment search is worth more than its compile cost, it moves into generated code with a byte-size cap.

## 10.12 What we do not do, and TPC-DS star joins

- **No worst-case-optimal joins by default.** Pure WCOJ is about 25x slower than binary joins on JOB in Umbra (Diamond). Free Join's 2.94x geomean over circa-2023 DuckDB (https://arxiv.org/abs/2301.10841) came with a minimum of 0.85x. A hybrid that fires only where binary joins would grow intermediates had 5 false negatives out of 923 joins (https://doi.org/10.14778/3407790.3407797). JOB is acyclic and `reduce_fold` covers what WCOJ would buy there. The multiway intersect of `../graph/05-execution.md` §5.8 is for cyclic queries, is not in C0 to C12, and is listed in document 20.
- **No radix-partitioned probe.** In a code-generating engine a radix join rarely pays for its code and optimizer complexity (Bandle et al., https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf [snippet]). We partition the build, as §10.5 does, and never the probe.
- **No SIMD hash probe.** The research found no 2025 to 2026 result where SIMD probing beats the scalar unchained probe in a real system. SIMD is used in the filter stages of §10.4, where it is measured to pay.
- **Star joins get fused probe chains with all dimension filters in the fact scan.** TPC-DS and SSB queries join one fact table to several dimensions. The compiled form is one pipeline: the fact scan applies every dimension's reduction filter as a §10.4 stage, then the survivors probe each dimension table in turn, staged or fused per §10.6. This is LIP (geomean 4.0x) and SQL Server's bitmap cascade (up to 3.47x on TPC-H SF100, https://www.vldb.org/cidrdb/papers/2026/p29-zhao.pdf) expressed as scan stages.

## 10.13 The JOB budget, and what C6 and C7 measure

**The per-query arithmetic.** 49 ms on average per query, single-threaded, end to end. Gate G1 takes at most 1 ms of that at the median. The remaining 48 ms is execution, and it has to absorb DuckDB's roughly 490 ms average divided by the four engine factors of document 02 §2.9 (layout, fusion, probes, strings), which multiply to 6.75x. The other 1.5x must come from the plan `[derived]`. The prediction of where the 48 ms goes, which document 17 checks per query:

| Component | Share of execution, predicted | Mechanism |
|---|---|---|
| scans of the large tables with reduction filters | about half | §10.4, bit tests at batch level |
| string predicates | about a fifth | §10.11 |
| probes and expands into non-tiny tables | about a fifth | §10.5 to §10.8 |
| builds, finalize, final gathers | the rest | §10.5, §10.8 |

These shares are a hypothesis. JOB totals are dominated by a few templates (8d, 13a, 16b, 17e among them), so the per-query shape matters more than the average.

**What each gate measures.**

- **C6: JOB at 5x DuckDB single-threaded**, which is at most 11.06 s total against 1.3.2 `[derived]`. No query may be slower than DuckDB. Every mechanism in §10.3 to §10.8 has a switch, and C6 publishes a per-query ablation table that turns each one off in turn. C6 also runs the tuning sweeps named above: the bitmap budget, the 0.95 drop threshold, the LLC threshold and the group size.
- **C7: JOB at 10x single-threaded**, which is at most 5.53 s total `[derived]`, with compiled `LIKE`, dictionary predicates and the fused `OR`. C7 re-runs the ablation, because the factors are not independent (document 02 §2.9, caution 1).
- **Both gates are run against the current DuckDB release, not the paper's version.** DuckDB 1.5.x already pushes join Bloom filters into probe scans (duckdb/duckdb#19502 [snippet]), and research-notes C expects it to absorb about 1.2 to 1.5x of the reduction gains. If that happens, the engine factors have to make up the difference, and the table above says where they would.

## What we should take from this document

JOB is won by never doing the work, not by doing it faster. The 36M-to-486 ratio on 6a is typical of the benchmark's shape. Exact reduction compiled into scans at one to three instructions per tuple, together with the `MIN` rule that lets a fully reduced relation be folded without building the join, removes most of the work a binary plan does. Whether `reduce_fold` beats a binary plan with the same filters is an open measurement, and SYA's 5x warning is why it stays a costed choice rather than a rule.

Exactness is worth more than cleverness. Key bitmaps over dense ids and row-id bitmaps over links are exact, branch-free, and cheaper than any Bloom filter. The approximate filters are a fallback for sparse domains, and accuracy matters twice because false positives widen the ranges that later filters are built from.

The hash table is the unchained design, with a generated entry layout narrowed by facts. The probe variant (empty, tiny, dense, fused, staged) is picked at probe-pipeline start from exact build facts, as a runtime parameter of already-generated code. Staging with a group size of 16 is used only when the table misses the cache.

Adaptivity is allowed wherever it cannot change a result. Filters are kept, dropped and reordered at morsel boundaries using RPT+'s sample size and threshold for expensive filters and a near-1 threshold for cheap exact ones. Row-group skipping is never made worse, and C6 counts row groups to check.

Semantics are decided, not inherited. The mark join exists from C1. On a documented DuckDB bug both engines return the SQL-correct answer and `rudb-compat` records the divergence. The compiled engine and the first engine never disagree.
