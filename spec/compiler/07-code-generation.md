# Code generation

Artifact A6 in, artifact A7 out. This document is the translator in `rudb-qc-gen`: its layers, how it walks a pipeline, where it stops generating and calls precompiled kernels, and how it turns data facts into specialized code without losing correctness when a fact turns out to be wrong.

## 7.1 The job and its budget

**The generator makes one linear pass over each pipeline of A6 and appends QIR. It never looks at what it has already emitted.** Its budget from document 02 is 0.1 ms median and 0.5 ms maximum per JOB query, which at the ≤ 10,000 instructions of section 6.6 is 10 ns per instruction. For comparison, that is about the cost of one hash map insert and one `Vec` push in Rust. The rules in section 7.11 exist to keep it there.

Everything the generator decides, it decides from A5 and A6: the physical operator, its algorithm, the facts attached to it, the state layout document 05 fixed. Nothing is decided by inspecting generated code. That is why no backtracking is needed, and why a translator can be unit-tested by printing its QIR for a hand-written A6 fragment.

The research line is Tidy Tuples (TIDY, https://db.in.tum.de/~kersten/Tidy%20Tuples%20and%20Flying%20Start%20Fast%20Compilation%20and%20Fast%20Execution%20of%20Relational%20Queries%20in%20Umbra.pdf) for the layering, HyPer's produce/consume for the traversal (NEU11, https://www.vldb.org/pvldb/vol4/p539-neumann.pdf), ROF for staging inside a pipeline (http://www.vldb.org/pvldb/vol11/p1-menon.pdf), and HyPer's Data Blocks for the scan boundary (E §2.1 [snippet]). Of the four ways to stage code in Rust (E §3.3), the choice is (a) a typed builder, for the generator, plus (d) precompiled monomorphized kernels, for everything the generator calls. Tracing in the Nautilus style needs a trace pass per control-flow split, O(2^n) in the worst case (NAUT, https://nebula.stream/paper/grulich_sigmod2024.pdf), and that cost lands on the query path.

## 7.2 The layers

Five layers, top to bottom. **Only layer 4 knows about NULLs, overflow, casts and collations.** A translator that tests a validity bit itself is a bug in review.

**Layer 5, the typed builder.** A thin typed skin over `rudb-qc-ir`'s arena. A value is a `Val` with a phantom type, so a `V<I32>` cannot be passed where a `V<I64>` is expected, and the Rust compiler catches the mistakes the QIR verifier would otherwise catch at runtime.

```rust
pub struct V<T: QTy>(Val, PhantomData<T>);          // Copy; 4 bytes
pub struct Fb<'a> { ir: &'a mut FuncBuilder, node: PlanNodeId, scopes: ScopeStack }

impl Fb<'_> {
    #[track_caller] pub fn load<T: QTy>(&mut self, a: Addr) -> V<T>;
    #[track_caller] pub fn add_t<T: Int>(&mut self, a: V<T>, b: V<T>, e: ErrSite) -> V<T>;
    #[track_caller] pub fn crc32c(&mut self, seed: V<I64>, x: V<I64>) -> V<I64>;
    #[track_caller] pub fn rtcall<S: ProxySig>(&mut self, p: Proxy<S>, args: S::Args) -> S::Ret;
    // structured control: the only way to make blocks
    pub fn if_<F: FnOnce(&mut Self)>(&mut self, c: V<I1>, then: F);
    pub fn if_else<R: Vals, F, G>(&mut self, c: V<I1>, t: F, e: G) -> R;   // R becomes block params
    pub fn loop_with<S: Vals, F>(&mut self, init: S, body: F) -> S          // S is loop-carried
        where F: FnOnce(&mut Self, S) -> Flow<S>;                            // Continue(S) | Break(S)
}
```

`if_else` and `loop_with` are why QIR needs no SSA construction (section 6.2): a merged value is the closure's return value, a loop-carried value is the closure's argument. Both push a CSE and row-cache scope (section 6.11) and pop it on exit. `loop_with` also records the preheader cursor for loop-invariant placement and flags the header `loop`. Every method is `#[track_caller]`, which is how provenance (section 6.12) gets the call site for free.

**Layer 4, SQL values.**

```rust
pub enum Validity { Always, Dyn(V<I1>) }
pub struct SqlVal { pub raw: Raw, pub valid: Validity, pub ty: SqlType }
pub enum Raw { I8(V<I8>), I16(V<I16>), I32(V<I32>), I64(V<I64>), I128(V<I128>),
               F64(V<F64>), Str(V<Str>, StrClass) /* ... */ }

impl SqlCx {
    pub fn binary(&mut self, fb: &mut Fb, op: BinOp, a: &SqlVal, b: &SqlVal) -> SqlVal;
    pub fn cast(&mut self, fb: &mut Fb, v: &SqlVal, to: &SqlType) -> SqlVal;
    pub fn cmp(&mut self, fb: &mut Fb, op: CmpOp, a: &SqlVal, b: &SqlVal, coll: Collation) -> SqlVal;
    pub fn is_true(&mut self, fb: &mut Fb, v: &SqlVal) -> V<I1>;   // NULL and FALSE collapse
}
```

`binary` is Tidy Tuples' `evaluateBinary`. It resolves the DuckDB result type, including decimal widening such as `DECIMAL(18,0) + DECIMAL(18,0)` → `DECIMAL(19,0)` (E §4.1 [snippet]). It picks the physical width, emits the checked or proven-safe operation, registers the error site with the SQL operator and operand types, and combines validity. When both inputs are `Always`, the result is `Always` and no validity code exists. Document 12 owns the semantic tables. This layer owns the rule that they are applied in exactly one place.

**Layer 3, tuples.** A `TupleLayout` fixes field offsets, validity bytes and the string storage class for a materialized row: a hash table entry, an aggregate state row, a sort run row, a stage buffer row.

```rust
pub struct TupleLayout { fields: SmallVec<[Field; 8]>, size: u32, align: u32 }
impl TupleLayout {
    pub fn pack(&self, fb: &mut Fb, dst: V<Ptr>, vals: &[SqlVal]);      // promotes transient strings
    pub fn lazy(&self, src: V<Ptr>) -> LazyRow;                          // no loads yet
    pub fn hash(fb: &mut Fb, keys: &[SqlVal]) -> V<I64>;                 // two CRC chains
    pub fn eq(&self, fb: &mut Fb, src: V<Ptr>, keys: &[SqlVal]) -> V<I1>;
}
```

`pack` is the one place transient strings are promoted: `rtcall @str_promote` for non-inline strings whose class is `transient`, a plain 16-byte store otherwise. Rule V10 (section 6.10) catches any translator that bypasses it.

**Layer 2, data structures.** Code-generating wrappers over structures whose algorithms are precompiled in `rudb-qc-rt`: `JoinHt`, `AggHt`, `DenseArray`, `SemiBitmap`, `StageBuf`, `SortKeyEnc`, `ResultSink`. Each has methods that emit the hot path (probe, update, append) and names the runtime functions for the cold path (grow, spill, flush). Document 10 specifies `JoinHt`'s layout, and a layout change is a change to one file here.

**Layer 1, operator translators.**

```rust
pub trait Translate {
    fn produce(&mut self, cx: &mut Cx, fb: &mut Fb);                   // sources and breakers' drain side
    fn consume(&mut self, cx: &mut Cx, fb: &mut Fb, row: &mut Row);    // one tuple, in registers
}
pub enum Tr { Scan(ScanTr), Filter(FilterTr), Project(ProjTr), Build(BuildTr),
              Probe(ProbeTr), Agg(AggTr), SortKey(SortKeyTr), Sink(SinkTr), VBatch(VBatchTr) }
```

Translators live in one `Vec<Tr>` per pipeline with parent indices, and dispatch is a `match`, not a `dyn` call. `cx.consume_parent(id, fb, row)` is the produce/consume hand-off.

## 7.3 Produce and consume over pipelines

**A pipeline's function has a fixed skeleton, and translators fill it in.** The source translator's `produce` emits the morsel prologue (hoisted guards, state loads) and the batch loop. For each qualifying tuple it builds a `Row` and calls `consume` on its parent. Each operator either consumes and forwards (filter, project, probe) or consumes and absorbs (build, aggregate, sink). The absorbing operator is the pipeline's sink. After the batch loop, the source emits the morsel epilogue: accumulators are flushed to state and the function returns `Ok`. This is HyPer's model with the loop boundaries document 05 fixes: one function, one morsel per call, batches of up to 1,024 inside.

**Columns are loaded as late as possible.** A `Row` is a small vector of slots, each either `Lazy { src, row_idx }` or `Loaded(SqlVal)`. The first `row.get(fb, col)` emits the load and caches it in the current scope. Leaving a scope drops the cache entries made inside it, so a value loaded inside a filter arm is never used where it does not dominate. The effect is EVOL's rule of loading attributes late and keeping them in registers (E §1.4): a column read only after a selective probe is loaded only for the tuples that survive it. This is late materialization at the register level. Document 10 extends it across joins, where probe payloads stay `Lazy` against the hash table entry until a later operator reads them.

**Register pressure is the one reason to load early.** When a pipeline carries more than about 12 live columns across a probe, the translator packs the extra ones into a stage buffer row instead of keeping them live (EVOL). That threshold is a starting value for document 17 to tune, not a measured number.

## 7.4 The scan boundary

**The generator never emits per-format decoding. Scans are precompiled kernels, and the generated body starts at a selection vector.** HyPer's Data Blocks scan was interpreted, vectorized and SIMD-evaluated on compressed data, and fed the compiled pipeline through a TID vector. It did not generate scan code, because formats × predicate kinds would multiply code size and compile time (E §2.1 [snippet]). The same argument holds here with more formats. The kernels live in `rudb-qc-rt/src/scan/`, monomorphized over (encoding, physical type, predicate family), with AVX2/AVX-512/NEON variants selected once at startup (B §9).

A kernel's contract:

```rust
extern "C" fn scan_k(st: *mut ScanState, m: *const Morsel, out: *mut Batch) -> u32; // n in 0..=1024
#[repr(C)] struct Batch { n: u32, sel: [u32; 1024], cols: [ColView; MAXC] }
enum ColView { Plain(*const u8), Codes { codes: *const u8, width: u8, dict: DictId },
               Runs { vals: *const u8, lens: *const u32, nruns: u32 },
               For { base: i64, deltas: *const u8, width: u8 }, Const(i128) }
```

It evaluates every conjunct of the form column-op-constant, column-in-set and IS [NOT] NULL on the scan's own columns, using zone maps to skip, SIMD on the encoded form, and it returns the qualifying positions. The generated body sees only survivors. Predicates the kernel cannot express (cross-column, string patterns beyond prefix, function calls) become residuals in the body (section 7.5).

**The morsel's encoding is known before the body runs, so the body is specialized per encoding.** Morsels align with row groups (document 03 section 3.7), so one morsel has one encoding per column. The version table (section 7.8) maps the morsel's encoding signature to a function version.

| Encoding | What the body gets | Predicates | Aggregation | Join key | Output |
|---|---|---|---|---|---|
| dictionary | codes, dictionary id | precomputed per dictionary into a code bitmap in state; one `load.bit` per tuple | group by code into a `DenseArray` when the dictionary has ≤ 65,536 entries | probe with codes when build and probe share the dictionary (fact), else decode | decoded at the sink, only for survivors |
| run-length | runs | evaluated once per run | `SUM += v * len`, `COUNT += len`, `MIN/MAX` once per run | expanded | expanded |
| frame of reference | base + narrow deltas | constants rewritten into delta space once per morsel (`c - base`) | narrow accumulators (section 7.7) | `base + delta` | decoded |
| constant | one value | evaluated once; the morsel is skipped or fully accepted | multiply by the count | once | broadcast |
| plain | values | as written | as written | as written | as written |

The dictionary row is where JOB and ClickBench get most of their encoding factor in document 02 section 2.9. `kind_type.kind = 'movie'` is evaluated once against the dictionary, not once per row. Bespoke OLAP's most-used techniques include dictionary predicate rewrite (71% of queries) and bitmap semi-joins (73.7%) (A §5.1, https://arxiv.org/abs/2603.02001).

## 7.5 Filters: representation and order

**Inside the kernel, the representation follows selectivity.** The first conjunct is evaluated over all rows into a bitmap. While the running selectivity estimate is above 0.15, later conjuncts are also evaluated over all rows as SIMD bitmaps and ANDed together. At or below 0.15 the kernel converts to a selection vector and evaluates the remaining conjuncts only on selected positions. NGOM measured selection vectors as best at ≤ 0.15 selectivity and hand-written SIMD bitmap evaluation as 3 to 11x faster above it (NGOM, https://db.cs.cmu.edu/papers/2021/ngom-damon2021.pdf). The kernel always hands the body a selection vector, because the body is tuple-at-a-time.

**In the body, residual predicates branch or predicate by selectivity.** A residual whose selectivity is below 10% or above 90% is a branch. Between those, it is evaluated branch-free, and the pass bit is folded into whatever consumes it: a `select` in an aggregate update, or an unconditional stage-buffer write that advances its cursor by `zext(pass)`. That is Vectorwise's micro-adaptivity baseline (MICRO, http://oai.cwi.nl/oai/asset/21351/21351B.pdf [snippet]). The selectivity used is the estimate at generation. Document 09 regenerates a version when the counters of section 6.5 show the estimate was on the wrong side of the band for most of a pipeline's morsels.

**Cheap residuals are fused. Expensive residuals are stages whose order can change without recompiling.** A residual is expensive if it calls the runtime (`LIKE '%x%'` over non-inline strings, regex), makes a `vcall`, or needs the out-of-line bytes of a string. Each expensive term is compiled as a loop from one selection vector to another. The order of the loops is read from a permutation slot in `PipelineState` through a `switch`, so the runtime can reorder them at a morsel boundary from measured cost and selectivity. PCQ showed permutable filters stay within 10% of optimal where a static order can be up to 4.4x worse, and that the code grows only about 20% going from 1 to 7 terms (PCQ, https://www.vldb.org/pvldb/vol14/p101-menon.pdf). The reordering rule is a function of the counters of the morsels already run, not a learned policy, so it respects document 03 section 3.9. Reordering conjuncts never changes the result set. It can change which error, if any, a query raises, and DuckDB itself does not promise more (E §4.1, §5.2). Document 12 states that contract.

**Stages re-pack sparse streams.** A stage buffer is filled across batches up to 1,024 entries, or to the end of the morsel, before the next stage runs. After a selective join, the next stage therefore runs over a full buffer and not over fifty nearly empty batches. It is the scalar analogue of lane refill, which gave up to 34% on a scan and 25% on probes (LANG, https://db.in.tum.de/~lang/papers/simd_divergence.pdf), and of DuckDB's chunk compaction, which gave up to 63% (COMPACT [snippet]).

## 7.6 Staged probes

**A probe into a table larger than its per-thread share of the last-level cache is preceded by a stage boundary. The probe itself runs in groups of 16 with software prefetch.** ROF places stages at the input of random-access operators whose structure exceeds cache. It found the prefetch group size best at 16, and reported up to 2.2x over pure fusion (ROF). Kersten et al. give the reason it matters: Tectorwise beat the fused Typer by 32% on Q9, a probe-heavy query, because independent loads overlap (KER18, https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf). JOB is almost entirely probes, so this is not optional for C6.

**The decision uses the built size, not the estimate.** A probe pipeline starts only after its build finished, so the table's size is an exact fact by then (ROF recommends deciding at probe-pipeline start; E §9.5). Probe pipelines are generated lazily when their builds finalize (document 09, Rule I3), so the generator reads the exact size and emits only the chosen version, with no guard. The threshold is document 10 section 10.6's: fused while the table fits the per-thread share of the last-level cache, staged otherwise. Cache sizes are read from the CPU at startup.

```
stage A (fused into the tuple loop, per surviving tuple):
    k = key(row); h = hash(k)
    buf.row[j] = row_idx; buf.h[j] = h; buf.k[j] = k; j += 1
    if j == 1024: run stage B; j = 0
stage B (over the buffer, groups of G = 16):
    for g in (0..j).step_by(G):
        for t in g..g+G: prefetch.r dir[slot(buf.h[t])]              // phase 1
        for t in g..g+G: w[t] = dir[slot(buf.h[t])]                  // phase 2
                         if tag_hit(w[t], buf.h[t]): prefetch.r chain_head(w[t])
                         else w[t] = 0
        for t in g..g+G: if w[t] != 0: walk chain from w[t], compare buf.k[t],   // phase 3
                         for each match: consume(Row::lazy(buf.row[t], entry))
at morsel end: run stage B on the partial buffer
```

`w` is 16 values, which is too many registers, so it is a 16-slot array in the stage buffer. Phase 3's consume continues the rest of the pipeline tuple-at-a-time. When the next operator is another large probe, its stage A runs inside this phase 3, so a chain of JOB probes becomes a chain of stages. Semi-joins, anti-joins and existence checks against a `SemiBitmap` or a small `DenseArray` stay fused: they are a load and a bit test on data that fits in cache. Document 10 owns the Bloom tag layout and the reduction filters that run before any probe.

## 7.7 Specialization on facts

**A specialization relies on a fact. An exact or certified fact needs no guard. An estimated fact, or a fact that holds per morsel rather than per column, gets a guard, and a failed guard deoptimizes the morsel.** Facts come from document 04. The table lists the specializations the generator knows.

| Fact | Specialization | Guard | Checked | Fallback |
|---|---|---|---|---|
| column NOT NULL (schema) | validity `Always` | none (exact) | | |
| morsel has no NULLs (`null_count == 0`) | validity `Always` | header field | per morsel, hoisted | nullable version |
| dictionary encoded, ≤ 65,536 entries | code bitmaps, code-indexed `DenseArray` | morsel encoding and dictionary id | per morsel, hoisted | decoded version |
| dense integer key range `[lo, hi]` on a built table, `hi - lo` ≤ 4x rows | `DenseArray` indexed by `k - lo` instead of a hash table | none: the range check is the lookup, and out of range means no match | | |
| build key unique (exact after build, or a primary key) | probe stops at the first match; no expand loop | none (exact) | | |
| value range fits a narrower width (zone map) | `i64` instead of `i128`; unchecked `add`/`mul` where the bound proves safety; morsel-local accumulators | zone-map bound in the morsel header | per morsel, hoisted | wide version |
| narrow accumulator probably will not overflow (estimated) | `i64` accumulator with `sadd.ov` | the overflow edge | per tuple | wide version |
| sorted or clustered on the join or group key | merge join, streaming aggregation | morsel's first key ≥ previous morsel's last key | per morsel | hash version |
| all strings in the morsel ≤ 12 bytes | inline-only compare and hash, no `rtcall` | max-length statistic | per morsel, hoisted | general version |

**Deoptimization is at morsel granularity and must be side-effect free.** A guard returns `Status::Deopt(G)`. The scheduler reruns the same morsel on the fallback version (document 09), which is legal only if the failed attempt changed nothing the fallback will see. Rule V9 enforces this. In practice, per-morsel guards are hoisted to the entry and precede every effect. The one per-tuple guard, `sadd.ov` on a narrow accumulator, is used only when the accumulator is morsel-local: it sits in a register, is flushed at the morsel epilogue, and is abandoned on deopt. Grouped aggregates whose state lives in a shared table do not get per-tuple speculation. They are sized by the value-range fact or run wide.

**The first fallback runs on `interp`.** A fallback version is generated as QIR on the first guard failure. That costs the generation budget, well under a millisecond, and the morsel reruns immediately on `interp` while `direct` compiles the fallback in the background. A wrong estimate therefore costs one slower morsel and a background compile, never a stall.

## 7.8 Versions and code size

**At most four versions per pipeline.** A version is a combination of specializations: encoding signature, fused or staged probe, narrow or wide arithmetic. The generator emits the version the facts predict. It emits a second one only when section 7.6's 4x ambiguity rule applies, and others lazily on guard failure. The version table in the module maps a morsel's signature (encoding bits, null bits, zone-map class) to a function. It is consulted by the runtime at the morsel boundary, not by generated code. Excalibur needed a code cache to make fragment reuse pay, about 26x for Q1 with 64 cached fragments (EXCAL, https://www.vldb.org/pvldb/vol16/p829-boncz.pdf). Four versions per pipeline plus document 09's cache is our bound on the same trade-off.

**Adaptivity is spent only where Amdahl allows.** A 10x speedup on 40% of a query gives at most 1.5x overall, by `S = (φ + (1-φ)/y)^-1` (EXCAL; E §1.8). Lazy versions are generated only for pipelines whose share of the query's measured time exceeds 30%, a threshold document 17 should validate.

**Code size is bounded linearly.** Rules:

1. A function over 20,000 QIR instructions (section 6.6) is split at a stage boundary into two functions that communicate through the stage buffer. A stage boundary is always available, because one can be placed before any operator.
2. An `IN` list or `OR` chain over one column with more than 16 constants becomes a constant set in state: a code bitmap for dictionary columns, a sorted array or hash set otherwise. It is probed once per tuple. EVOL's customer query with 300,000 disjunctions (https://vldb.org/pvldb/vol14/p3207-neumann.pdf) compiles to a set probe, not 300,000 compares.
3. Cold paths (trap stubs, deopt exits, NULL slow paths, string slow paths, runtime-call error checks) go to the cold block list and are shared per error site.
4. Expression trees deeper than 64 nodes, or larger than 2,000 QIR instructions, are outlined into internal functions called per tuple. Spark's all-or-nothing fallback at 8,000 bytes of bytecode (A §4.1) is the failure this avoids.

## 7.9 Operator translations

**Filter.** `consume(row)`: `let c = sql.is_true(expr(row)); fb.if_(c, |fb| parent.consume(row))`. The branch-free and staged forms of section 7.5 replace `if_` when their thresholds apply.

**Project.** Adds `Slot::Expr` entries to the row that are evaluated on first use, like lazy columns. A projected expression that no operator reads costs nothing, and one that is read twice is computed once through CSE.

**Hash build.** Generated code hashes the key, packs the entry with `TupleLayout::pack` into the worker's reserved chunk (bump pointer, bounds check, `rtcall @build_next_chunk` cold on exhaustion), and stores the hash. It does not insert. After all build morsels finish, the runtime sizes the directory exactly and links entries with a precompiled, key-agnostic CAS loop over the stored hashes. That is the morsel-driven two-phase build (MORSEL, https://db.in.tum.de/~leis/papers/morsels.pdf). It keeps insert out of generated code and makes the facts in section 7.7 (unique, dense range) exact before any probe is generated.

**Probe, lookup.** For inner, semi, anti and mark joins against a unique key: hash, tag test, walk, compare, then one branch on found. The section 6.8 example is the fused form. Anti-join consumes on miss. Mark join consumes always, with the mark as an `i1` column.

**Probe, expand.** For non-unique keys: the chain walk becomes a `loop_with` that calls `parent.consume` once per match, with `poll` on its back edge (rule V11). The payload stays `Lazy` against the entry pointer.

**Aggregate update.** Ungrouped aggregates are `loop_with` carried values in registers for the whole morsel, flushed once at the epilogue. That is how Q6 in section 7.12 has no memory traffic for its accumulator. Grouped aggregates:

- Dense or dictionary keys index a `DenseArray`.
- Other keys probe a thread-local `AggHt` with the same expanded probe as a join. On a miss, the entry is appended to reserved space, and `rtcall @agg_spill` runs cold when the reservation is full (MORSEL's partitioned pre-aggregation).
- The update body is straight-line code across all aggregates, with NULL checks specialized away. `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`, `ANY_VALUE` and `BOOL_AND/OR` are inline. Rare aggregates get an opaque state slot and an `rtcall` to the first engine's update, as document 03 section 3.4 says.

The choice between thread-local partitioned and global ticketed tables is made at runtime from observed cardinality. GHT measured the global strategy 1.78x faster at low cardinality on 48 threads (GHT, https://arxiv.org/abs/2505.04153). The generated update body is the same for both. Only the slot lookup differs. Document 11 owns the policy.

**Sort-key encode.** Generates one function per query that writes a fixed-width, memcmp-comparable key:

- a NULL byte per nullable column;
- big-endian integers with the sign bit flipped;
- inverted bytes for DESC;
- a string prefix, with ties sent to a generated comparator.

The width is rounded up to 8, 16, 24 or 32 bytes plus a payload index, and the sort itself is a precompiled kernel monomorphized over that width. DuckDB 1.4's move to compile-time fixed-width key structs sped sorting up 2.7x on random integers, 10.4x on sorted ones and 3.4x on TPC-H SF100 (DSORT25, https://duckdb.org/2025/09/24/sorting-again). Top-N adds an inline compare against the current k-th key in state and skips tuples that cannot enter.

**Result sink.** Writes values into the output chunk buffers in the shared result format of document 03 section 3.3, at a cursor in state, and makes a cold `rtcall @sink_flush` when a chunk fills. Strings are promoted by `pack`, because the client outlives the morsel.

## 7.10 `vcall` batching

**A function without an inline translator runs as the first engine's vectorized kernel, over up to 1,024 tuples at a time, behind a stage boundary.** It is the mechanism that lets the router of document 03 section 3.4 accept every scalar function from C1.

```
stage A (in the tuple loop): for each tuple reaching the call:
    argbuf0[j] = a0(row); argbuf1[j] = a1(row); valid bits set per arg
    buf.row[j] = row_idx; buf.carry[j] = values computed before the call that are used after it
    j += 1; if j == 1024: flush
flush:
    st = vcall @k(argbuf0, argbuf1, outbuf), j        ; mayfail: status checked, error slot filled
    for t in 0..j: row = Row::from_stage(buf, t); row.set(call_id, outbuf[t], outvalid[t]);
                   parent.consume(row)
at morsel end: flush the partial buffer
```

Arguments are packed densely: only tuples that reach the call are copied, so the kernel sees no selection vector and runs its fastest variant. Column slots needed after the call are reloaded by row index, which is one indexed load. Computed values are carried in the buffer. The kernel's errors are the first engine's errors, so their messages match DuckDB's by construction.

The cost is the kernel's own cost plus one store and one load per argument per tuple. For string functions that allocate, it is dominated by the kernel. For cheap arithmetic, it is two to three times the inline cost [derived: the copy is two memory operations against a one-instruction operation]. That is what makes the inline list in document 03 a profiling decision and not a coverage decision. Every `vcall` has a counter, and `rudb-bench` reports the top `vcall` kernels by time per suite, which is the inline translator backlog.

## 7.11 Generation speed rules

1. **One pass, no backtracking.** No translator reads emitted QIR. A decision that seems to need it is moved into A5 or A6.
2. **No allocation per instruction.** `FuncBuilder` arenas, CSE tables, row caches and scope stacks are pooled per worker thread and cleared by a generation counter, not by freeing. `Row` is a `SmallVec` with 16 inline slots. The per-pipeline `Vec<Tr>` is the only allocation proportional to plan size.
3. **No `dyn` in the inner generator.** Operators dispatch through an `enum`. SQL-value operations dispatch on the `(op, physical type)` pair through a `match` the Rust compiler turns into a jump table.
4. **No strings on the hot path.** Value names are `&'static str` hints stored only in debug builds. Proxies and kernels are identified by integer ids resolved at build time.
5. **Every table is sized from A6 up front:** operator count, expression node count and column count bound the instruction count to within a small factor.
6. **Measured on every commit.** `rudb-bench` records generation ns per QIR instruction and total generation time per JOB query. CI fails when the JOB median exceeds 0.1 ms or any query exceeds 0.5 ms, which is the generation share of gate G1 (document 02 section 2.8).

## 7.12 End to end: TPC-H Q6

```sql
SELECT sum(l_extendedprice * l_discount) AS revenue
FROM lineitem
WHERE l_shipdate >= DATE '1994-01-01' AND l_shipdate < DATE '1995-01-01'
  AND l_discount BETWEEN 0.05 AND 0.07 AND l_quantity < 24;
```

**Physical plan fragment (A5, abbreviated).** Dates are days since 1970-01-01. The decimals are `DECIMAL(15,2)`, stored as `i64` in hundredths [background].

```
p0  scan lineitem cols=[l_shipdate, l_discount, l_quantity, l_extendedprice]
      kernel=ranges3<i32,i64,i64>  shipdate in [8766, 9131)  discount in [5, 7]  quantity < 2400
      facts  F1 l_extendedprice NOT NULL (exact)   F2 l_discount NOT NULL (exact)
             F3 per-row-group max |l_extendedprice| (zone map, per morsel)
             F4 l_discount in [5, 7] after the kernel (exact, from the predicate)
    agg ungrouped  revenue = sum(l_extendedprice * l_discount)  -> DECIMAL(38,4), i128 state
```

The product of two `DECIMAL(15,2)` values is a scale-4 decimal wider than 18 digits, so DuckDB computes it as a 128-bit value, and `SUM` over it accumulates in `i128` [background; document 12 pins the exact result types]. The generic version does exactly that per row: an `i128` multiply and a checked `i128` add.

**The specialization.** When F3 bounds `|l_extendedprice|` below 2^24 hundredths in a morsel, each product is below 2^24 × 7 < 2^27. A morsel's sum of at most 16,384 = 2^14 products is below 2^41. So an `i64` accumulator with unchecked arithmetic is exact for the whole morsel `[derived]`, and only the per-morsel fold into the `i128` state needs a checked add. The guard is one compare of the morsel header's zone-map maximum, hoisted to entry.

**QIR (A7), the narrow version.**

```
func @q6.p0.narrow(ptr %st, ptr %m) -> i32  version=narrow  plan=#3
block b0(ptr %st, ptr %m):
  %zmax = load.i64 [%m + 48]                    ; zone-map max |l_extendedprice| for this morsel
  %fits = icmp.ult i64 %zmax, 16777216          ; 2^24
  guard %fits, !G0                              ; fallback q6.p0.generic
  %sel  = load.ptr [%st + 136]                  ; batch buffer, filled by the kernel
  %ep   = load.ptr [%st + 144]                  ; l_extendedprice view
  %dp   = load.ptr [%st + 152]                  ; l_discount view
  br b1(0)
loop(1) b1(i64 %acc):                           ; morsel batch loop, bounded by the morsel
  %n    = rtcall @scan.ranges3.i32_i64_i64(%st, %m) -> i32  mayfail
  %none = icmp.eq i32 %n, 0
  brif %none, b3(%acc), b2(0, %acc)
loop(2) b2(i32 %i, i64 %a):                     ; n > 0 here
  %r    = load.i32 [%sel + %i*4]
  %e    = load.i64 [%ep + %r*8]
  %d    = load.i64 [%dp + %r*8]
  %p    = mul i64 %e, %d                        ; < 2^27 by F3, F4
  %a2   = add i64 %a, %p                        ; < 2^41 per morsel
  %i2   = add i32 %i, 1
  %more = icmp.ult i32 %i2, %n
  brif %more, b2(%i2, %a2), b1(%a2)
block b3(i64 %sum):
  %w    = sext i64 %sum -> i128
  %s    = load.i128 [%st + 32]                  ; this worker's revenue state
  %s2   = sadd.t i128 %s, %w, !E0              ; the only overflow check that can fire
  store.i128 [%st + 32], %s2
  ret 0
```

The hot loop is eight QIR instructions per qualifying tuple: an index load, two value loads, one multiply, one add, the loop counter, a compare and a branch. There is no NULL code (F1, F2), no overflow branch (F3, F4) and no memory traffic for the accumulator. The four predicates never appear, because the kernel evaluated them on the encoded columns with SIMD.

**The honest expectation.** Q6 is a selection-bound scan. Kersten et al. measured it at 11 cycles per tuple for both the compiled and the vectorized engine, with SIMD selection worth 1.4x end to end (KER18). ROF's 5.4x over Vectorwise on Q6 came from its SIMD predicate stage plus prefetching (ROF). So the compiler's contribution to Q6 is the narrow fused aggregate. The kernel and the zone maps carry most of the win, and document 17 should attribute Q6 accordingly, not credit fusion.

## What we should take from this document

The generator is Tidy Tuples in Rust: operator translators, data structures, tuples, SQL values, and a typed builder, with NULL, overflow and cast semantics confined to the SQL-value layer. Structured `if_else` and `loop_with` helpers make SSA with block parameters directly, so there is no SSA construction and no backtracking. At 10 ns per instruction, generation costs the same order as the frontend, never more.

The scan boundary is fixed: precompiled SIMD kernels per encoding evaluate column-versus-constant predicates and hand a selection vector of up to 1,024 positions to a tuple-at-a-time body. The body is specialized to the morsel's encoding: codes, runs, frame of reference, constants.

Filters use a selection vector at or below 0.15 selectivity and SIMD bitmaps above, inside the kernel. In the body they branch outside 10-90% and are predicated inside it. Expensive terms become reorderable stages, as in PCQ.

Probes into tables larger than their per-thread share of the last-level cache are staged ROF-style, with buffers of 1,024 and groups of 16. The choice between fused and staged is made from the table's actual built size. Build is append-only in generated code, and linking is a precompiled pass, which makes uniqueness and density exact facts before probes are generated.

Every specialization names its fact and, unless the fact is exact, its guard. Guards are hoisted per morsel wherever possible. Deoptimization reruns the morsel with no side effects, on `interp` first while the fallback compiles. Versions are capped at four per pipeline and code size is linear in plan size, which keeps a guess that fails cheap: it costs one slower morsel and a background compile.
