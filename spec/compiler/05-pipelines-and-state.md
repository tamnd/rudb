# Pipelines and state

Document 04 produced A5, a physical plan with representations, guards and reservations. This document turns A5 into A6, the pipeline graph. A6 is the unit every later stage works on. The code generator emits one set of step functions per pipeline. The tiering controller picks a backend per pipeline. The scheduler dispatches (pipeline, morsel range) tasks.

Crate: `rudb-qc-pipe`. Input: `PhysPlan`. Output: `PipeGraph`, printable with `EXPLAIN (CODEGEN, PIPELINES)`. The budget is shared with physical planning: 0.1 ms median, 0.5 ms max (document 02).

Three decisions in this document constrain every later document, so they come first:

1. **One ABI for every step.** `extern "C" fn(state: *mut PipelineState, morsel: *const Morsel) -> Status`, as fixed in document 03. Init, body, merge and finalize all use it. Non-body steps pass a null morsel.
2. **Generated code never allocates and never blocks.** It returns a `Status` and the runtime does the rest. That is what makes a morsel restartable and a pipeline suspendable.
3. **Parallelism is a compiler pass, not a code generator feature.** The pipeline is generated as if single-threaded. A pass over A6 classifies every state slot as shared or local and inserts the merges.

## 5.1 Decomposition

**A pipeline runs from one source through inline operators into one sink.** Code between two breakers is one loop that keeps attributes in registers. This is HyPer's definition (https://www.vldb.org/pvldb/vol4/p539-neumann.pdf) and we keep it unchanged. Document 04 tagged every operator as source, inline, sink or breaker, so decomposition is a single post-order walk:

```
fn decompose(op, g: &mut PipeGraph) -> PipeId {
    match role(op) {
        Source          => g.new_pipeline(op),
        Inline          => { let p = decompose(op.child, g); g.push(p, op); p }
        Sink            => { let p = decompose(op.child, g); g.close(p, op); p }
        Breaker         => { let p = decompose(op.child, g); g.close(p, op.sink_half());
                             let q = g.new_pipeline(op.source_half()); g.edge(p, q, Finalize); q }
        Probe(build)    => { let b = decompose(build, g);           // build pipeline first
                             let p = decompose(op.probe_child, g);
                             g.push(p, op); g.edge(b, p, Finalize); p }
    }
}
```

After the walk, two passes add the edges the tree does not show:

- **Filter edges.** For every transfer edge in the reduction schedule (document 04, 4.4), add `edge(producer, consumer, FilterPublish)`. The consumer pipeline may not start until the producer has published the filter. Verifier rule V4 guarantees these edges are acyclic.
- **Reduction buffers.** The backward pass of LargestRoot needs the root's survivors *before* the root's own joins run. The walk handles that by splitting the root scan. The first pipeline scans the root, applies its forward-pass filters and cheap lookups, and writes survivors into an `AppendBuffer`. At the same time it builds the backward filters. A second pipeline uses that buffer as its source. The buffer holds only the join keys, the codes and a row id per survivor, never decoded strings (document 04, 4.6). The same split is what makes the runtime build/probe flip possible (4.5 there). Both sides of the flipped join are materialized and exactly sized when the join pipeline starts.

Pipelines whose source is known (Exact) to fit in one morsel are marked `tiny`. They run on the dispatching thread, on the `interp` tier, with no task dispatch. `company_type` (4 rows) and `info_type` (113 rows) in JOB are tiny. Dispatching a task for them would cost more than running them.

## 5.2 The dependency DAG

**A6 is a DAG of pipelines with typed edges.** Edges come in three kinds:

| Edge | Meaning | Consumer may start when |
|---|---|---|
| `Finalize` | consumer reads a structure the producer builds (HT, agg table, buffer, sorted run) | producer's finalize step returned `Ok` |
| `FilterPublish` | consumer's scan applies a filter the producer builds | producer published the filter (at its finalize) |
| `Order` | consumer must see results after producer (UNION ALL order, CTE) | producer finished |

The scheduler runs every pipeline whose incoming edges are satisfied. Independent pipelines run concurrently from the same pool (document 03: one process-wide pool, a task is a (pipeline, morsel range) pair). Critical-path length is reported in `EXPLAIN ANALYZE`. On JOB the DAG is deep and narrow, 5-12 pipelines with a critical path through nearly all of them, because the reduction schedule is sequential by design. Parallelism comes from morsels within a pipeline, not from pipelines side by side.

## 5.3 The Morsel

```
#[repr(C)]
pub struct Morsel {
    source:    u32,   // source id inside the pipeline (table, buffer, HT, run)
    chunk:     u32,   // row group, buffer chunk, or partition index
    begin:     u32,   // first row, relative to chunk
    end:       u32,   // one past last row
    seq:       u64,   // global order key: (chunk << 20) | morsel-in-chunk
    enc:       u32,   // encoding tag of every column of this morsel (bitset per column group)
    flags:     u32,   // FIRST_IN_CHUNK, LAST_IN_CHUNK, ZONE_ALL_MATCH, NO_NULLS, ...
}
```

Thirty-two bytes, passed by pointer. `enc` and `flags` carry the per-morsel facts that guards test (document 04, 4.7). They are computed by the dispatcher from the zone map before the morsel is handed out, so a guard costs one compare at morsel start. `ZONE_ALL_MATCH` means the zone map proves every row passes the pushed predicate. The body then skips predicate evaluation for that morsel.

### 5.3.1 Why 16,384 tuples

**16,384 tuples: 16 batches of 1,024, never crossing a row group.**

- **Lower bound.** Scheduling overhead is negligible above about 10,000 tuples per morsel (MORSEL, https://db.in.tum.de/~leis/papers/morsels.pdf). ADAPT uses about 10,000 (https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf).
- **Upper bound.** MORSEL uses about 100,000. We go smaller for four reasons, all [derived]:
  1. A deopt re-runs the whole morsel, so the morsel bounds the wasted work.
  2. Cancellation is checked between morsels, so latency is one morsel. At roughly 1-10 ns per tuple that is 16-160 µs.
  3. JOB's reduced relations are small. A 60K-row input splits into 4 morsels and can use 4 threads, where 100,000-tuple morsels would give it 1.
  4. The tiering controller decides after each morsel (document 09), and smaller morsels mean earlier decisions.
- **Row-group alignment.** A morsel never straddles two encodings (document 03), so a row group of *n* rows yields ⌈*n*/16,384⌉ morsels and the last one may be short. Buffer and hash-table sources use the same size over their chunks.
- **Batch of 1,024.** Within a morsel the body loops over batches of 1,024 with a selection vector of `u16` indices (2 KiB, L1-resident). Kersten et al. find vector size best around 1,000, with some queries preferring larger, e.g. Q3 15% faster at 64K (https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf). The batch is the unit for `vcall` into the first engine's vectorized scan kernels, and the unit of staging in 5.7.

The morsel size is a constant of the runtime, not of the plan. The dispatcher may hand a worker several consecutive morsels as one task when a pipeline has many morsels and few threads. It never splits one.

## 5.4 The step functions

**Each pipeline compiles to up to five steps, all with the same ABI:**

| Step | Runs | Does |
|---|---|---|
| `init` | once per worker, before its first morsel | zero local accumulators, set local cursors, record buffer lengths |
| `body` | once per morsel (possibly re-entered) | the fused loop: scan → inline operators → sink into local state |
| `local_fin` | once per worker, after its last morsel | flush thread-local partials (e.g. scalar accumulators, pre-agg overflow) |
| `merge` | once per worker, serially or per partition | fold one worker's local state into shared state |
| `finalize` | once per pipeline | size and build shared structures, publish filters and size facts |

`merge` and `finalize` may themselves be parallel: a partitioned aggregation merges partition *i* in task *i*. In that case the morsel pointer carries the partition index in `chunk`. This is Umbra's model of a pipeline as a state machine of steps, each step a generated function, with suspension possible between any two (https://db.in.tum.de/~freitag/papers/p29-neumann-cidr20.pdf). The only change is that our steps share one signature, so the interpreter, `direct`, `clif` and `llvm` all implement one calling convention and the tiering controller can swap the function pointer for a step between two calls.

### 5.4.1 Status

```
#[repr(C)] pub struct Status(u64);    // low 8 bits: kind, high 56: payload

kind  name         payload            runtime action
0     Ok           -                  next morsel / next step
1     Yield        -                  re-call body with the same morsel; cursor is in local state
2     Done         -                  pipeline may stop early (LIMIT reached, TopN closed)
3     Deopt        site id            roll back morsel, switch this pipeline to the site's
                                      fallback variant, re-run the same morsel there
4     NeedMemory   slot id            grant a chunk to the slot, re-call body (cursor in local state)
5     Cancelled    -                  stop; the query is being torn down
6     Error        error code         abort query with the SQL error for that code
```

`Yield` exists for backpressure. A `ResultSink` whose output chunk is full, or a staged probe that has handed a full vector to the next stage, saves its position and returns. `NeedMemory` exists because generated code never allocates. An append buffer or hash-table chunk list that fills its current chunk saves its position and asks for more. Both are resumable, which is why the body is written as a loop over a cursor held in local state (`hdr.cursor`) and not over a local variable.

**Errors are deferred to morsel end.** Checked arithmetic, division by zero and failed casts set a bit in the local error word and continue with a harmless value. The body tests the word once after the batch loop and returns `Error`. That keeps the fast path branch-free. It is legal because an error aborts the query, so no side effect of that morsel is ever observed. This is the MORSEL treatment of overflow and cancellation (checked after each morsel) moved into the body. Cancellation is checked by the runtime between calls. Inside the body, only loops not bounded by the morsel (hash chain walks, nested loops, unbounded string scans) check it, with the counted `poll` instruction of document 06 and verifier rule V11.

### 5.4.2 Deopt: the side-effect rule

A morsel can be re-run only if the first run left no trace. The code generator enforces one of three conditions for every guard site the planner declared:

1. **Guard before first effect.** The guard is a per-morsel check on `Morsel.enc`/`flags` or on a zone-map fact, placed before the batch loop. Most guards in document 04 section 4.7 are of this kind: no NULLs, dictionary encoding, string length.
2. **Effects into rollback-able state.** Append buffers, hash-table chunk lists and filter builders record their lengths at `body` entry. `Deopt` truncates them back. Bloom bits are the exception because they cannot be unset, so filter bits are set only after the morsel's last guard has passed. The body collects keys for the filter in a batch-local array and sets bits at batch end.
3. **Effects into morsel-local partials.** A narrow scalar accumulator (i64 `SUM` with an overflow guard) accumulates into a morsel-local register or slot. It is folded into the worker's local accumulator only after the morsel-end overflow check passes. Hash aggregation does not speculate on overflow unless the check is possible from zone-map facts at morsel start (max |x| × morsel rows fits the headroom). Otherwise the planner picks the wide type.

On `Deopt(site)`, the runtime switches only this pipeline's `body` function pointer to the fallback variant named by the site, for all workers from their next call. The deopt count is reported per site. The fallback is always compiled at the tier the body is currently running at, or `interp` if that is not ready, because document 09's tiering must never wait on a compile.

## 5.5 PipelineState

**One `PipelineState` per (pipeline, worker), laid out by the pipeline pass, with every slot at a constant offset.** The generated code addresses slots as `state + K`. It never chases a table of pointers.

```
#[repr(C, align(64))]
pub struct StateHeader {                // exactly one cache line
    shared:     *const u8,              // SharedState of this pipeline (read-only in body)
    rt:         *const RtVtable,        // runtime ABI: vcall table, string heap, rare-agg fns
    profile:    *mut Counters,          // per-worker counters (5.9)
    cursor:     u64,                    // resume point for Yield / NeedMemory
    error:      u32,                    // deferred error word
    deopt_site: u32,
    worker:     u16,
    variant:    u16,                    // which body variant is live (deopt, staging)
    poll_left:  u32,                    // countdown for `poll` (document 06); cancel word is in rt's query context
    params:     *const u8,              // parameter block (document 14), null if unparameterized
    _pad:       [u8; 8],
}
pub struct PipelineState { hdr: StateHeader /* then local slots at fixed offsets */ }
```

`SharedState` is a separate block, also at fixed offsets, one per pipeline. It holds pointers to built hash tables, published filters, the shared aggregation table (if any) and pipeline-wide counters. During `body` it is read-only, with two exceptions: the `Limit` counter and GHT ticketing (5.6), both atomic and both listed per pipeline in `EXPLAIN`.

### 5.5.1 Slot kinds

| Slot | Local part (per worker) | Shared part | Merge |
|---|---|---|---|
| `HtBuild` | chunk list of (hash, key, payload) | directory + adjacency array | none; finalize builds from all lists |
| `HtProbe` | - | directory + adjacency, Bloom tags | - |
| `AggTable` (thread-local) | open-addressing table | final table | insert-or-combine |
| `AggPartitioned` | small table + 64 partition lists | 64 tables | per-partition task |
| `AggTicketed` | ticket range cache | ticketed table + arrays | atomic or local slot combine |
| `DenseAgg` | array over the domain | array | element-wise combine |
| `ScalarAcc` | accumulators | accumulators | combine |
| `AppendBuffer` | chunk list | chunk directory | concat, keeping `seq` order |
| `FilterBuild` | min/max + key hashes | min/max, CSBF or small PSF, exact bitmap | OR bits, min/max |
| `FilterProbe` | pass/fail sample counters | the published filter | counters summed at finalize |
| `SortRun` | normalized-key run | run list | k-way merge (document 11) |
| `TopN` | heap | heap + threshold | heap merge; threshold published to scan |
| `Limit` | - | atomic row counter | - |
| `ResultSink` | output chunk | ordered chunk queue | by `seq` if order is required |
| `RareAgg` | opaque bytes, size from runtime ABI | opaque | runtime `combine` call |

Local slot memory comes from the reservation of document 04 section 4.8. It is allocated by the runtime at pipeline start, before `init`. Growth happens only via `NeedMemory`.

## 5.6 Local vs shared, and parallel instantiation

**The pipeline is generated single-threaded. A pass makes it parallel.** The pass `parallelize` runs on A6 after decomposition and before QIR generation (document 06):

1. For each slot, pick its kind from the table in 5.5.1 using the strategy the planner chose. `HashAgg{strategy}` maps to one of the three `Agg*` kinds.
2. Every write in `body` goes to the local part. Every read of a built structure goes to the shared part.
3. Emit `local_fin` and `merge` from the slot kinds. Emit `finalize` from the slot kinds plus the publishing duties in 5.7.
4. Choose the degree: `dop = min(pool_threads, ceil(morsels / 2))`, with `morsels` Exact for table and buffer sources. A pipeline with `dop = 1` skips `merge` entirely and its local state *is* the shared state.

No atomics in the body is the default. The two allowed exceptions are the ones that measurably win. The `Limit` counter is needed for early exit. The GHT ticketed aggregation gives 1.78x over partitioned at low cardinality with 48 threads (https://arxiv.org/abs/2505.04153), and the planner selects it only with an Exact or Certified group count (document 04, 4.3.3).

The hash-table build follows MORSEL's two-phase design, adapted to the unchained table's per-thread partitioning. It has no atomics (https://db.in.tum.de/~birler/papers/hashtable.pdf):

```
body (per morsel):   hash keys (CRC32), append (hash, key, payload) to local chunk list,
                     partitioned by the top bits of the hash; add keys to local FilterBuild
finalize:            n = Σ local counts (exact)          → publish size fact
                     directory = pow2 ≥ n / 0.65, huge pages
                     parallel over hash partitions: count per slot, prefix-sum, scatter into
                     adjacency array; set 16-bit Bloom tags in directory entries
                     merge FilterBuild parts; apply keep/drop rule (document 04, 4.4.3); publish
```

## 5.7 Build → probe handoff

**A build pipeline's `finalize` publishes three things, and the probe pipeline's first action reads them.**

1. **The structure.** The hash table, sized exactly from the counted tuples.
2. **The filters.** Min/max, then the Bloom or small filter (or none, if dropped), then any exact bitmap. Min/max goes to the dispatcher as well, which applies it to row-group zone maps before creating morsels (document 04, 4.4.2 step 1).
3. **The size fact.** Tuple count, distinct count if cheap, directory bytes. This is an `Observed` fact in the `../planner-v2` vocabulary. It is used at runtime decision points only and never written back to the catalogue.

The probe pipeline's runtime decision points read the size fact:

- **Build/probe flip** (document 04, 4.5). Both sides are materialized, so both sizes are exact. Flip if the build is more than 4x the probe. Both variants of the join pipeline are in the plan.
- **Fused vs staged probe.** If the table fits its per-thread share of the last-level cache (document 10 section 10.6), the probe is *fused*: lookup and expand are inline in the batch loop. Otherwise it is *staged* (ROF, http://www.vldb.org/pvldb/vol11/p1-menon.pdf). The batch loop hashes all 1,024 keys and prefetches directory entries in groups, then filters by Bloom tag and prefetches the surviving buckets, then compares and emits. ROF found group size 16 best and Psaropoulos et al. about 10 (2.7-3.7x from group prefetching, https://doi.org/10.14778/3149193.3149202). We start at 16 and calibrate at C6. The probe pipeline is generated only after its builds finalize (document 09, Rule I3), so only the chosen variant is ever generated. The choice is made once per probe pipeline, after the build, because ROF warns that stage boundaries placed from estimates land in the wrong place.

Kersten et al. show why this handoff matters. Compiled Q9 was 32% slower than vectorized because tuple-at-a-time probing serializes cache misses (https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf). The staged variant is our answer, and the size fact decides when it pays.

## 5.8 Order preservation

**Every morsel carries a `seq`, and every order-sensitive sink respects it.** DuckDB preserves insertion order for queries without `ORDER BY` by default [GK]. We are 100%-compatible, so the result must match row for row where DuckDB's order is defined, and the compat harness compares ordered output.

- A pipeline marked `ordered` (source is a table scan, sink is `ResultSink` or `AppendBuffer` feeding one, no reordering operator in between) has its sink tag output chunks with the morsel's `seq`. `ResultSink` releases chunks in `seq` order from a small reorder queue. A chunk from a later morsel waits for earlier ones.
- `Limit` in an ordered pipeline stops dispatching morsels past the watermark: the smallest `seq` at which the counted rows of all lower `seq`s reach *n*. Workers holding lower morsels finish them. This is correct and costs at most one morsel per worker.
- Pipelines after a `Sort` are sourced from the merged run and are ordered by construction.
- Hash-based operators destroy order. A pipeline downstream of one is `ordered` only if the plan re-imposes order.

JOB never needs any of this: every query is one row of `MIN`s. TPC-H, ClickBench and the compat suite do.

## 5.9 Profiling counters

Every pipeline has per-worker `Counters`: morsels, tuples in and out per operator, filter pass counts, deopts per site, `Yield` and `NeedMemory` counts, and cycles per step read at step boundaries. Counters are incremented per batch, not per tuple, except where a filter's sample needs per-tuple counts during its first 100K tuples. Kersten et al. list profiling and adaptivity as the weak points of compiled engines. Here they are designed in. The same counters drive the keep/drop rules, filter reordering and tiering.

## 5.10 Worked example: the pipeline graph of JOB 1a

From the physical plan in document 04 section 4.11:

```
PipeGraph job-1a   dop=10 (M4, 10 cores)          critical path P3 → P4 → P5 → P6

P1  tiny  interp   Scan ct [pred kind='production companies'] → FilterBuild bm_ct(#code company_type_id)
P2  tiny  interp   Scan it [pred info='top 250 rank']          → FilterBuild bm_it(dense id domain)
P3                 Scan mi_idx {movie_id, info_type_id}
                     filters: bm_it
                   → HtBuild ht2 (dedup, MIN-only)  → FilterBuild f_mi(movie_id: minmax, bloom)
                   finalize: publish ht2, f_mi, size(ht2)          ~250 [derived]
P4                 Scan mc {company_type_id #code, movie_id, note #code, #rid}
                     zone: minmax(f_mi)   pred: dict-bitmap(note)   filters: bm_ct, bloom(f_mi)
                   → HashLookup ht2 (fused: ht2 fits L1)
                   → AppendBuffer b_mc {movie_id, note #code}  → FilterBuild f_mc(movie_id)
                   finalize: publish b_mc, f_mc, size(b_mc)
P5                 Scan t {id, #rid}   zone: minmax(f_mc)   filters: bloom(f_mc)
                   → HtBuild ht3 {id → #rid}
                   finalize: publish ht3, size(ht3)
                   runtime R2: if |ht3| > 4×|b_mc| flip → build from b_mc, probe from ht3 source
P6                 Source b_mc
                   → HashLookup ht3 (N:1) → RowIdFetch t{title, production_year}
                   → ScalarAcc {min(note: decode at local_fin), min(title), min(production_year)}
                   → ResultSink (1 row)

edges: P2 -FilterPublish→ P3;  P1 -FilterPublish→ P4;  P3 -Finalize→ P4;  P3 -FilterPublish→ P4
       P4 -FilterPublish→ P5;  P4 -Finalize→ P6;  P5 -Finalize→ P6
guards: P4 body site 1: enc(mc.note) = dict   → variant P4' (value note, LIKE via vcall)
```

Notes on what this graph encodes:

- **P1 and P2 cost nothing.** They are tiny, interpreted, and run before the first dispatch.
- **P4 dominates.** It is the only pipeline over a multi-million-row table, and most of its row groups never become morsels because `f_mi`'s min/max over about 250 movie ids prunes them at dispatch. The surviving morsels test a dictionary bitmap and a Bloom filter per row before touching `ht2`.
- **`min(mc.note)` stays a code until `local_fin`.** The accumulator holds the code, compared through the order-preserving dictionary if it has one. Otherwise the note is decoded per surviving row, which is a few hundred rows. The decode happens once per worker, at the end.
- **P5 is where the backward pass pays.** Without `f_mc` it would build a 2.5M-entry table over `title`. With it, the build is expected to be about as large as `b_mc`. At that point R2 decides which side builds, from exact counts.

## What we should take from this document

A pipeline is a set of steps sharing one ABI, and every step returns a `Status`. That single convention lets four backends, the tiering controller, the scheduler and deoptimization all work on the same unit without special cases.

Generated code never allocates, never blocks, and checks for cancellation only on loops the morsel does not bound. It returns `Yield`, `NeedMemory`, `Deopt` or `Error`, and the runtime acts. The side-effect rule (guard first, rollback-able effects, or morsel-local partials) is what makes deopt a re-run and not a recovery.

Parallelism is a pass over single-threaded pipelines. Slots are classified local or shared, merges are generated from the slot kind, and atomics appear in the body only for `Limit` and ticketed aggregation.

The build's `finalize` is the moment the plan gets exact information. It publishes the table, the filters and the size fact. The flip, the fused-vs-staged probe choice and the filter keep/drop rules all read it. That is how we get the benefit of adaptivity while the plan stays deterministic.

Morsels of 16,384 tuples in batches of 1,024 balance scheduling overhead against deopt cost, cancellation latency and parallelism on JOB's small reduced inputs. Both numbers are runtime constants to be re-measured at C5.
