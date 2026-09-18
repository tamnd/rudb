# Open questions

What this design does not know. Each has a trigger: the measurement that would answer it, and the milestone where that measurement exists.

## 1. Does the seam tax stay bounded?

Twenty-seven seams, each a dynamic dispatch. The rule in [`04-modularity.md`](04-modularity.md) section 3 says a seam is crossed once per chunk, and if that holds the tax is unmeasurable. If three or four seams leak into row granularity, the tax is an order of magnitude and this design is worse than v1's, which just picks an implementation.

This is the largest risk v2 carries and it is a risk v1 does not have.

**Trigger:** F1. Run ClickBench with the instrumentation and dispatch shims compiled out entirely, a build feature, not a runtime setting, and compare. If the difference exceeds three per cent, seams get collapsed, starting with the ones whose alternatives never win a sweep.

## 2. Is 122,880 the right chunk width?

DuckDB uses 2048 for chunks and 122,880 for row groups. Velox and v1 use 1024. This design sets both to 122,880 so a chunk is a block, which is what makes whole-block encoded operations possible without cross-block state.

That may be too large. A 122,880-row chunk of ten columns is several megabytes and does not stay in L2, which is the argument for 2048 in the first place. The design's answer is that kernels tile internally at 1024 for cache and that the chunk is a unit of plumbing rather than of locality, but that is an assertion.

**Trigger:** F4. Sweep `morsel.size` over 2048, 8192, 32768, 122880 and 1048576 on ClickBench and TPC-H, with chunk width both tied to and independent of morsel size.

## 3. Which columns deserve a global dictionary?

The mechanism in [`05-data-model.md`](05-data-model.md) section 7 is what makes string group-by cheap, which is three of the seven expensive ClickBench queries. It is opt-in per column, chosen by a heuristic over the KMV distinct count against a size budget.

Nobody knows what that budget should be. A column with a hundred million rows and eighty million distinct values gains nothing and costs a lot. `URL` on `hits` may be that column.

**Trigger:** F7. Build the dictionary for every string column regardless of cost, measure, then remove them one at a time.

## 4. Does the heavy-hitter path actually win?

`topk=heavy-hitter-two-pass` turns O(distinct) memory into O(k) at the price of a second pass. The prediction is that it wins comfortably on ClickBench Q32 and Q18, where the group count is enormous and the first pass is bandwidth-bound.

The prediction could be wrong in two ways. The second pass might not be cheap if lazy materialisation cannot restrict it to the blocks that matter. And a distribution with no heavy hitters cannot be certified, forcing a fallback that costs a wasted pass.

**Trigger:** F5 for the mechanism, F7 for the number. Measure the fallback rate across the whole suite, not just the queries it was built for.

## 5. Does join ordering still matter?

"Debunking the Myth of Join Ordering" argues that with robust predicate transfer applied, join order matters far less than the literature assumes. If true on our workload, most of [`12-optimizer.md`](12-optimizer.md) section 4 is wasted effort and the work belongs in section 5.

**Trigger:** F8, and the experiment is two lines once both are registered passes: JOB and CEB with each alone and both together. The gate requires the answer to be written back into that document.

## 6. What is the write path actually worth?

M1 measured 5 MB/s of values per core, which is 5.6 CPU hours for `hits` against DuckDB's 126 seconds. The design's answer is that the chooser is the cost, not the encoder, and that choosing on a sample of blocks recovers most of it. That is a guess about where the time goes.

**Trigger:** F2, and the first thing to do is profile the existing encoder rather than redesign it. The gate, load within 2x of DuckDB, is 80x away and it is the milestone most likely to slip.

## 7. Is one buffer manager fast enough?

Every access to operator state goes through a page indirection. The claim is that this is noise. For a hash table probe, which is already a cache miss, it probably is. For a tight aggregate update over an array-grouped table, it might not be.

**Trigger:** F3 and F5. Measure the array-grouped aggregate with state in pages and with state in a raw allocation, and if the gap is real, permit a pinned-for-the-query fast path with the pinning counted against the budget.

## 8. How much does instrumentation cost in a published run?

Two clock reads per operator per chunk. At 122,880 rows and ten operators that is nothing. It is on by default in published runs, which is a decision made for honesty rather than for speed.

**Trigger:** F1, measured once with `--no-instrument`, and recorded in the ledger so the tax is a number. If it exceeds one per cent, the sampling rate drops rather than the instrumentation being turned off, because a published number produced by an engine configured differently from the shipped one is a different kind of dishonesty.

## 9. Is F0's throwaway estimate right?

Six to eight weeks, roughly two thousand lines written to be deleted, except that they become reference implementations and are not deleted. If F0 takes four months, the argument for skeleton-first weakens considerably, because its whole case is that the early number is worth the early cost.

**Trigger:** F0 itself. This is the one question the plan answers by being executed.

## 10. What happens if F7 lands at 4x?

[`../02-the-goal.md`](../02-the-goal.md) section 2.7 already names four gates that would make the project not worth doing, and one of them, M1's on-disk target, has already failed and been recorded honestly, which is the best evidence available that this project can survive a bad number.

The design's position: 4x to 6x on ClickBench, with a genuinely lower CPU-seconds and peak-RSS profile and full DuckDB compatibility, is a good engine and a failed claim, and the right response is to say both. What it is not is a reason to keep the claim and adjust the benchmark.

**Trigger:** F7, and the response is written down now, before the number exists, on purpose.

## 11. Things nobody has asked yet

Transactions. `rudb-txn` is nine lines and this design says nothing about concurrency control, which is fine for an analytical engine reading immutable blocks and not fine the day somebody writes while somebody reads.

Multi-query workloads. Everything here assumes one query at a time. The thread pool is per instance, the memory hierarchy has a query level, and the VLDB 2026 predictive buffer policy targets concurrent scans, so the hooks exist, but no gate measures concurrency and no suite tests it.

Vectorised execution on nested types. Struct, list and map are one variant of `Form` and one paragraph, and they are where a surprising amount of real analytical work now lives.

GPU. [`02-research-2026.md`](02-research-2026.md) section 2.3 explains why not, and the explanation has a shelf life.
