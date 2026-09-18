# Execution

The three traits, and why they are final at F0.

## 1. What is there now

`rudb-exec/src/operator.rs` is thirty-two lines:

```rust pub trait Operator: fmt::Debug {
    fn schema(&self) -> &Schema;
    fn next(&mut self) -> Result<Option<Chunk>>;
}
```

Pull. Every operator drives its children. `&mut self` means one instance, one thread. Its doc comment already anticipates the problem: it says the scheduler is supposed to know only that a pipeline has a source, some streaming operators and a sink, which is a description of the push interface, written above a pull interface.

v1 keeps this through layer 7 and converts at layer 8. This design does not, for the reason in principle 6: the conversion touches every operator, every test, and every internal loop that assumed it could block, and the single-threaded implementation behind the push interface is fourteen lines.

## 2. The three traits

```rust
/// Produces chunks. One instance, many concurrent readers. pub trait Source: Send + Sync {
    /// A unit of work. `None` means the source is exhausted.
    /// Called concurrently; must be internally synchronised and cheap.
    fn morsel(&self) -> Option<Morsel>;

    /// Fill `out` from `morsel`. May be called many times for one morsel,
    /// returning `More` until the morsel is drained.
    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress>;
}

/// Transforms chunks in place. No state that outlives a pipeline instance. pub trait Stream: Send + Sync {
    type Local: Send;
    fn local(&self) -> Self::Local;

    /// Transform `chunk` in place. May shrink it via its selection, may
    /// replace columns, may not grow it beyond the chunk width.
    fn push(&self, chunk: &mut Chunk, local: &mut Self::Local) -> Result<Progress>;
}

/// Consumes chunks into state. This is where pipelines end. pub trait Sink: Send + Sync {
    type Local: Send;
    fn local(&self) -> Self::Local;

    fn sink(&self, chunk: &Chunk, local: &mut Self::Local) -> Result<Progress>;

    /// Merge one thread's local state into the global state.
    fn combine(&self, local: Self::Local) -> Result<()>;

    /// After every `combine`. Produces whatever the next pipeline sources from.
    fn finalize(&self) -> Result<()>;
}
```

Five observations, each of which is a decision.

**`&self`, not `&mut self`.** An operator is shared across threads; per-thread mutable state is `Local`. This is what makes a pipeline instantiable N times without N copies of the operator's configuration, and it is what forces the state that matters to be explicit.

**`Stream` transforms in place.** A filter writes a selection. A projection replaces columns. Neither allocates a new chunk. This is what makes a chain of streaming operators run in one cache-resident buffer, and it is the difference between a push engine and a pull engine wearing a push costume.

**`read` may be called repeatedly for one morsel.** A morsel is a block, and a block of `URL` does not fit in a chunk. Without this, morsel size and chunk size are forced to be equal, and [`06-storage.md`](06-storage.md) needs them to differ.

**`combine` takes `Local` by value.** The local state is consumed. This prevents the bug where a thread's state is merged twice, which is the kind of bug that produces a wrong `SUM` once in every few hundred runs.

**`finalize` is separate from the next pipeline's source.** A hash aggregate's sink finalises into a structure; a separate `Source` reads that structure out in parallel. Fusing them would make the parallel read of an aggregate result impossible.

## 3. Progress, and the closed set of blocked reasons

```rust pub enum Progress {
    /// Did work, call again.
    More,
    /// Did work, this unit of input is consumed.
    Done,
    /// Could not proceed. The scheduler parks this task.
    Blocked(Blocked),
}

pub enum Blocked {
    Io(IoToken),
    Memory(MemoryToken),
    Dependency(PipelineId),
    Downstream(BufferId),
}
```

Four reasons and the set is closed. This is DuckDB's shape and v1 chose it over Polars' wait tokens for a stated reason that this design agrees with: with a closed set, the wait-for graph is a finite graph over four edge kinds and deadlock is enumerable. The scheduler asserts acyclicity at plan time and again whenever every task is blocked, and a cycle is a bug report with the cycle in it rather than a hang.

Polars' design is more elegant and gives exact backpressure. The cost is that a deadlock is a property of a graph of async state machines, and finding one means reading all of them. For an engine whose test strategy is differential execution of a large corpus under many strategy combinations, enumerable beats elegant.

## 4. Pipelines

A **pipeline** is one source, zero or more streams, one sink. A **pipeline breaker** is an operator that cannot emit until it has consumed everything: hash join build, hash aggregate, sort, top-N, window, distinct, set operations.

A query is a DAG of pipelines. Edges are dependencies: the join's probe pipeline cannot start until the build pipeline's `finalize` has returned. `EXPLAIN` prints the decomposition, numbered, with the dependency edges, and `EXPLAIN ANALYZE` prints per-pipeline totals, which is where the answer to "why is this query slow" usually is, and which no current output shows.

Pipelines are instantiated N times, N being the degree of parallelism chosen for that pipeline. Each instance has its own `Local` per operator and its own chunk buffer. They share the operator objects and the sink's global state.

## 5. The root

A query root is pull, because the C API, the CLI and the Arrow interface all pull. One adapter: a `Sink` whose local state is a bounded queue, and a `Source`-shaped reader on the other side that the caller drives. Backpressure through it is `Blocked(Downstream)`, which closes the loop with section 3.

This is the only pull in the engine and it is fifty lines.

## 6. Cancellation and limits

Cancellation is checked at chunk granularity, in the scheduler's loop, not inside operators. An operator whose `push` takes longer than a chunk is an operator with a bug. `rudb-exec/src/cancel.rs` exists already.

`max_execution_time` and `memory_limit` are both enforced at the same point, which is why they are one mechanism rather than two. A query that exceeds either is cancelled with an error naming which, and the metrics document is still emitted, a killed query's metrics are the most useful metrics there are.

## 7. I/O inside execution

A scan does not block on a read. `Source::morsel` returns a morsel whose pages have been requested; `Source::read` returns `Blocked(Io(token))` if they have not arrived. The scheduler parks the task and runs another. Read-ahead depth is governed by the memory limit, as in DuckDB v2.0, because a read-ahead queue is memory and a fixed depth is either wasteful or insufficient depending on the machine.

DuckDB v2.0's measured 1.5x cold on local files on a laptop is the number this is worth on the fleet's hardware; the 3.0x to 3.7x on S3 and 19.4x on CSV are cloud numbers and are not ours to claim.

## 8. The single-threaded driver, in full

Because the claim that the push interface costs nothing single-threaded should be checkable.

```rust fn run_serial(p: &Pipeline) -> Result<()> {
    let mut locals = p.locals();
    let mut chunk = Chunk::with_capacity(p.width());
    while let Some(mut m) = p.source.morsel() {
        loop {
            chunk.clear();
            match p.source.read(&mut m, &mut chunk)? {
                Progress::Blocked(b) => { b.wait()?; continue }
                done => {
                    for (s, l) in p.streams.iter().zip(&mut locals.streams) {
                        s.push(&mut chunk, l)?;
                    }
                    p.sink.sink(&chunk, &mut locals.sink)?;
                    if matches!(done, Progress::Done) { break }
                }
            }
        }
    }
    p.sink.combine(locals.sink)?;
    p.sink.finalize()
}
```

Twenty-two lines. The parallel driver is a hundred and forty and is in [`09-parallel-and-distributed.md`](09-parallel-and-distributed.md). Nothing above the driver changes between them.

## 9. Instrumentation

Every operator is wrapped, at pipeline construction, in a counter shim that records rows in, rows out, wall time, CPU time, bytes decoded, and bytes spilled. The shim is a `Stream` or `Sink` that delegates, which means instrumentation is not a special case in the operator interface and an operator cannot forget to do it.

The shim costs two clock reads per chunk per operator. At 122,880 rows per chunk and ten operators, that is twenty `clock_gettime` calls per 1.2 million row-operations, which is not measurable. It is on by default, including in published benchmark runs, and the ledger records the overhead once by running the suite with `--no-instrument` so the tax is a number rather than a worry.

## 10. What this rejects from v1, and what it keeps

Rejected: the pull interface and the plan to convert. Kept, essentially unchanged: the blocked-reason set and its rationale, the two thread pools, the chunk-granularity cancellation, the decision that the root stays pull for the C API, the resumable-probe state machine discipline for joins, and the observation that the resumable probe is the single most common source of bugs in this kind of engine.
