# Memory, and larger than memory

Larger-than-memory is the second of the four things the user asked to be designed properly. It is a constraint on the shape of every stateful operator, which is why it is F3 and not F8.

## 1. The argument that was open, and how it closes

The firepanda notes and v1 disagree, politely, about who owns spilling.

v1's position: a buffer manager with pinning and clock or LRU eviction, at layer 8, with operators spilling in aggregate-then-sort-then-join order.

The firepanda position, from the Kuiper paper's framing: a buffer manager can evict the page an operator is about to read, and an operator knows things about its own access pattern that no eviction policy can infer. Therefore operators should control their own spilling.

Both are right about something and the 2026 literature resolves it. Kuiper, Boncz and Mühleisen unify temporary and persistent data under one manager, because an aggregate that can only use the "temporary" half of memory cannot use the memory there is. Umami adds the missing half: buffers are managed independently per operator inside the shared manager, so the operator decides which of its own pages go, in their case, thread-local hash table bucket ranges evicted to partitions, while the manager still owns the total and still enforces the limit. Umami's specific criticism of DuckDB is that it unpins pages the hash table still references, which is the failure mode firepanda predicted. Otaki et al. at CIDR 2025 supply the interface for deciding: a consumer that knows what is pinned can publish a curve relating memory to cost.

The synthesis, which is this design:

> One manager owns the total. Each operator owns a budget and decides what leaves it. The manager never evicts an operator's page; it asks the operator to free bytes and the operator chooses which.

## 2. The manager

```rust pub struct BufferManager { /* pages, budget, waiters */ }

impl BufferManager {
    /// A budget an operator holds for the life of a query.
    /// Fails only if the query's total would exceed the query's limit.
    fn open(&self, owner: OwnerId, min_bytes: u64) -> Result<Budget>;
}

pub struct Budget { /* owner, current, high water */ }

impl Budget {
    /// Allocate a page inside this budget. Returns `Blocked` rather than
    /// failing when the budget is full and the owner has not yet been asked
    /// to shrink.
    fn page(&mut self) -> Result<PageRef, Blocked>;

    fn pin(&self, page: PageId) -> Result<Pinned<'_>>;
    fn unpin(&self, page: PageId);

    /// Hand a page back to the manager, which may write it to the spill
    /// file. Only the owner calls this.
    fn release(&mut self, page: PageId) -> Result<()>;
}
```

The inversion from the usual design is in `release`. The manager never takes; the owner gives. When the manager is over budget it calls, on each owner in turn, in an order determined by `spill.policy`:

```rust pub trait MemoryOwner {
    /// Free at least `bytes`, by releasing pages of your own choosing.
    /// Return how much you actually freed.
    fn shrink(&self, bytes: u64) -> Result<u64>;

    /// What this operator would cost at various budgets. Otaki et al.
    fn curve(&self) -> MemoryCurve;

    /// Bytes that cannot be released at any price, e.g. a partition
    /// currently being probed.
    fn floor(&self) -> u64;
}
```

`shrink` is the whole interface and it is three lines. The aggregate's implementation picks its coldest radix partitions and writes them out. The sort's picks completed runs. The join's picks build-side partitions whose probe has not started. Each of those is a decision the operator can make correctly and a policy cannot.

`MemoryCurve` is a small step function, cost at floor, cost at half, cost at ideal, and the manager uses it to choose *whom* to ask, which is the thing clock and LRU are guessing at. An aggregate whose curve is flat gives up memory first. One whose curve is a cliff is asked last.

## 3. The rule

> No operator allocates query state outside a `Budget`.

This is the lint that makes everything else true. `xtask lint memory` fails on any `Vec`, `HashMap`, `Box` or `String` field on a type that implements `Sink` or holds `LocalState`, unless it is annotated `#[bounded(N)]` with a constant byte bound and N is small.

It is a blunt instrument and it will be annoying. It is also the only mechanism that reliably prevents the failure where an engine honours its memory limit to within a factor of three because a dozen small structures are outside the accounting. DuckDB's `memory_limit` and ClickHouse's `max_memory_usage` are both approximately honoured for exactly this reason, and "approximately honoured" is not good enough when peak RSS is one of the four axes the project claims 10x on.

## 4. Accounting

A hierarchy: process, then database instance, then query, then operator, then the per-thread local states inside an operator.

Every level has a limit and a high-water mark. `memory_limit` sets the instance level and is DuckDB-compatible. The query level defaults to the instance level divided by the number of concurrent queries, with a floor, which is a policy and is switchable.

Reservation is optimistic with a retry: an operator takes what it needs and, on failure, blocks rather than errors. `Blocked(Memory)` is one of the four blocked reasons in [`08-execution.md`](08-execution.md), and the scheduler's deadlock enumeration depends on the set being closed. An operator that has reached its floor and still cannot proceed is the one case that becomes an error, and the error names the operator and its floor.

The metrics document carries, per operator: bytes reserved, high water, bytes spilled, bytes read back, and the number of times `shrink` was called and what it freed. `EXPLAIN ANALYZE` prints spill bytes in red, following Polars' convention, because a spilled query is a query whose number needs an asterisk.

## 5. Spilling, per operator

**Aggregate.** Radix partitions on the top hash bits. Cold partitions are released whole. Reading back is a pin, because the partition's page layout is the hash table's page layout, this is the "no serialisation" property from Kuiper. Combining spilled partitions happens at finalise, one partition at a time, and a partition that still does not fit is re-partitioned on the next bits down. The recursion has a depth bound and a bailout, because a column with one value that hashes everywhere the same is a real dataset and not a hypothetical.

**Sort.** Runs. A run is sorted, written, and released. Merge is a k-way merge with one page pinned per run, which bounds residency at k pages regardless of data size. When k exceeds what fits, merge in passes. This is 1960s technology and it is correct.

**Join.** Grace partitioning on the top hash bits, so a partition never has to be rehashed. Build partitions spill; probe partitions spill to match. Probing a spilled build partition means reading it back whole, which is why `floor()` for a join is the size of one build partition, and why the partition count is chosen so that one partition fits in the floor. Skew, one partition that does not fit even alone, falls back to a nested loop over that partition, which is slow and correct and finishes.

**Window.** Partition at a time, and a partition larger than memory sorts externally and streams the frame. Window is F9 and the design is the sort's.

**Top-N and heavy hitters.** Never spill, because their state is O(k). This is the entire point of [`13-encoded-execution.md`](13-encoded-execution.md) section 4 and it is worth noting here: converting an operator from O(distinct) to O(k) is a better answer to larger-than-memory than spilling it.

## 6. Serialisable state, and the coincidence

Every spillable structure must be representable as pages with no pointers into the process's address space. Offsets, not pointers. Block-relative, not absolute.

This constraint is annoying and it is what makes spilling free of serialisation. It is also, and this is principle 7's whole content, exactly the constraint that makes state sendable to another machine. A hash table partition that can be written to a temporary file can be written to a socket. [`09-parallel-and-distributed.md`](09-parallel-and-distributed.md) section 6 does not add a serialisation layer; it reuses this one.

The lint is `xtask lint pointerfree`, and it checks that types reachable from a spillable page contain no reference or raw pointer fields.

## 7. Eviction policy for persistent pages

The one place an ordinary eviction policy still applies: pages holding table data that no operator has claimed as state. Those are cache, and the manager may evict them without asking.

`buffer.eviction` is the seam, with `clock`, `lru` and `sampled-predictive` in tree. The third is the VLDB 2026 predictive scan policy: use knowledge of long-running scans to approximate the optimal policy, examining a sampled subset of buffers per eviction rather than all of them. It targets exactly the concurrent-scan shape the benchmark suites have, the sampling makes it cheap, and it is a fifty-line strategy once the seam exists. Whether it beats clock on `hits` is a measurement and it lives in the F3 ledger.

## 8. Tiered and remote memory

Not built. Two hooks, because both are cheap now and neither is retrofittable.

A page's home is a `Tier`, and the manager's cost model asks the tier what a fetch costs. Today there are two tiers, memory and the spill file. `vmcache`-style tiered memory and CXL add a third without changing the interface.

The spill cost model counts **transfer rounds** as well as bytes. On local NVMe that term is zero and the model degenerates to bytes, which is why it costs nothing today. REMOP's finding is that with disaggregated or remote memory the fixed per-transfer latency dominates and a policy that minimises bytes triggers too many rounds, so the operator buffer-partitioning strategies that are optimal for disk are wrong. Having the term in the model now means the day it matters is a constant change rather than a redesign of every operator's spill logic.

## 9. Gate

F3 is done when:

TPC-H SF100 completes on `server1`, which is four cores and five gigabytes, under a one-gigabyte `memory_limit`, with every query returning DuckDB's answer.

ClickBench completes on the same machine under the same limit.

Peak RSS stays within fifteen per cent of the limit across the whole run, measured externally by `rudb-bench`'s `memory.rs` and not by the engine's own accounting, because the engine's accounting is what is being tested.

The degradation curve is recorded: the same suite at 16 GB, 8, 4, 2, 1, and 512 MB, so that "larger than memory" is a curve in the ledger rather than a claim. DuckDB and Polars are run on the same curve, because both now do this and both publish about it, Polars' hundred-gigabyte join on a sixteen-gigabyte laptop is the number to be measured against.

And the switched-off number: the same suite with `spill.policy=fail`, which records where the cliff is without the mechanism.
