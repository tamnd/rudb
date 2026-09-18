# Parallel now, distributed later

Multi-core is the third of the four things the user asked to be designed properly, and distribution is the fourth with "later" attached. This document does both, because the only way distribution is cheap later is if multi-core is shaped correctly now.

## 1. Morsel-driven, and the numbers everyone converged on

Leis, Boncz, Kemper and Neumann, SIGMOD 2014. Work is handed out in morsels from a shared queue; a thread that finishes takes another; the degree of parallelism is a property of the queue rather than of the plan.

The mechanism matters less than the property it produces: **no operator knows how many threads there are.** Everything in [`08-execution.md`](08-execution.md) follows from that, and so does the fact that F0's serial driver and F4's parallel one run identical operators.

Morsel size converged independently: Polars at about 128k rows, DuckDB at 122,880, which is 60 × 2048 and is its row group. This design uses 122,880 for the same reason [`06-storage.md`](06-storage.md) uses it as the block size, a morsel that is a block is a morsel whose dictionary, bit-packing base and RLE run space are self-contained, which is what makes an encoded kernel possible without cross-morsel state. It is a seam (`morsel.size`) and is swept.

## 2. The scheduler

One thread pool per database instance, not per query. Sized to hardware threads. A second pool for I/O, sized to the device.

Per-query parallelism is a number the planner sets per pipeline, bounded by the pool and by a floor: a query estimated to touch ten thousand rows gets one thread, because spinning up thirty-two pipeline instances to read ten thousand rows is how an engine ends up with a bad per-query floor. The per-query floor is one of the four axes in [`../02-the-goal.md`](../02-the-goal.md), the median DuckDB-to-Umbra ratio is 4.65 but the minimum is 1.25, and it is lost by scheduling overhead more often than by anything else.

Tasks are pipeline instances. A task runs until its source is exhausted or it returns `Blocked`. A blocked task parks on its token and is woken by whatever satisfies it.

Work stealing is used for the merge and finalise phases, where the work is unequal and known, and not for the scan phase, where the morsel queue already balances. This is v1's position and it is right: stealing during the scan buys nothing and costs cache locality.

## 3. Deadlock, enumerated

With four blocked reasons, the wait-for graph has four edge kinds. The scheduler maintains it and checks two things.

At plan time, pipeline dependency edges form a DAG. A cycle here is a planner bug and is caught before execution.

At runtime, whenever every task is blocked and no I/O is outstanding, the graph is walked. A cycle is reported with the cycle in the error. This costs nothing because it only runs when nothing else can.

The reason this is worth the ceremony: the strategy registry means the engine will run in configurations nobody tested by hand, and a deadlock that only appears under `agg.parallel=global-concurrent` with `spill.policy=operator-chosen` at a one-gigabyte limit is a deadlock nobody will find by reading.

## 4. NUMA

A hook and nothing more, because the fleet has no NUMA machine, `server3` is eight cores, `server1` is four, `laptop` is ten, and the reporting machine is a `c6a.4xlarge`, which is one socket.

The hook: a morsel carries a locality hint, the pool has an affinity per worker, and the queue prefers local morsels. Both are no-ops today. The reason to have them at all is that they are the only thing that is expensive to add later, because they change the morsel queue's shape.

## 5. Exchange as a plan node

The decision that makes distribution a swap rather than a rewrite.

Every point where data is redistributed between pipeline instances is an explicit plan node, today, with threads on both sides.

```rust pub enum Distribution {
    /// Any instance may see any row. The default for a scan.
    Any,
    /// Partitioned on these expressions, this many ways.
    Hash { on: Vec<ExprId>, ways: u32 },
    /// Every instance sees every row.
    Broadcast,
    /// Exactly one instance. A sort's merge, a global aggregate's finalise.
    Single,
    /// Partitioned by range, for sorted output.
    Range { on: Vec<ExprId>, bounds: Vec<Scalar> },
}

pub struct Exchange {
    from: Distribution,
    to: Distribution,
    transport: StrategyRef, // seam: exchange.transport
}
```

Today `exchange.transport` has one non-reference implementation, `in-process-channel`, and an `Exchange` between two in-process distributions is frequently a no-op the planner elides. That is fine. What matters is that the plan says where the boundary is.

Three things follow immediately, all of them useful before any distribution exists.

The planner has to reason about distribution to elide an exchange, which means it knows which operators require which distribution, which is exactly the analysis a distributed planner needs.

`EXPLAIN` shows where data is redistributed, which is where parallel queries spend time and is currently invisible.

A hash aggregate's radix partitioning and a join's Grace partitioning stop being private operator details and become the same mechanism as an exchange, which removes one of the two implementations.

## 6. The distributed seam

What F11 adds, and what it does not have to add because it was done earlier.

**Does not have to add: serialisable state.** [`07-memory.md`](07-memory.md) section 6 already requires every spillable structure to be pages with offsets rather than pointers. A build-side partition that can be written to a spill file can be written to a socket. This is the coincidence principle 7 is built on and it is the single largest piece of the work.

**Does not have to add: operators that do not assume global visibility.** Every sink already has `combine`, already merges partial state, and already cannot see the whole input. A distributed aggregate is the same aggregate with `combine` running across a network instead of across threads.

**Does not have to add: a plan representation of redistribution.** Section 5.

**Has to add:** a transport, a catalog that knows where blocks live, a coordinator that assigns pipeline instances to nodes, failure handling, and a cost model in which network bytes are expensive. Four of those five are genuinely new work and the fifth, the cost model, is the REMOP adjustment noted in [`07-memory.md`](07-memory.md) section 8: count transfer rounds as well as bytes, because a fixed per-transfer latency changes which partitioning is optimal.

The trigger for F11 is stated in [`16-milestones.md`](16-milestones.md) and is deliberately harsh: no distributed work begins until the single-node ClickBench number is published and is better than DuckDB's. The project's claim is a single-node claim. A distributed engine that is slower than DuckDB on one machine is not interesting, and the failure mode of every project like this one is discovering distribution before discovering speed.

## 7. Determinism

Float summation under `agg.parallel=radix-partitioned` is deterministic for a fixed thread count and a fixed data order, and not across thread counts. This matches DuckDB and it is the right trade.

What makes it safe is that it is declared: `Strategy::deterministic()` returns `Determinism::PerThreadCount`, the differential oracle compares within tolerance for those strategies and exactly for everything else, and `EXPLAIN` says so. An engine that is nondeterministic without saying where is an engine whose test failures are unreproducible.

A `deterministic` session setting forces `Determinism::Exact` strategies everywhere, at a cost in speed, for the case where a user needs bit-identical results across runs. It is also what the fuzzer runs under.

## 8. Gate

F4 is done when:

Scaling on ClickBench is near-linear to eight threads on `server3`, the specific bar is 6.5x on eight cores for the scan-and-aggregate queries, which is what DuckDB gets on that shape.

Total CPU seconds at eight threads is within twenty per cent of total CPU seconds at one thread. This is the number that cannot be bought with hardware and it is the one that matters. An engine that goes 6.5x faster on eight cores while burning 2x the CPU has bought its speedup, and the ledger says so.

The per-query floor is not regressed: the smoke suite's six queries, which run in tens of milliseconds, are no slower than at F3.

The switched-off number is the serial driver, which stays in the tree as `scheduler=single-thread` and is the reference.
