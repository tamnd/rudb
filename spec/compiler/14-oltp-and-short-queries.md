# OLTP and short queries

Every other document in this folder assumes a query runs long enough to amortize its compile. This one is about the queries that do not: a TPC-C statement that does a microsecond of work, a prepared lookup issued a million times, and the first morsel of any query on a cold cache. For these, the compiler wins only if it almost never compiles, and the design question is how to make reuse the common case.

## 14.1 The problem, in instructions

**Statement overhead, not execution, is where short queries spend their time.** Document 02 section 2.7 gives the frame:

- A vectorized engine spends 10^5 to 10^6 instructions per statement on parsing, planning and executor setup.
- Compiled procedures bring a whole transaction to about 10^4 instructions.
- Hekaton: "To go 10X faster, the engine must execute 90% fewer instructions." It also measured a B-tree lookup at thousands of instructions and a simple interpreted transaction at several hundred thousand (http://sites.computer.org/debull/A14mar/p22.pdf).

`../engine-v4/13-the-point-path.md` section 13.1 measured what that means for rudb today, through each shell, including printing:

| operation | SQLite | rudb | DuckDB |
|---|---|---|---|
| `SELECT 1` | 0.9 µs | 45 µs | 176 µs |
| point read by key | 2.9 µs | 223 µs | 429 µs |

Its section 13.2 traces the 45 µs. `Prepared` saves the parse and nothing else. Binding, optimization, physical planning and pipeline construction run on every execution, a catalog write lock is taken per statement, and the pipeline machinery allocates 8,192-row chunks for a one-row answer.

**The references.**

- HyPer, 2011: LLVM-compiled TPC-C at 169,491 tps with 0.81 s total compile, because each transaction compiled once and ran forever (https://www.vldb.org/pvldb/vol4/p539-neumann.pdf).
- Hekaton: native procedures at 15.7x the throughput of interpreted SQL Server (research-notes D section 5).
- Umbra: 27,000 TX/s on one thread at 100 warehouses with snapshot-isolation MVCC, and 413,300 at 48 threads (https://www.vldb.org/pvldb/vol15/p2797-freitag.pdf).

All three compile once per statement or procedure, never per execution.

**What the compiler cannot fix.** Umbra's ablation measured in-place updates alone at 5.1x (research-notes D section 5). Index structure, the log, group commit and concurrency control are also storage decisions. `../engine-v4/` owns all of them. This document owns the path from "the client calls execute" to "the first index probe", and the expression and control-flow code around the storage calls.

## 14.2 Division of labor with engine-v4

**engine-v4 defines the point plan. The compiler compiles it and everything the point plan does not cover.** `../engine-v4/13-the-point-path.md` section 13.4 already specifies:

- **A closed list of point shapes:** `Lookup`, `Range`, `UpdateOne`, `DeltaOne`, `DeleteOne`, `InsertRows` and `Upsert`.
- **Plans that are bound and optimized once**, with validity checked by one load of the catalog version.
- **Parameter-type specialization**, keeping up to four specializations per statement.
- **Its own executor.** A small Rust struct run on the calling thread, without pipelines, chunks, morsels or the scheduler, with expressions evaluated in a flattened interpreted form.

This document takes that design as given and does not duplicate it. The compiler adds three things.

1. **Compiled point functions.** The same point plan, lowered to one QIR function with parameter loads, key encoding, the expressions of `SET`, `WHERE` residuals and `RETURNING`, and the storage calls in order. It pays where the point plan interprets expressions. TPC-C's New-Order stock update is `s_quantity = CASE WHEN s_quantity - ? >= 10 THEN s_quantity - ? ELSE s_quantity - ? + 91 END`, and its order-line insert computes `ol_amount` from two values.
2. **Compiled short pipelines.** Statements outside the closed list that are still tiny: Stock-Level's join with `COUNT(DISTINCT)`, Delivery's `MIN(no_o_id)` and `SUM(ol_amount)`, Order-Status's `ORDER BY ... LIMIT 1`. These are ordinary compiled-engine plans that run with single-morsel pipelines inline on the calling thread (document 13 section 13.7), with no pool hand-off.
3. **The statement cache for unprepared SQL.** It gives repeated text the same reuse a prepared statement gets (section 14.4).

**The compiled point function must earn its place by measurement.** engine-v4's point plan already removes most of the overhead, and its budget is 200 to 700 ns for a point read (section 13.3 there). A compiled function saves expression interpretation and some dispatch. On a pure `SELECT c FROM t WHERE k = ?`, the saving may be small next to the index probe.

C11 measures both paths per TPC-C statement. The router sends a point shape to the compiled function only where the function is measurably faster. That is recorded per shape, not assumed.

## 14.3 What is baked in and what is loaded

**Values are parameters, and shapes are code.** Research-notes E section 6.3 puts the split this way: bake types, nullability, collation and pattern shape, and pass values through the query state. Document 00 settles it the same way. Concretely:

| thing | baked into code | loaded from the parameter block |
|---|---|---|
| parameter SQL types | yes, one specialization per type vector (engine-v4's four-entry list) | |
| parameter nullability | yes: a NULL parameter selects a variant, because `k = NULL` is a different plan | |
| literal constants in the SQL | yes, when the statement is prepared with them | |
| `?` values | | yes, 8 or 16 bytes per slot, strings as a 16-byte header |
| `LIKE` pattern given as `?` | its shape (section 12.7): prefix, suffix, contains, segment count | segment bytes and skip-table seeds |
| `IN (?, ?, ?)` | the list length | the values; a perfect-hash table is built per execution only above 16 values |
| `LIMIT ?` | whether it is present | the value |
| catalog version | checked, not baked | |

A parameter value whose shape differs from the compiled one misses the specialization list and compiles or interprets a new one. Examples are a `LIKE ?` bound first to `'abc%'` and then to `'%abc%'`, or a NULL where a value was seen. The four-entry LRU from engine-v4 bounds how many specializations one statement keeps.

**The parameter block is one contiguous buffer, filled by the API, read by the function.** Its layout is fixed at specialization:

- fixed-width slots at plan-fixed offsets;
- a string slot holding the 16-byte header, with its payload in the block's tail;
- a null bitmap at offset 0.

The function reads it through `state.params` (document 13 section 13.2). Filling it costs a type check and a store per parameter.

## 14.4 The statement cache

**Two levels: the prepared handle points straight at its entry, and a process-wide cache serves repeated unprepared text.**

```rust
pub struct StmtEntry {
    pub key: StmtKey,
    pub catalog_version: u64,                      // checked on every execution: one load, one compare
    pub a4: Arc<LogicalPlan>,                      // bound and rewritten once
    pub point_plan: Option<PointPlan>,             // engine-v4's, when the shape is on the list
    pub specs: [Option<Spec>; 4],                  // per parameter-type vector, LRU
}
pub struct Spec {
    pub param_types: SmallVec<[TypeId; 8]>,
    pub a5: Arc<PhysicalPlan>,
    pub code: AtomicPtr<Module>,                   // null until compiled; interp or point plan meanwhile
    pub per_worker: PerWorker<Counters>,           // executions and nanoseconds; no shared writes
    pub compile_requested: AtomicBool,             // written once per spec lifetime
}
pub struct StmtKey {
    pub fingerprint: u128,                         // prepared: the SQL text; unprepared: normalized text
    pub settings: u64,                             // hash of the settings the binder read
    pub search_path: u64,
}
```

**The settings in the key are the binder's read set, not all settings.** The binder records which settings it consulted while binding the statement: `integer_division`, the default collation, the time zone, and anything else whose value changes A4 [background]. The key hashes their values. A change to an unrelated setting does not invalidate the entry, and a change to a relevant one cannot be missed. This is stricter than hashing a hand-maintained list, which is the bug that list would eventually have.

**Invalidation is by catalog version, lazily.** DDL publishes a new catalog version (engine-v4 section 13.4). An entry whose version is stale is rebound on its next execution, and all its specializations are dropped. That is what the DuckDB pin does for a prepared statement across a schema change.

DuckDB reportedly also rebinds or replans on `EXECUTE` in cases where nothing changed (issue #17237, https://github.com/duckdb/duckdb/issues/17237 [snippet]). PR #14616 removed one such case, prepare and execute in different transactions (https://github.com/duckdb/duckdb/pull/14616 [snippet]). Honoring the cache is itself a win over the reference.

**Unprepared repeated SQL gets a cache too, and it is verified, not assumed.** ClickBench and the JOB harness issue the same text several times, and many OLTP clients do not prepare. For unprepared text:

1. The lexer (not the parser) produces a token stream. Literals are replaced by typed placeholders, where the placeholder type is DuckDB's literal type, including the value-dependent integer literal class [background].
2. The normalized stream is hashed into `fingerprint`.
3. On a miss, the statement is planned twice, once as written and once with the literals lifted to parameters. The entry is created only if the two A4 texts are equal modulo parameter slots.

A4's textual form makes that comparison exact (document 03 section 3.1). Literals that change the plan fail the check and stay in the key as text: a `LIKE` pattern, a `LIMIT` that enables top-N, a literal folded into a partition bound. The check costs one extra bind on a miss and nothing on a hit.

**Eviction is by code bytes, LRU, with a budget that defaults to 64 MB of machine code** [derived]. Evicted modules are freed by the epoch scheme in document 13 section 13.4.

Entries for prepared handles are owned by the handle and never evicted while it lives. Their compiled code can be evicted, in which case the handle falls back to the interpreter or the point plan until the next compile.

## 14.5 When to compile a short statement

**Rule: compile a specialization once its cumulative execution time exceeds the predicted compile time.** This is the ski-rental rule. It spends at most twice the optimal amount against any future number of executions [derived]. It is ADAPT's extrapolation (https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf) keyed on executions instead of morsels, which research-notes E section 8.3 recommends for OLTP.

The inputs:

- **Cumulative execution time.** The per-worker nanosecond counters, summed when a worker's own counter crosses a local threshold.
- **Predicted compile time.** QIR size times `direct`'s measured per-instruction cost, from document 09's model.

The first worker to observe the crossing sets `compile_requested` with one CAS and submits a low-priority compile task. Every execution before the pointer is published runs on the point plan or the interpreter.

What it means in practice:

- A TPC-C statement at 5 µs interpreted, with a 100 µs `direct` compile, is compiled after about 20 executions.
- A one-shot ad hoc statement is never compiled.
- An analytical query that runs for seconds is compiled per pipeline by document 09's policy before its second morsel, regardless of this rule. It applies only to statements whose whole execution is shorter than the first tier decision point.

**The tier for a short statement is `direct`, never `clif` or `llvm`, unless the statement is also long.** A point function has no loop worth optimizing. Its time is in the calls to storage.

## 14.6 The point function

**One function, no morsel, no pipeline state machine.** EVOL describes the same case: provably small OLTP queries compile to a single function (https://vldb.org/pvldb/vol14/p3207-neumann.pdf). Here the function uses the one ABI of document 13 with `morsel = null`.

TPC-C's New-Order stock step, as prepared by the driver:

```sql
UPDATE stock
   SET s_quantity = CASE WHEN s_quantity - ?3 >= 10 THEN s_quantity - ?3 ELSE s_quantity - ?3 + 91 END,
       s_ytd = s_ytd + ?3, s_order_cnt = s_order_cnt + 1,
       s_remote_cnt = s_remote_cnt + ?4
 WHERE s_w_id = ?1 AND s_i_id = ?2
RETURNING s_quantity, s_dist_01, s_data;
```

The function it compiles to, abbreviated:

```
fn point.stock_update(%state, %morsel=null) -> status
  %p     = load ptr [%state + 16]                 ; parameter block
  %w     = load i32 [%p + 8]   %i = load i32 [%p + 12]
  %q     = load i32 [%p + 16]  %r = load i32 [%p + 20]
  %key   = keyenc.i32i32 %w, %i                   ; normalized key, 8 bytes, inline
  %st    = call rt_row_lock(%state, idx.stock_pk, %key, 8, out %rid, out %row)   ; engine-v4
  br.nz %st, ret.status
  %sq    = load i32 [%row + off.s_quantity]       ; hot row: direct column access (engine-v4 layout)
  %d     = sub.chk.s i32 %sq, %q                  -> ovf.0
  %ge    = icmp sge i32 %d, 10
  %d91   = add.chk.s i32 %d, 91                   -> ovf.1
  %nq    = select %ge, %d, %d91
  %ytd   = add.chk.s i64 [..s_ytd], sext %q        -> ovf.2
  ...                                              ; all new values computed before any write
  %st2   = call rt_row_update(%state, %rid, colset.{q,ytd,oc,rc}, %newvals)       ; undo + log + in-place
  br.nz %st2, ret.status
  store.sink %out, %nq, [..s_dist_01], [..s_data] ; RETURNING from the row image, no second lookup
  ret ok
```

**Compute every new value before the first write.** Errors in `SET` expressions (overflow, cast failure, a `CHECK` constraint) surface before the storage call that writes. So an erroring statement has written nothing and needs no undo of its own. This is document 13 section 13.4's entry-guard principle applied to DML.

**The index probe is a storage call with an inline key.** Hekaton generated per-table hash and compare callbacks. engine-v4's keys are normalized and byte-comparable, so no per-table compare code is needed. The compiled function encodes the key inline (a byte swap and a sign flip per integer column) and calls `rt_index_lookup` or `rt_row_lock`.

Inlining the B+tree descent itself into generated code is deliberately not done at C11. The descent is engine-v4's code, and duplicating it in generated form would put a second implementation of the index on the correctness path. It is listed in document 20, pending a measurement that shows the call boundary matters.

**Column access to a hot row is inline; a frozen row is a call.** engine-v4 hands back either a pointer to a hot row image with a layout fixed per table version, or a row id in a frozen row group. The function reads the first with plan-fixed offsets and calls the storage decoder for the second. The layout is part of the specialization, and a layout change is a catalog version change.

**Results go straight to the caller's buffer.** A point `SELECT` or `RETURNING` writes into the API's row sink (engine-v4 section 13.4: "hand the row back, into the caller's buffer, no chunk"), not into a 2,048- or 8,192-row chunk.

## 14.7 The instruction budget

**The C11 gate: under 20,000 instructions of statement overhead per transaction, summed over its statements, from cache lookup to first index probe** (document 02 section 2.7, document 18 section 18.13). Delivery is the transaction that makes it hard, because it has the most statements. Statement counts, for a straightforward driver with an average of 10 items per order [derived from the TPC-C transaction profiles]:

| transaction | mix | statements | budget per statement |
|---|---|---|---|
| New-Order | 45% | 6 + 4 per item = 46 | 434 |
| Payment | 43% | 7 to 8 (60% by last name) | 2,500 |
| Order-Status | 4% | 3 to 4 | 5,000 |
| Delivery | 4% | 7 per district × 10 = 70 | 285 |
| Stock-Level | 4% | 2 | 10,000 |

So the per-statement overhead must be under about 285 instructions. The allocation, as targets to be measured in C11:

| step | target instructions |
|---|---|
| API entry, handle to `StmtEntry` | 20 |
| catalog version check | 5 |
| specialization match on the parameter type vector | 15 |
| parameter type checks and copy into the block, per parameter | 10 to 20 (3 parameters typical) |
| snapshot: load the durable cut, store the worker's epoch (engine-v4 section 13.5) | 15 |
| epoch publish for code reclamation (document 13 section 13.4), shared with the snapshot store | 0 |
| call into the compiled function, prologue, parameter loads | 20 |
| key encoding | 10 to 30 |
| **total to the first probe** | **about 150 to 200** |

The same count for today's `Prepared::run` path starts at binding and is several orders of magnitude larger, as engine-v4 section 13.2 traced. The gap is not closed by making any of those steps faster. It is closed by not doing them at all on a hit.

**How it is measured before engine-v4 lands.** `rudb-bench` runs the five transactions against a stub storage whose `rt_index_lookup` returns a fixed row, and counts `instructions:u` per transaction with hardware counters. Stub calls are counted and subtracted by a calibration run. That is the "instrumentable before the storage work exists" property document 02 asks for. Throughput at 100 warehouses is reported only once engine-v4 is in, against the target in document 02 section 2.7: within 2x of Umbra's 27,000 TX/s on one thread.

**No shared cache line is written on a hit.** engine-v4 section 13.5 makes that a rule for the point path, and every structure here obeys it:

- the handle holds its entry;
- counters are per worker;
- the compile flag is written once per specialization lifetime;
- code reclamation rides on the snapshot epoch.

A contended atomic on the hit path would cap the process at roughly ten million statements per second, whatever the core count (engine-v4 section 13.5).

## 14.8 Short analytical queries and the first morsel

**G1 applies to every query: time to first morsel on a cache miss is at most 1 ms at the median and 5 ms at the maximum** (document 02 section 2.8). Measured from A4 handed to the router to the first morsel entering generated code, it decomposes into shares owned by other documents. The allocation below is this document's proposal, to be checked by C0 and C3:

| stage | owner | median share |
|---|---|---|
| physical plan (A5) | document 04 | 150 µs |
| pipelines and state layout (A6) | document 05 | 50 µs |
| QIR generation for all pipelines (A7) | documents 06 and 07 | 100 µs (document 03 section 3.5) |
| `direct` for the first pipeline only | document 08 | 300 µs |
| runtime setup: reservations, state instantiation, first morsel dispatch | document 13 | 100 µs |
| slack | | 300 µs |

**Only the first pipeline needs to be compiled before the first morsel.** Later pipelines, usually probes and aggregations after the builds, compile in the background while the first one runs. Each later pipeline is compiled on `direct` by the first worker to claim it, when its dependencies finish (document 09, Rules I2 and I3). Its compile time is counted in the query's total compile budget, the second half of G1. Compile latency is overlapped rather than summed, which is what makes a 16-join JOB query fit.

**Queries predicted to finish in under a millisecond run on the interpreter** (document 02 section 2.8). Examples are `SELECT 1`, a count answered from metadata, and a filter on a small dimension table. Their fixed cost is A5, A6 and QIR generation, about 300 µs by the table above. That is still too much for a statement issued thousands of times a second. The statement cache is what fixes it, not a faster interpreter: a hit skips A2 through A7 and goes straight to the cached module or the point plan.

**On a warm cache, benchmark reruns skip compilation, and that is reported, never hidden.** ClickBench and the JOB harness run each query several times. A hit on the second run is legitimate plan caching, the same thing a prepared statement does. Document 17 requires every report to state whether the code cache was cold or warm per run. The G1 gate and the headline JOB number are cold.

## What we should take from this document

A short statement wins only by reuse. The design makes reuse the default: prepared handles point straight at a cache entry, unprepared text is normalized and cached behind an A4-equality check, and a hit skips binding, planning, pipeline construction and code generation.

engine-v4 owns the point plan, the storage calls and the rules against shared writes. The compiler adds compiled point functions for shapes with expressions worth compiling, compiled single-morsel pipelines for the TPC-C statements outside the point list, and the statement cache. The compiled point function replaces the point plan only where C11 measures it faster.

Values are parameters and shapes are code. Types, nullability, `IN`-list length and `LIKE` shape are baked in. Values are read from a parameter block. The cache key is the fingerprint, the binder's settings read set and the search path, with the catalog version checked on every execution and up to four parameter-type specializations per entry.

A statement is compiled once its cumulative execution time exceeds its predicted compile time. That is ski rental, with at most 2x regret, and it is always to `direct`. One-shot statements are never compiled. DML computes every new value before its first write, so an erroring statement has written nothing.

The gate is instructions, not throughput. Delivery's 70 statements set the per-statement bar at about 285 instructions from cache lookup to first probe, and the allocation above totals 150 to 200. It is measured with stub storage before engine-v4 lands.

For analytical queries, G1 is met by compiling only the first pipeline before the first morsel and overlapping the rest. The statement cache takes repeated queries off the compile path entirely, and every report says whether it was warm.
