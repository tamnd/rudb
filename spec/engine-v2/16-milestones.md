# Milestones

Twelve, F0 through F11. The naming is deliberate: v1's sub-milestones are 2a through 2m and these are a different sequence of a different plan, so that a ledger row can never be ambiguous about which design produced it.

## The rule, again

> A milestone is not done when it is measured. It is done when the mechanism it introduced can be switched off, the engine still answers every query correctly with it off, the harness has a number for both, and the difference is in the ledger with the milestone's name on it.

Every gate below therefore has two numbers: the on number and the off number.

## F0: The walking skeleton

**Ships:** SQL to plan to `EXPLAIN` to execution to metrics to the board. Parquet scan through the async I/O interface. The three operator traits final. A serial scheduler behind them. The strategy registry with reference implementations only. The metrics document, complete. `Rudb` as a real engine in `rudb-bench`.

**Does not ship:** parallelism, spilling, encoded execution, the native format, optimizer passes on by default, windows, recursive CTEs.

**Gate:** all 43 ClickBench queries and all 22 TPC-H queries return DuckDB's answer on `server3`. `EXPLAIN ANALYZE` renders. The metrics cross-check from [`14-metrics.md`](14-metrics.md) section 3 passes. The board has a rudb column and it is last by a wide margin, and it is published anyway, because it is the baseline every subsequent ratio is against.

**No performance gate.** F0 is the only milestone without one.

**Estimate:** six to eight weeks. Most of the operator code exists in `rudb-exec` and is being rehoused rather than written.

## F1: The data plane

**Ships:** `Column` and `Form` from [`05-data-model.md`](05-data-model.md). Macro-generated kernel specialisations for the form combinations that matter, with `UnifiedAccess` instrumented as the fallback. Selection threading. Expression programs and fusion from [`10-expressions.md`](10-expressions.md). Chunk compaction as a seam. The `flatten` counter and the row-loop lint.

**Depends on:** F0.

**Gate:** CPU seconds down 5x against F0 on ClickBench Q1 to Q5 and TPC-H Q1 and Q6. TPC-H Q6 down 3x, ClickBench Q20 to Q27 down 2x. Off numbers from `expr.eval=tree-walk` and `kernel.compare=decoded-loop`. The `UnifiedAccess` fallback counter for the whole suite recorded, because it is the F7 work queue's first draft.

## F2: Storage

**Ships:** the native format from [`06-storage.md`](06-storage.md), file, block, page, tile; lazy column metadata; block statistics and KMV sketches; the sort key; column-scoped global dictionaries as an opt-in. The write path, parallel, with a sampled encoder chooser.

**Depends on:** F1.

**Gate:** `hits` loads in under 252 seconds, which is 2x DuckDB's 126, at 10.23 GB or better, which is 0.50 of DuckDB's 20.46, M1 already reached 9.65 GB so the size is in hand and the time is 80x away. ClickBench runs on the native format and is faster than on Parquet. The sorted-versus-unsorted ablation is recorded. Off number: `storage.encoder=plain`.

**Risk:** the load time. This is the milestone most likely to slip and the one whose failure is most visible, because load time is a published column on the board.

## F3: Memory and larger than memory

**Ships:** the buffer manager from [`07-memory.md`](07-memory.md), operator-owned eviction, `MemoryOwner` with `shrink` and `curve`, spilling for aggregate, sort and join, the no-allocation-outside-`Budget` lint, and the accounting hierarchy.

**Depends on:** F2 for the page layout.

**Gate:** TPC-H SF100 and ClickBench both complete on `server1`, four cores, five gigabytes, under a one-gigabyte `memory_limit`, with correct answers. External peak RSS within fifteen per cent of the limit. The degradation curve at 16, 8, 4, 2, 1 and 0.5 GB recorded for rudb, DuckDB and Polars on the same machine. Off number: `spill.policy=fail`, which records where the cliff was.

## F4: Parallelism

**Ships:** the morsel scheduler, work stealing on merge phases, `Exchange` as a plan node, the blocked-reason wait-for graph and its cycle check, the per-query parallelism floor, the NUMA hooks as no-ops.

**Depends on:** F3, because a parallel engine without memory bounds is a faster way to run out of memory.

**Gate:** 6.5x on eight cores on `server3` for scan-and-aggregate queries. **Total CPU seconds at eight threads within twenty per cent of total CPU seconds at one thread**, this is the number that cannot be bought with hardware and it is the real gate. Per-query floor on the smoke suite not regressed. Off number: `scheduler=single-thread`.

## F5: Hash, aggregate and top-k

**Ships:** unified key encoding with the four key cases; the hash table seam with `open-addressing-salt`, `unchained`, `linear-chained` and `global-concurrent`; `AggregateFunction` with fixed state size; the three grouping shapes including array-grouped on dictionary codes; radix-partitioned parallel aggregation; bounded-heap and heavy-hitter top-k.

**Depends on:** F4.

**Gate:** CPU seconds down 10x against F3 on the GROUP BY queries. Probe and build throughput per core at or above DuckDB's. Beat DuckDB on TPC-H Q1 single-threaded. The full `hash.table` sweep published, which is the first real test of whether [`04-modularity.md`](04-modularity.md) pays for itself. Off numbers: `hash.table=std-hashmap`, `topk=sort-then-limit`.

## F6: Join

**Ships:** hash build and probe with the resumable state machine; match flags for right and full; semi and anti skipping payload; Grace partitioning with a nested-loop bailout; Bloom filters and derived range predicates pushed into the probe-side scan.

**Depends on:** F5.

**Gate:** within 2x of DuckDB on TPC-H SF100 single-threaded. This is explicitly not a win, because there is no join ordering yet and the plans are as written. Probe throughput per core above DuckDB's. Off number: `join.filter=none`, which isolates what the Bloom pushdown was worth and is expected to be the larger half.

## F7: Encoded execution

**The milestone the project exists for.** [`13-encoded-execution.md`](13-encoded-execution.md).

**Ships:** form negotiation end to end; predicates on dictionary, bit-packed, RLE and FSST data; lazy materialisation with the random-access cost term; global dictionary group keys reaching array-grouped aggregation; scalar functions applied to dictionaries rather than rows; the pattern analyser for regex-shaped string work.

**Depends on:** F2, F5 and F6.

**Gate:** ClickBench hot total on the rented reporting machine below **2.63 seconds**, which is 10x DuckDB and 3.1x past Umbra. If it does not land, the seven queries of section 2 of that document reported individually with the DuckDB-to-Umbra ratio beside each, and a statement of which of the three mechanisms failed. The four-way ablation is mandatory either way: `storage.dictionary=per-block`, `topk=bounded-heap`, `scan.materialisation=eager`, pattern analyser off.

## F8: The optimizer

**Ships:** subquery unnesting, join enumeration by DPccp and DPhyp with a planning budget, partial aggregate pushdown below joins, CSE, full predicate transfer, and cardinality estimation from the KMV sketches with the correlation term.

**Depends on:** F7, deliberately. A better plan for a slow engine is worth less than a fast engine on a mediocre plan, and the optimizer's value is easiest to measure once the execution numbers have stopped moving.

**Gate:** total CPU seconds across TPC-H SF100 below DuckDB's. Optimizer-on and optimizer-off answers identical across the whole corpus and per pass. q-error distributions on JOB and CEB recorded. And the experiment from [`12-optimizer.md`](12-optimizer.md) section 4, join ordering against predicate transfer, each alone and both together, run and its answer written into that document.

## F9: Sort, window and the long tail

**Ships:** normalised order-preserving keys, radix sort, external merge; window functions, which do not exist in `rudb-plan` at all today; IEJoin, merge join, AsOf; recursive CTEs including `USING KEY` with DuckDB v2.0's visibility semantics.

**Depends on:** F3 for external sort, F8 for nothing in particular, it could run in parallel with F8 and probably should.

**Gate:** beat DuckDB on a hundred-million-row single-key BIGINT sort single-threaded. Windows within 2x. Every remaining query in `rudb-compat` that currently fails for want of a feature now passes. Off number: `sort=comparator`.

## F10: Adaptivity

**Ships:** `Policy::Adaptive`, a contextual bandit over the applicable set at each seam, following Piece of CAKE. Plus the replay guarantee: an adaptive run records its choices and re-running with them pinned reproduces it.

**Depends on:** F5 through F9, because a bandit needs alternatives worth choosing between, and every milestone before this one has been quietly stocking the registry.

**Gate:** better than `Policy::Default` on total CPU seconds across ClickBench and TPC-H, with exploration cost reported separately. Answers unchanged, convergence demonstrated, replay exact. Off number is `Policy::Default`, which is what ships if this gate fails, and it failing is an acceptable outcome, because the registry was built for [`04-modularity.md`](04-modularity.md)'s reasons and this milestone is harvesting it.

## F11: Distributed

**Ships:** a transport behind `exchange.transport`, a catalog that knows where blocks live, a coordinator, failure handling, and a cost model that counts transfer rounds as well as bytes.

**Depends on:** everything, and on a trigger.

**The trigger, stated harshly on purpose:** no work begins on F11 until the single-node ClickBench number is published and is better than DuckDB's. The project's claim is a single-node claim. A distributed engine that is slower than DuckDB on one machine is not interesting, and the characteristic failure of projects like this one is finding distribution before finding speed.

**Gate:** to be written when the trigger fires. Writing it now would be fiction.

## Dependencies

```
F0 ──► F1 ──► F2 ──► F3 ──► F4 ──► F5 ──► F6 ──► F7 ──► F8
                                    │                    │
                                    └──────► F9 ◄────────┘
                                              │
                              F5..F9 ─────► F10
                                              │
                              (trigger) ─► F11
```

F9 is the only milestone that can run in parallel with another, and it should, because it is the largest block of missing SQL surface and it is not on the path to F7.

## Where the ten times is expected to come from

Honest accounting, so that the plan can be checked against reality as it goes.

F1 is a large multiple against F0 and almost none of it counts, because F0 is a reference engine and the comparison is against ourselves.

F2 through F6 are what get rudb from "an engine" to "a competitive engine", roughly DuckDB's neighbourhood, possibly a little ahead on some shapes. That is a factor of one, against the claim.

F7 is the claim. Three mechanisms, 9x to 11x if all three land, 4x to 6x if one fails, per [`../02-the-goal.md`](../02-the-goal.md).

F8 and F10 are the twenty-six per cent that [`02-research-2026.md`](02-research-2026.md) section 1.1 prices the whole execution-engineering story at. Real, worth having, and not the order of magnitude.

If F7 lands at 4x and F8 and F10 together deliver their twenty-six per cent, the project ends at about 5x. That is a good engine and a failed claim, and [`../02-the-goal.md`](../02-the-goal.md) section 2.7 already says what to do about it, which is to say so.
