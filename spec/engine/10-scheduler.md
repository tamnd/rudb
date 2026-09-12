# Layer eight: the scheduler, memory and spilling

This is sub-milestone 2j. It is the layer document 00 argued should not come first, and the argument was that the scheduler's contract has to be imposed from layer one while its implementation can wait, because a parallel implementation of a slow operator is a slow operator that is harder to profile.

By the time this layer starts, every operator obeys the contract, every layer below has been measured single threaded, and the per-core numbers are known. That is the state in which turning on the cores tells you something. This document is what turning them on means.

## 10.1 What exists today

`crates/rudb-exec/src/operator.rs` is 32 lines and defines the whole execution model: a `schema()` and a `next() -> Result<Option<Chunk>>`. It is a pull model. The driver calls `next` on the root, the root calls `next` on its child, and chunks come back up the tree.

The module doc argues for exactly two methods on the grounds that an operator needing a third would be an operator the scheduler has to know the shape of, and that the morsel-driven scheduler is supposed to know only that a pipeline has a source, some streaming operators and a sink. That instinct is right and the conclusion is going to have to change, for the reason in section 10.2.

`stream.rs` has the three operators that never block: filter, projection and limit. Its doc already names the property that matters, which is that none of them allocates anything proportional to the input and none of them can block, so a morsel is a run of a scan pushed through all of them by one thread.

Everything runs on one thread. There is no thread pool, no memory accounting, and nothing spills.

## 10.2 Pull becomes push, inside a pipeline

The pull model is the right interface at the top of a query, where a client asks for the next batch of results and gets it. It is the wrong model inside a pipeline once there are threads.

Under pull, a thread's position in the pipeline is a call stack, and eight threads running the same pipeline means eight independent call stacks each pulling from a shared source, which works until an operator has state that spans threads. Under push, a thread takes a morsel from the source and pushes it upward through the streaming operators into the sink, and the thread's position is a loop rather than a stack. That makes the parallel structure explicit: what is parallel is the pushing, what is shared is the sink's global state, and what is per-thread is the sink's local state.

Push also fixes something pull gets wrong that is not about threads at all. Under pull, an operator that wants to emit more rows than fit in one chunk has to be resumable in the middle, which document 08 section 8.2 called the single most common source of hash join bugs. Under push, it emits as many chunks as it likes into the next operator and there is no suspended state to get wrong.

So the interface becomes three shapes rather than one.

```rust
trait Source  { fn morsel(&self) -> Option<Morsel>; fn read(&self, m: Morsel, out: &mut Chunk) -> Result<Progress>; }
trait Stream  { fn push(&self, chunk: &mut Chunk, state: &mut LocalState) -> Result<Progress>; }
trait Sink    { fn sink(&self, chunk: &Chunk, local: &mut LocalState) -> Result<Progress>;
                fn combine(&self, local: LocalState) -> Result<()>;
                fn finalize(&self) -> Result<()>; }
```

The pull interface stays at the query root, implemented over the last sink, because that is what the C API and the CLI want and because a result set is genuinely pulled.

This is a change to every operator, and it is why it is worth having said in document 00 that the contract is imposed early. The operators written in layers five through seven already have their global and local state separated and already have `combine` and `finalize`, so what changes for them is the shape of the call and not the design. If the contract had not been imposed early, this layer would be a rewrite of six operators rather than a mechanical change to their signatures.

## 10.3 Pipelines

A plan becomes a set of pipelines. A pipeline starts at a source, passes through zero or more streaming operators, and ends at a sink. A pipeline breaker, meaning an aggregate, a sort, a join build or a window, ends one pipeline and begins another, and the dependency between them is that the second cannot start until the first has finished.

A hash join produces two pipelines with a dependency: the build side's pipeline ends at the join's build sink, and the probe side's pipeline has the join as a streaming operator and cannot start until the build has finalized. That dependency graph is the schedulable unit, and running two independent pipelines at once when there are cores to spare is a real win on the multi-join TPC-H queries, where several dimension table builds are independent of each other.

The decomposition is computed once when the plan is turned into an executable, it is shown by `EXPLAIN ANALYZE`, and the per-pipeline timings are what makes a slow query diagnosable. That output is worth as much as the parallelism itself, because without it every performance investigation from this layer onward is guesswork.

## 10.4 Morsels

The design is Leis et al. from SIGMOD 2014 and it has not been improved on in a way that matters.

A morsel is a unit of source data, which document 05 section 5.9 already fixed at a row group or a block, of the order of a hundred thousand rows. A task is one thread executing one pipeline over morsels until the source runs out. A dispatcher hands out morsels on demand, so a thread that gets an easy one comes back sooner, which is what makes the scheme robust against skew without any estimation.

The thread pool is one worker per hardware thread by default, created once per database instance rather than per query, because creating threads per query is a latency floor and the parent spec's second axis is explicitly about the per-query floor. The `threads` setting controls it, which `rudb-common/src/settings.rs` already classifies and canonicalizes from `worker_threads`.

Work stealing is not needed for morsels, because the dispatcher already balances them. It is needed for the merge phases, meaning the partitioned merges from document 07 section 7.5 and document 09 section 9.4, where the units are of uneven size and are known up front. A simple deque-based stealer covers it.

NUMA is not addressed at this layer and the reason is that the fleet has no NUMA machine. `server3` is eight cores on one socket. Writing NUMA-aware allocation and scheduling against no machine that can show it working is writing untested code, so the design keeps the hook, which is that a morsel carries the memory node its data is on and the dispatcher prefers local morsels, and the implementation is a no-op until there is a machine to measure it on. That is recorded as a known gap rather than an oversight, and it is one of the places where renting a machine, as document 02 section 2.7 proposes, would buy something beyond a publishable number.

## 10.5 Backpressure

A pipeline can stall for reasons other than running out of morsels, and what happens then decides whether the engine degrades gracefully or thrashes.

Document 01 recorded Polars' approach, which is exact backpressure through wait tokens: a sink that cannot accept more data hands back a token, and the producer waits on it rather than buffering. DuckDB's approach is a blocked-task mechanism where a task that cannot proceed is descheduled and requeued when the condition clears, so the thread goes and does something else instead of sleeping.

rudb takes DuckDB's shape, because it composes better with a work-stealing pool and because a thread that blocks is a core that is not working. `Progress` in the interfaces in section 10.2 is the mechanism: an operator returns `Progress::Blocked(reason)` and the task is parked on that reason and requeued when it clears. The reasons are a small closed set: waiting on I/O, waiting for memory, waiting for a dependency pipeline, and waiting for a downstream buffer.

That closed set matters. An open-ended blocking mechanism is a deadlock waiting to happen, and with four reasons the possible cycles can be enumerated and asserted against. The assertion is a test that constructs each potential cycle deliberately and checks that the scheduler either breaks it or reports it, rather than hanging.

## 10.6 Two pools

Document 05 section 5.3 already established a separate I/O thread pool with a submission interface, on the grounds that DuckDB v2.0 made this a correctness-of-measurement issue and that a compute thread waiting on a read is a compute thread not computing.

This layer is where the two pools meet. A scan that submits a batch of reads returns `Progress::Blocked(Io)`, the compute thread takes another task, and the I/O completion requeues the scan. That is the whole integration and it is only clean because the blocking mechanism from section 10.5 exists.

The sizing of the two pools is different and both are measured. Compute is one per hardware thread. I/O is however many concurrent requests the device wants outstanding, which is a small number for a single NVMe and a large number for object storage, and which is measured on `server1` and `server3` separately because their disks are not alike.

## 10.7 Memory

This is the half of the layer that has nothing to do with threads and is at least as important.

Today nothing accounts for memory and nothing bounds it. `rudb-common/src/settings.rs` classifies `memory_limit`, canonicalized to `max_memory`, as remembered rather than honoured, with the argument written out that it only decides whether a query errors and that the memory manager is later work. This is the later work, and this is where that setting becomes honoured.

The pieces, in order.

**A buffer manager.** Blocks read from disk are held in a pool of fixed-size buffers with pinning and eviction, rather than being allocated per read and dropped. Document 03 section 3.8 already decided that a vector holds a refcounted pin handle rather than a Rust lifetime, precisely so that this can be added without changing every signature, and this is where that decision is cashed in. Eviction is clock or a simple LRU approximation, because the difference between eviction policies is small compared to the difference between having one and not.

**An accounting hierarchy.** Every operator that holds memory proportional to its input reports how much, which documents 06 section 6.9, 07 section 7.10 and 08 section 8.8 all already require. The database has a total budget, a query has a share, and an operator asks for a reservation before it grows. An operator that is refused a reservation returns `Progress::Blocked(Memory)` and either waits for another operator to release or is told to spill.

**Spilling.** Three operators have seams already cut for it and no implementation. The hash aggregate spills partitions of its table, which it can do because the table partitions by hash bits without rehashing and because aggregate states serialize. The hash join spills partitions of both sides, which is a grace hash join and which needs the probe pipeline to be redirectable into partition files, which document 08 section 8.8 specified. The sort spills sorted runs and merges them from disk, which is the classic external merge sort and which the normalized key layout in document 09 section 9.3 makes straightforward because a run is a flat array of fixed-width entries.

The order to build them in is aggregate, sort, join, by increasing difficulty and by decreasing likelihood that a user hits the limit there first.

**The failure mode when spilling is not possible.** Some things cannot spill, and the answer is a clear error naming the operator and the amount, not an out-of-memory kill. A database that dies is worse than a database that says no.

`max_execution_time` becomes honourable at the same time and for the same reason, since a cancellable scheduler is what a timeout needs.

## 10.8 `WITH RECURSIVE`

A recursive CTE is a loop in the plan: evaluate the anchor, then repeatedly evaluate the recursive term against the previous iteration's output until it produces nothing.

That is a scheduling construct rather than an operator, because each iteration is a full pipeline execution whose source is the previous iteration's result, and that is why document 00 put it here rather than treating it as SQL surface. The implementation is a driver that runs the sub-plan repeatedly with a working table swapped between iterations, plus the union semantics that decide whether duplicates are eliminated between iterations.

Document 01 recorded that DuckDB v2.0 rewrote recursive CTEs and that `USING KEY` changed the visibility semantics of what an iteration can see. Matching that is a compatibility obligation and it needs the corpus tests for it, because the semantics are subtle enough that reading the documentation is not sufficient.

Iteration limits and cycle detection matter here for a practical reason: an unterminated recursive CTE is an infinite loop that allocates, and without the memory accounting from section 10.7 it takes the machine down. With it, it hits the limit and errors, which is the right behaviour and is another reason these two are in the same layer.

## 10.9 Cancellation, progress and observability

A query has to be interruptible, which means every operator checks a cancellation flag at chunk granularity, which is cheap because it is once per thousand rows rather than once per row.

Chunk granularity is the rule and it is not the whole rule, because an operator that loops over rows it has already read produces no chunks while it is doing so. A nested loop join is the case: the join runs to the end before its first chunk exists, so a check between chunks is a check at the end. An operator like that checks inside its own loop, at whatever unit of work it has that is smaller than the whole operator and larger than a row.

Progress reporting, meaning the fraction of morsels consumed across the source pipelines, is nearly free once the dispatcher exists and it is what a CLI progress bar and a client's cancel button need.

`EXPLAIN ANALYZE` reports per pipeline and per operator: rows in, rows out, wall time, CPU time, peak memory, and blocked time by reason. The blocked time by reason is the part that is unusual and it is the most useful number in the whole system for diagnosing a slow query, because it distinguishes an operator that is slow from an operator that is waiting.

## 10.10 The test gate

Determinism is the property that makes everything else testable, and the honest version of it is: for a fixed thread count and a fixed input, a query gives the same answer every time, and across thread counts it gives the same answer up to row order and up to floating point associativity. Both halves are stated in documents 07 and 09 and this is where they are enforced, by running every corpus query at one, two, four and eight threads and comparing.

That test is expensive and it is exactly the test that finds the bugs this layer introduces, so it runs on a schedule rather than on every commit, and the single-threaded comparison runs on every commit.

Stress tests with deliberate contention: many threads, small morsels, tiny memory limits so that spilling triggers constantly, and injected I/O delays through the simulator so that blocking paths are exercised. The simulator from `rudb-io` is what makes the I/O half of this deterministic, and the memory half is made deterministic by making the memory limit a test parameter so that a spill can be forced at an exact point.

Spilling correctness: every spilling operator produces the same answer with a memory limit that forces spilling as it does with an unlimited one, over the full corpus, at several limits including absurdly small ones. Absurdly small is the interesting case because it exercises recursive spilling, where a spilled partition still does not fit.

Deadlock: the four blocking reasons from section 10.5 are enumerated and each potential cycle is constructed deliberately.

`gamingpc` runs the thread-count comparison, because thread scheduling and memory behaviour differ enough on Windows to be worth a separate signal.

## 10.11 The benchmark gate

This is the layer where wall clock finally becomes an interesting number, and where the discipline from document 02 section 2.5 has to hold hardest: CPU seconds is the axis that decides whether the project is real, and a wall clock win bought with eight times the CPU is a scheduling result and not an engine result.

So the primary measurement is a scaling curve. ClickBench and TPC-H at one, two, four and eight threads on `server3`, reporting wall clock, total CPU seconds and the efficiency ratio. What has to be true is that CPU seconds stay roughly flat as threads increase, which means the parallelism is not adding work. A curve where CPU seconds rise sharply with thread count is a curve showing contention, and the shape of the rise says where.

The out-of-core measurement is the second one and it is new. TPC-H SF100 on `server1`, which has five gigabytes of RAM and a dataset far larger, with the memory limit set to two gigabytes, one gigabyte and five hundred megabytes. Every query must complete and the degradation must be gradual. This is the measurement Polars is built for and DuckDB has invested heavily in, it is a genuine differentiator in real use, and `server1` is the machine that makes it measurable without artificial constraint.

The per-query floor gets measured here too, because a thread pool and a dispatcher add fixed cost per query. A trivial query over a one row table, timed end to end, is the floor, and it must not regress. The parent spec makes this one of its four axes and this is the layer most likely to damage it.

The target at 2j: near-linear scaling to eight threads on the scan-and-aggregate queries with CPU seconds flat within twenty percent, all twenty-two TPC-H queries completing at SF100 under a one gigabyte limit on `server1`, and the per-query floor no worse than before the pool existed.

## 10.12 Exit criterion for 2j

**Operators are sources, streams and sinks pushed by morsel-driven tasks over a shared thread pool, pipelines are scheduled with their dependencies and shown by `EXPLAIN ANALYZE` with blocked time by reason, blocking is a closed set of four reasons with no cycles, a buffer manager with pinning and eviction backs every block read, `memory_limit` and `max_execution_time` are honoured rather than remembered, the aggregate and the sort and the join all spill and produce identical answers under forced spilling across the whole corpus, `WITH RECURSIVE` runs including `USING KEY`, queries are cancellable, thread-count answer comparison passes at one through eight threads, CPU seconds stay flat within twenty percent across that range, and TPC-H SF100 completes on `server1` under a one gigabyte memory limit.**

Named as deferred: NUMA awareness, which has a hook and no implementation because there is no machine to measure it on, and distributed anything, which is not what this database is.
