# The physical plan

Document 03 put the boundary between the two engines at A4, the rewritten logical plan. Everything from A5 onwards belongs to the compiled engine. This document specifies A5, the physical plan: which operators exist, how each algorithm is chosen, how the reduction schedule is built, what representation every column travels in, and which decisions are made at plan time and which are left to the running query. Document 05 cuts A5 into pipelines (A6). Documents 06 onwards turn pipelines into code.

The physical plan is where most of JOB's time is won or lost, and none of that happens in the code generator. RPT reports a worst/best join-order spread of 179x on JOB 2a that collapses to 1.2x once the input relations are reduced first (arXiv 2502.15181, https://arxiv.org/abs/2502.15181). No amount of better machine code closes a 179x gap. A 10x target over DuckDB on JOB is a target for this document first and for the backends second.

Crate: `rudb-qc-plan`. Input: A4 plus the fact interface of `../planner-v2/04-facts-not-estimates.md`. Output: a `PhysPlan` value, printable with `EXPLAIN (CODEGEN, PHYSICAL)`. Budget: physical planning plus the pipeline split must fit in 0.1 ms median and 0.5 ms max (document 02, gate G1).

## 4.1 What the physical plan is for

**A5 is a tree of physical operators annotated with representation, reservations and guards. It is deterministic.** Given the same A4 and the same facts it produces byte-identical output. It never reads history: no execution feedback, no plan cache statistics, no timing. Adaptivity still exists, but it lives in explicitly named runtime decision points (section 4.9) that are part of the plan text. It never lives in a planner that quietly learns.

The planner has three jobs, in this order:

1. **Choose algorithms** for each logical operator from facts (4.3).
2. **Build the reduction schedule**, the set of filters passed between relations before and during the joins (4.4).
3. **Propagate representation**, deciding for every column at every edge whether it travels as a dictionary code, a decoded value, or a row id to be fetched later (4.6).

Build-side choice (4.5), memory reservations (4.8) and guard placement (4.7) fall out of those three.

**The join order is an input, not an output.** Document 03 settled this. `rudb-opt` produces the join order in A4. The compiled planner may change it only by a named physical transformation, printed as a diff in `EXPLAIN`. There are two such transformations, both in 4.5: *flip*, which swaps build and probe, and *link-rotate*, which re-roots a chain when a stored link makes one direction nearly free.

## 4.2 The operator set

**Twenty-two operators, each a pipeline source, a pipeline sink, or both.** Every operator states its role up front, because document 05's decomposition is mechanical and relies on it. An operator that is both a sink and a source is a *breaker*.

| Operator | Role | Notes |
|---|---|---|
| `Scan{table, cols, rep, pred, filters}` | source | Zone-map skip, pushed predicate, attached reduction filters (4.4). Emits codes, values or row ids per column. |
| `IndexScan{table, key_range}` | source | Range over a sorted or clustered column (OLTP, C11). |
| `RowIdFetch{table, cols}` | inline | Late materialization: fetch columns for surviving row ids (4.6). |
| `Filter{pred}` | inline | Residual predicates that could not be pushed into a scan. |
| `Project{exprs}` | inline | Computes expressions. Usually fused away. |
| `HashBuild{keys, payload, ht_kind}` | sink | Fills a join hash table (unchained by default). |
| `HashLookup{ht, keys}` | inline | Probe. Yields a match range, not tuples (Diamond split). |
| `Expand{range, payload}` | inline | Iterates the match range. Omitted when the join is N:1. |
| `LinkJoin{link, dir}` | inline | Index nested-loop over a stored link from `../graph`. No hash table. |
| `SemiJoin` / `AntiJoin` | inline | `HashLookup` with existence only. No `Expand`. |
| `MarkJoin{null_mode}` | inline | Tri-state mark for `IN` / `NOT IN` / `EXISTS` under NULLs (Birler & Neumann). |
| `GroupJoin{keys, aggs}` | sink + source | Join and aggregate on the same key in one table (Fent & Neumann). |
| `HashAgg{keys, aggs, strategy}` | sink + source | Strategies in 4.3.3. |
| `DenseAgg{key_domain, aggs}` | sink + source | Array indexed by a dictionary code or small integer domain. |
| `ScalarAgg{aggs}` | sink + source | No group keys. JOB's `MIN(...)` outputs. |
| `Sort{keys}` | sink + source | Generated key encoder plus precompiled kernels (document 11). |
| `TopN{keys, n}` | sink + source | Heap, with a threshold fed back to the scan as a filter. |
| `Window{partition, order, frames}` | sink + source | C10. Sort plus precompiled frame machinery. |
| `Limit{n, offset}` | inline | Counts, then sets the pipeline's early-exit flag. |
| `SetOp{kind}` | sink + source | UNION ALL is a source merge. The others are hash based. |
| `Materialize{id}` / `CteRef{id}` | sink / source | Shared subplans, recursive CTE (C10). |
| `ResultSink{schema}` | sink | Writes A9 result chunks in DuckDB's type layout. |

The split between `HashLookup` and `Expand` comes from Diamond hardened joins (https://db.in.tum.de/people/sites/birler/papers/diamond.pdf). The lookup returns a contiguous match range and the expansion is a separate operator. Two consequences follow. A semijoin is a lookup with no expand. An N:1 join (a key-foreign-key join into a unique key, the common case in JOB) is a lookup whose range has length at most one, so the loop is dropped entirely. This requires every hash table to store equal keys contiguously. The unchained table's adjacency array does that by construction (https://db.in.tum.de/~birler/papers/hashtable.pdf).

**Worst-case optimal joins are not in the set.** Diamond measures WCOJ at about 25x slower on JOB. Freitag's hybrid chooses WCOJ correctly except for 5 false negatives out of 923 joins (https://doi.org/10.14778/3407790.3407797), but JOB has no cyclic queries that need it. We revisit this for cyclic TPC-DS queries at C10 only if the reduction schedule fails there.

**There is no radix-partitioned join.** Bandle et al. (SIGMOD 2021) find partitioning is rarely worth it. The unchained table with staged prefetch probes covers the cases partitioning was meant for.

## 4.3 Algorithm choice from facts

**Every choice in this section is a *Decide* use of a fact** in the sense of `../planner-v2/04-facts-not-estimates.md` section 4.3. Any class is allowed, and `Unknown` means the documented default is used and printed as `default`. None of these choices can produce a wrong answer, only a slow one. The rewrites that *can* produce a wrong answer (4.7) have stricter rules.

### 4.3.1 Scans

A scan chooses, per column, the representation it emits (4.6) and, per predicate, where the predicate runs:

1. **Zone map.** Resolved per row group, before the morsel is dispatched. Min/max and null count are `Exact` facts from `../stats`.
2. **Dictionary.** A predicate on a dictionary-encoded column is evaluated once over the dictionary. The result is a bitmap over codes, and the scan tests `bitmap[code]`. `LIKE '%(co-production)%'` over `movie_companies.note` becomes one pass over the distinct notes, not 2.6 million string matches.
3. **Vectorized kernel.** A predicate the first engine already has a kernel for is called through `vcall` on a batch of 1,024 and yields a selection vector (document 03).
4. **Generated.** Everything else is inlined into the pipeline loop.

The order is fixed. A predicate always takes the earliest applicable form.

### 4.3.2 Joins

| Condition (facts) | Choice |
|---|---|
| A stored link exists for the edge (`LinkHeader`, Exact) and the probe side is at most 1/16 of the target | `LinkJoin` |
| Build side unique on key (`Distinctness`, Exact) | `HashLookup` without `Expand` (N:1) |
| Build side ≤ 50 distinct keys (Exact or Certified) | `HashLookup` on a sorted inline array (a branchy compare chain up to 8) |
| Build key is a dense integer domain (min/max Exact, span ≤ 4 × count) | `HashLookup` on a direct-indexed array |
| Otherwise | unchained hash table, `HashLookup` + `Expand` |
| Join key = group key of the parent aggregate | `GroupJoin` (4.3.4) |
| `IN` / `NOT IN` / `EXISTS` with nullable build keys | `MarkJoin{null_mode = tri}` |

The 1/16 threshold for `LinkJoin` is a starting value. It is calibrated at C6 against the measured cost of a random link hop against a hash probe. The threshold of 50 distinct keys matches the point where DuckDB 1.2's SIP switches from an IN-list to min/max only (Parachute, arXiv 2506.13670). Below it, a linear compare beats hashing.

The unchained table is the default because it is about 2x faster than Robin Hood on join workloads. Its 16-bit Bloom tag in the directory entry rejects most misses before touching a bucket (FPR ≈1/169 at load factor 0.65). The known cost is that tiny queries can be up to 30% slower (https://db.in.tum.de/~birler/papers/hashtable.pdf). The small-build rows of the table above exist to recover exactly that case.

### 4.3.3 Aggregation

| Condition | Strategy |
|---|---|
| No keys | `ScalarAgg`: accumulators live in a thread-local slot, merged at finalize |
| One key, dictionary-coded or domain ≤ 2^16 (Exact) | `DenseAgg` over the code |
| Group count ≤ ~64K (any class) | thread-local hash tables, merged at finalize |
| Group count large or `Unknown` | thread-local pre-aggregation spilling to 64 hash partitions, then per-partition merge (MORSEL) |
| Group count low, 16+ threads, and Exact or Certified | global ticketed table (GHT, https://arxiv.org/abs/2505.04153): up to 1.78x over partitioned at low cardinality |

GHT's evaluation assumes perfect cardinality estimates. We therefore select it only when the group count is Exact or Certified. With an estimate, partitioned is the safe choice. The switch from thread-local to partitioned is also a runtime decision point (4.9): a thread-local table that passes its reservation spills instead of growing.

Aggregate functions follow document 03's whitelist. COUNT, SUM, MIN, MAX, AVG, ANY_VALUE, BOOL_AND/OR and COUNT(DISTINCT) are generated inline. Everything else uses an opaque state slot and runtime ABI calls.

### 4.3.4 Groupjoin

A `HashBuild` followed by a `HashAgg` whose keys equal the join key (or are functionally determined by it) becomes a `GroupJoin`. Fent & Neumann find the pattern in about 1/8 of TPC-H and TPC-DS queries (https://vldb.org/pvldb/vol14/p2383-fent.pdf). The Enable rule applies: the functional dependency must be Exact. JOB never needs it, since its queries are single SPJ blocks with `MIN` outputs. It is a C8 item.

### 4.3.5 MIN over joins: eager aggregation

**Every JOB query ends in `MIN` over a join result.** MIN is duplicate-insensitive, so a subtree whose only consumers are MIN aggregates, and whose join into the rest is N:M, can be deduplicated before the join. FFX reports 102-105x on N:M joins using factorised expansion (arXiv 2609.09002). We take the cheap part: an `Expand` feeding only duplicate-insensitive aggregates may emit each distinct payload once per probe tuple, and a build side feeding only MIN may keep one row per (key, payload) pair. This is a C6 rewrite, verified by rule V7 (4.10).

## 4.4 The reduction schedule

**Before any join runs, every relation is reduced by filters built from its neighbours.** This is Yannakakis-style semijoin reduction made practical with approximate filters, which is what RPT does. It is the single largest JOB lever in the research notes:

- On JOB 6a, reduction shrinks `cast_info` from about 36M tuples to 486 (TreeTracker, arXiv 2403.01631).
- On JOB, RPT's robustness factor, the ratio of worst to best order, drops from 30.4 mean / 371 max to 1.2 / 1.6 for left-deep plans (arXiv 2502.15181).
- RPT's end-to-end speedup is 1.46x on JOB. RPT+ reaches 1.47x (https://people.iiis.tsinghua.edu.cn/~huanchen/publications/rpt+-vldb26.pdf). Parachute reaches 1.54x (arXiv 2506.13670).

Those speedups are over a baseline engine and not additive. What we take is the robustness: with reduction, the join order `rudb-opt` hands us matters much less, and that is what makes document 03's "join order is an input" decision safe.

### 4.4.1 Shape: LargestRoot

All 113 JOB queries are alpha-acyclic, so the join graph (after collapsing equivalence classes of join keys) has a join tree. We follow RPT's LargestRoot. The spanning tree is rooted at the largest relation by row count. It takes a **forward pass** from leaves to root, where each relation builds a filter on its join key over its surviving rows and the parent applies it. It then takes a **backward pass** from root to leaves the same way. After both passes every relation holds only rows that can participate in some result, up to filter false positives.

For cyclic queries (TPC-H Q5; TPC-DS 19, 24, 46, 64, 68, 72 and 85 per RPT) we take the maximum-weight spanning tree and treat the remaining edges as ordinary joins. The result stays correct but reduction is no longer complete. RPT's SafeSubjoin rule governs which joins are safe to reorder after partial reduction. We adopt it unchanged.

### 4.4.2 Filter kinds and the cascade

Each transfer edge carries a **cascade** of up to three filters, from cheapest to most exact (RPT+'s asymmetric design):

1. **Min/max** of the build keys. Applied as a zone-map predicate before a row group is dispatched, so it can skip whole morsels.
2. **Approximate filter.** A cache-sectorized Bloom filter (CSBF): 64 B blocks, 32-bit sectors, 20 bits/key, k=7, 2.48 cycles/tuple, FPR 6.1e-5 (RPT+). For builds under 8 KiB of keys we use Parachute's 8 KiB k=2 filter (2% FPR) because it fits L1.
3. **Exact membership.** Used when it is cheap: a bitmap over a dictionary code or dense key domain, or a stored-graph bitmap from `../graph` (Exact). It replaces the Bloom filter rather than following it.

Bloom probes are 2-7x cheaper than hash-table probes (RPT). They still cost something: filter construction and probing take 28%, 12% and 46% of total time on TPC-H, JOB and TPC-DS in RPT. A filter that does not filter is pure cost. Hence the next section.

**Filters are pushed into the scan.** DuckDB 1.5 does the same with join Bloom filters [snippet]. SQL Server additionally carries min/max inside every bitmap (CIDR 2026, https://www.vldb.org/cidrdb/papers/2026/p29-zhao.pdf). Our cascade is the same idea made explicit.

### 4.4.3 Keep or drop: plan decides the candidates, runtime decides the survivors

**The planner emits every candidate edge. The runtime drops the ones that do not pay.** Thresholds are taken from the published systems, not invented:

- **At build finalize.** If the approximate filter has more than 34% of its bits set, discard it (Parachute). With the build complete we know its size exactly. A build covering a large fraction of the probe side's key domain will not filter, so skip the filter when `build_keys / probe_distinct` is Exact and above 0.9 (RPT+'s stop threshold).
- **During the probe scan.** Sample the first 100K probe tuples per filter (RPT+). Keep the filter if observed selectivity is below 0.35 while progress is below 0.6. Parachute's cheaper rule applies to the small filter: disable it if it removes fewer than 60% of rows after 4,000.
- **Ordering.** Surviving filters on one scan are ordered by observed (1 − pass rate)/cost, re-ranked per morsel (LIP, https://www.vldb.org/pvldb/vol10/p889-zhu.pdf, which cut a 2.1-58 s spread to 1.3-7.4 s).

Every keep/drop outcome is recorded in the query profile and shown in `EXPLAIN ANALYZE`. It is never fed back into later plans. That is the determinism rule of document 03.

**Known regressions we design against.** RPT+ reports regressions on JOB templates 8, 10 and 24 where a transferred filter broke zone-map skipping on the probe side. Our fix is structural. Min/max always runs before any Bloom filter on the same scan, and a filter never reorders or replaces a pushed predicate. RPT+ also reports that plain RPT regresses on ≥28% of SQLStorm queries against 2.1% for RPT+. The adaptive drop rule is not optional.

### 4.4.4 Exact graph bitmaps

When `../graph` has a stored link for an edge, it provides an Exact bitmap of source rows that have at least one partner. This is a free semijoin in one direction. The planner uses it as filter kind 3 and skips building the Bloom filter for that edge. RPT+ notes that a more accurate filter gave 4x on JOB 07c. An exact one is the limit of that.

## 4.5 Build-side choice

**Build the smaller side after reduction, unless a link or a uniqueness fact says otherwise.** The planner does not know post-reduction sizes, so the rule is:

1. If one side is unique on the key (Exact), it builds. The join becomes N:1 lookup-only.
2. If a stored link makes one direction a pointer chase, use `LinkJoin` with no build (`link-rotate` when that requires re-rooting the chain).
3. Otherwise build the side with the smaller estimated post-reduction size. Pre-reduction row count times the product of the estimated pass rates of its incoming filters.
4. **Runtime flip.** Both sides of a hash join that are fully reduced before the join pipeline starts have exact sizes at that point. If the chosen build side turns out more than 4x larger than the probe side, the runtime swaps them. This is a pre-declared decision point with both variants in the plan (4.9). It is possible only because reduction runs as separate pipelines before the join pipelines (document 05). RPT reports that bushy plans gain only 6%/11% over left-deep after reduction, so we do not change tree shape at runtime. We only flip.

## 4.6 Representation propagation

**Every column at every plan edge carries one of three representations.** The planner assigns them bottom-up and then fixes them top-down:

- `Code(dict)`: a dictionary code. Equality, grouping, hashing and bitmap filters work on the code directly. Morsels never straddle two encodings (document 03), so a code is meaningful for the whole morsel.
- `Value(type)`: decoded, in the physical type of document 03's type map.
- `RowId(table)`: not fetched yet. Only the row id travels.

Rules:

1. A column used only in joins, group keys and equality predicates stays `Code` if both sides share a dictionary (Exact), or if the join is through a stored link.
2. A column used only in the final projection or a top-level `MIN` travels as `RowId` and is fetched by a `RowIdFetch` placed as late as possible, after the last join that reduces cardinality. This is late materialization. JOB queries emit a handful of `MIN` columns over strings (titles, names, notes). Carrying 20-byte strings through five joins to keep 250 rows is the waste this rule removes.
3. `MIN` over a `Code` column is legal only if the dictionary is order-preserving (Exact). Otherwise the column is fetched before the aggregate.
4. Crossing into a `vcall` kernel forces whatever representation the kernel's signature accepts.

The representation is printed on every edge in `EXPLAIN`. Row-id columns show as `#rid(t)` and codes as `#code(t.col)`.

## 4.7 Facts, guards and deopt sites

**A plan decision that relies on a fact beyond what its class allows needs a guard.** A guard is a compiled check plus a deopt target, as document 03 defines it. The planner enumerates guards explicitly, so document 07 (code generation) never invents one.

| Speculation | Fact needed without guard | Guard when not met | Deopt target |
|---|---|---|---|
| No NULLs in column | `NullCount = 0`, Exact per row group | per-morsel null-count check from zone map | generic nullable variant |
| Narrow accumulator (e.g. i64 SUM) | Exact min/max × count fits | headroom pre-check at morsel start: worker bound + rows × max\|v\| < i64::MAX (document 09 section 9.6); scalar SUM may instead use a morsel-local partial checked at morsel end | widened (i128) variant; with no bound, no narrow speculation |
| Strings ≤ 12 bytes (inline) | Exact max length | per-morsel length check | general string variant |
| Build unique on key (no `Expand`) | Exact distinctness | duplicate detected at build insert | `Expand` variant |
| Dictionary-coded input | encoding per row group (Exact) | per-morsel encoding tag check | value variant |

The Enable rule of `../planner-v2` still holds. Rewrites that change the result if the fact is wrong (join elimination, sort elimination, DISTINCT elimination) require Exact and never take a guard. A guard protects speed, never correctness. All guarded checks except "build unique" run before the first side effect of a morsel, so the morsel can be re-run (document 05, `Status::Deopt`). "Build unique" is checked at build time, before any probe exists, so it switches variants for the probe pipeline as a whole.

## 4.8 Memory reservations

**Memory is reserved at plan time from size facts and allocated by the runtime at pipeline start** (document 03). The planner writes one reservation per state object:

```
Reservation { state: StateId, bytes_lo: u64, bytes_hi: u64, basis: Fact, on_exceed: Spill | Regrow | Deopt }
```

- `bytes_lo` is from Exact or Certified facts and is always allocated.
- `bytes_hi` is the upper estimate. The runtime may allocate up to it lazily.
- A hash table's final size is not reserved at plan time. Two-phase build (document 05) sizes it exactly at finalize, as in MORSEL (https://db.in.tum.de/~leis/papers/morsels.pdf). The reservation covers the thread-local materialization buffers.

The sum of `bytes_lo` over pipelines that can be live at once is checked against the query memory budget before execution starts. If it does not fit, the query routes to the first engine, which has out-of-core operators. Spilling in the compiled engine is a C8 item, for aggregation partitions only.

## 4.9 What is decided at plan time and what at runtime

**Plan time decides structure. Runtime decides which of the pre-declared variants runs.** The runtime never invents a plan. It picks among alternatives that are all in the plan text.

| Decision | When | Input |
|---|---|---|
| Operator algorithms, join order, filter candidates, representation | plan | facts |
| Guard placement and deopt targets | plan | facts |
| Keep/drop each reduction filter | runtime, build finalize + first 100K probes | observed bits set, selectivity |
| Filter order on a scan | runtime, per morsel | observed pass rates |
| Build/probe flip | runtime, join pipeline start | exact reduced sizes |
| Hash table size | runtime, build finalize | exact tuple count |
| Probe staging: fused or staged with group prefetch | runtime, probe pipeline start | built table size vs LLC |
| Aggregation: thread-local vs spill to partitions | runtime, when the reservation is exceeded | observed groups |
| Tier: interp / direct / clif / llvm | runtime, per pipeline (document 09) | compile cost vs remaining morsels |

The staging decision follows ROF (http://www.vldb.org/pvldb/vol11/p1-menon.pdf). It warns that stage boundaries placed from estimates land in the wrong place, and that the table size is exact after the build. Document 05 specifies both variants of the probe loop.

## 4.10 Verifier rules

`rudb-qc-plan` ends with a verifier. On failure the query goes to the first engine and the failing rule is logged.

- **V1 Types.** Every edge's representation is consistent with its producer and consumer. A `Code` edge names one dictionary.
- **V2 Liveness.** Every column read above an edge is produced below it, possibly as `RowId` with a `RowIdFetch` in between.
- **V3 Roles.** Every sink has exactly one source above it in its pipeline. Every breaker is reached.
- **V4 Filters.** Every filter consumer is scheduled after its producer's build finalizes. The transfer graph is acyclic.
- **V5 Facts.** Every Enable rewrite cites an Exact fact. Every speculation cites its fact or carries a guard.
- **V6 Reservations.** Every state object has a reservation. The concurrent `bytes_lo` sum is within budget.
- **V7 Duplicate insensitivity.** Every eager deduplication (4.3.5) has only duplicate-insensitive consumers up to the root.
- **V8 NULL semantics.** Every `NOT IN` compiles to a tri-state `MarkJoin`. Birler & Neumann show DuckDB 1.3.0 returns wrong results for `NOT IN` with NULLs (https://www.vldb.org/cidrdb/papers/2026/p3-birler.pdf). The differential harness of C1 must include those cases.

## 4.11 Worked example: JOB 1a

```sql
SELECT MIN(mc.note) AS production_note,
       MIN(t.title) AS movie_title,
       MIN(t.production_year) AS movie_year
FROM company_type AS ct, info_type AS it, movie_companies AS mc,
     movie_info_idx AS mi_idx, title AS t
WHERE ct.kind = 'production companies'
  AND it.info = 'top 250 rank'
  AND mc.note NOT LIKE '%(as Metro-Goldwyn-Mayer Pictures)%'
  AND (mc.note LIKE '%(co-production)%' OR mc.note LIKE '%(presents)%')
  AND ct.id = mc.company_type_id
  AND t.id = mc.movie_id
  AND t.id = mi_idx.movie_id
  AND mc.movie_id = mi_idx.movie_id
  AND it.id = mi_idx.info_type_id;
```

Sizes [GK]: `title` ≈2.53M, `movie_companies` ≈2.61M, `movie_info_idx` ≈1.38M, `info_type` 113, `company_type` 4. The three `movie_id` equalities form one equivalence class {t.id, mc.movie_id, mi_idx.movie_id}, so the join graph is a star around that class, which is acyclic. LargestRoot roots the tree at `mc`.

Each 'top 250 rank' movie has one `mi_idx` row, so the `it` filter leaves 250 `mi_idx` rows [derived]. The whole plan's job is to never touch more than a few thousand rows of `mc` and `t` after that.

```
PhysPlan job-1a   (facts: stats@v17, graph@v4)            deterministic
reduction: root=mc  forward[it→mi_idx, ct→mc, mi_idx→mc, t→mc]  backward[mc→mi_idx, mc→t]

ScalarAgg{min(mc.note), min(t.title), min(t.production_year)}
└─ RowIdFetch t{title, production_year}      -- late: after last join
   └─ HashLookup ht3 (N:1, t.id unique Exact)  keys=[mc.movie_id]  out: #rid(t)
      └─ HashLookup ht2 (dedup build: mi_idx.movie_id not unique, all consumers MIN → no Expand, V7)
         └─ HashLookup ht1 (N:1 ct.id unique Exact, 4 keys → inline array)
            └─ Scan mc {company_type_id #code, movie_id, note #code(mc.note)}
                 zone: -
                 pred: dict-bitmap(note: NOT LIKE ... AND (LIKE ... OR LIKE ...))   [4.3.1 step 2]
                 filters: [minmax(mi_idx.movie_id) → bloom(mi_idx.movie_id) ; ct bitmap(#code)]
HashBuild ht1 ← Scan ct pred: kind='production companies'           (4 rows, inline array)
HashBuild ht2 ← Scan mi_idx {movie_id, info_type_id}
                  filters: [exact bitmap(it) on info_type_id]        (it: 113 rows, dense domain)
HashBuild ht3 ← Scan t {id, #rid}  filters: [minmax+bloom(mc.movie_id) (backward pass)]
guards: G1 mc.note dictionary-coded per row group → deopt: value variant (string LIKE via vcall)
reservations: ht1 lo=64B; ht2 lo=8KiB hi=22MiB (basis mi_idx.rows Exact × sel Estimated); ht3 lo=0 hi=40MiB
runtime points: R1 keep/drop bloom(mi_idx→mc); R2 flip ht3 if |t reduced| > 4×|mc reduced|; R3 staging per probe
```

Read from the bottom. `it` and `ct` are resolved to bitmaps over tiny domains. `mi_idx` builds from its 250 surviving rows. The `mc` scan evaluates the three `LIKE`s once per dictionary entry and then tests a bit per row. The min/max of 250 `movie_id`s skips most row groups of `mc` before they become morsels. The Bloom filter removes almost all of the rest. `t` is built only from the ids that survived `mc` (backward pass), and it contributes `title` and `production_year` as row ids fetched at the very end.

`ht2` shows the eager-deduplication rewrite. `mi_idx.movie_id` is not unique, but every consumer above is `MIN`, so the build keeps one row per movie. The flip at R2 will almost always fire, because after reduction `t` is at most a few hundred rows while `mc` is in the thousands. The plan says so without betting on it.

What the plan does not contain matters as much. There is no string decoding in any loop, no hash table larger than a few hundred entries in the expected case, and no join order sensitivity: any of the orders `rudb-opt` might pick produces this same reduced input.

## What we should take from this document

The physical plan, not the code generator, is where JOB is won. A 179x spread between join orders is a planning problem, and reduction with filters pushed into the scan is the published answer. Our contribution is to make that reduction deterministic in structure and adaptive only at named runtime points.

Facts decide algorithms, and the class of a fact decides what it may be used for. Decisions may use any fact. Rewrites that change the answer need Exact facts. Speculation beyond a fact needs a guard with a deopt target, and a guard protects speed, never correctness.

Every column travels as a code, a value or a row id, and the choice is printed on every edge. Late materialization and dictionary-level predicate evaluation are why JOB's string-heavy `MIN` outputs cost almost nothing.

The runtime chooses only among variants already in the plan text: filter keep/drop, filter order, build/probe flip, table size, probe staging, aggregation spill and tier. That list is closed. Adding to it requires a change to this document.

Every threshold here is taken from a published system, with its source next to it, and is recalibrated at C6 and C8 against our own measurements: 34% of bits set, 0.35/0.6/0.9, 60% after 4,000, 4x flip, 1/16 link, 50 distinct keys.
