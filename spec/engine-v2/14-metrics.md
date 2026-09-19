# Metrics

Capture all metrics was one of the four things the user asked for. This is the schema, the plumbing, and the honesty rules.

## 1. The document

Every execution produces one, whether it succeeded, was cancelled, or failed. A failed query's metrics are the most useful metrics there are.

```json
{
  "schema": 1,
  "query": { "sql": "...", "hash": "...", "suite": "clickbench", "id": "q32" },
  "engine": { "version": "0.3.0", "commit": "ab1510c", "build": "release" },
  "machine": { "name": "server3", "cores": 8, "memory": 24696061952, "os": "linux" },
  "settings": { "memory_limit": 1073741824, "threads": 8, "pinned": {} },
  "timing": {
    "parse_ns": 41000, "bind_ns": 88000, "optimize_ns": 310000,
    "physical_ns": 44000, "execute_ns": 1323000000, "total_ns": 1323483000
  },
  "resource": {
    "cpu_ns": 9880000000, "peak_bytes": 894000000,
    "bytes_read": 1420000000, "bytes_decoded": 210000000,
    "bytes_spilled": 0, "bytes_read_back": 0, "io_requests": 1180
  },
  "strategies": {
    "hash.table": { "chosen": "unchained", "by": "default",
                    "provenance": "Birler et al., DaMoN 2024" },
    "agg.parallel": { "chosen": "radix-partitioned", "by": "default" },
    "topk": { "chosen": "heavy-hitter-two-pass", "by": "default" }
  },
  "pipelines": [
    { "id": 0, "instances": 8, "depends_on": [],
      "wall_ns": 980000000, "cpu_ns": 7600000000,
      "blocked_ns": { "io": 120000000, "memory": 0,
                      "dependency": 0, "downstream": 3000000 } }
  ],
  "operators": [
    { "id": 3, "pipeline": 0, "kind": "Scan", "table": "hits",
      "rows_in": 0, "rows_out": 99997497,
      "wall_ns": 620000000, "cpu_ns": 4800000000,
      "bytes_read": 1420000000, "bytes_decoded": 210000000,
      "decode_sites": [ { "column": "ClientIP", "form": "BitPacked",
                          "forced_by": "op:7", "bytes": 210000000 } ],
      "blocks_scanned": 814, "blocks_skipped": 0,
      "memory": { "reserved": 0, "high_water": 0, "shrink_calls": 0 },
      "reference_impl": false }
  ],
  "adaptive": [],
  "warnings": [ "operator 7 forced a decode of ClientIP (210 MB)" ]
}
```

Four properties of that shape are decisions.

**Versioned, with a compatibility test.** `schema: 1`. `rudb-bench` reads documents from every commit in the ledger, so a breaking change is a migration, and the test that proves it is a corpus of old documents in the repository.

**Flat arrays with parent ids**, not a tree. It is easier to query, and `SELECT * FROM rudb_metrics()` is a table.

**Strategies are first-class.** Without them a ledger row is not reproducible, and reproducibility is the whole reason [`04-modularity.md`](04-modularity.md) exists.

**Warnings are generated, not written.** Anything the engine knows it did badly, a forced decode, a spill, a fallback to a reference implementation, a cardinality estimate off by more than an order of magnitude, becomes a warning automatically. The warnings list is what somebody reads first.

## 2. Where the numbers come from

**Wall time** is a monotonic clock, read twice per operator per chunk by the instrumentation shim from [`08-execution.md`](08-execution.md) section 9.

**CPU time** is per-thread, from `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` on Linux and macOS and `QueryThreadCycleTime` on Windows. It is read once around the whole execution, once around every pipeline's driver and once around every worker, which is where section 3 gets both sides of its cross-check. This is the axis that cannot be bought with threads and it is the one the project's resource claim rests on.

**It is not read per operator per chunk, which is what this section first said.** The wall clock can be, because Linux answers `CLOCK_MONOTONIC` out of the vDSO without entering the kernel and a reading is about twenty nanoseconds. `CLOCK_THREAD_CPUTIME_ID` has no vDSO entry on any Linux this runs on, so every reading of it is a real system call of several hundred nanoseconds, and a chunk of a thousand rows does not cost that much to produce. Four of those per chunk, which is what a source and a sink came to, made a `count(*)` over twenty million in-memory rows take 212 ms where the same count without them takes 15 ms. The engine was fourteen times slower than itself at measuring how slow it was, and it was six times slower than DuckDB on a query where it is now faster.

So an operator's row has a wall column always and a CPU column only when the statement asked for one, which `EXPLAIN ANALYZE` and `PRAGMA enable_profiling` are the two ways of doing. The shim reads the flag off the counters it was built with, so the decision is made once per operator at build time rather than once per chunk. Nothing above the operator changed: `resource.cpu_ns`, the per-pipeline numbers and the per-worker numbers are all still measured on every query, because those spans are taken once per pipeline rather than once per chunk and a system call is nothing next to a pipeline. The cross-check in section 3 is therefore unaffected, and the `driver` column it describes is reported for a profiled run.

**Peak memory** is the buffer manager's own high-water mark internally, and `getrusage`/`GetProcessMemoryInfo` externally from `rudb-bench`. The two are cross-checked and a gap over fifteen per cent means something is allocating outside a `Budget`, which the lint in [`07-memory.md`](07-memory.md) section 3 is supposed to prevent and this is how it is caught when it does not.

**Bytes read** is counted inside `rudb-io` at the point of the syscall, which is the only place it is unambiguous. `rudb-bench` cross-checks against `/proc/<pid>/io` `read_bytes` on Linux for cold runs.

**Bytes decoded** is counted in `Column::flatten` and in `UnifiedAccess`. It has no external cross-check and is the engine's word, which is why it is a diagnostic rather than a published number.

## 3. The cross-check

Principle 8's rule, stated as an assertion the harness makes:

> The sum of per-pipeline `cpu_ns` must land within five per cent of the `cpu_ns` the engine measured around the whole execution.

A run that fails it is marked unusable and does not enter the ledger. The failure means either that time is being spent outside any operator, planning, I/O waits accounted wrongly, a serialisation point nobody modelled, or that the instrumentation is double-counting. Both are worth knowing, and both are invisible without the check.

Four corrections to how this was first written, all from building it in F0.

It is not the external process CPU on the right hand side. The process starts, links itself, opens the data, declares the views and prints the answer, and none of that happens inside an operator, so on any query short enough to be interesting the process number is mostly the process. A five per cent band against it would mark every rudb run unusable and take rudb off the board that F0 exists to get it onto. The span the breakdown describes is the execution, so the execution is what it is compared against. The gap out to the process is still measured and still printed, as the `outside` column of the run report, because it is the honest version of what that sentence was reaching for.

It is the pipelines on the left hand side, not the operators, with the operators as the fallback for a plan that ran as no pipeline at all. A pipeline is the unit the scheduler runs. An operator charges itself for the time inside its own call, and the loop that makes those calls is neither of them, so a check that only added up operators would read the driver as time that went missing. The difference between the two sums is the driver, and the report prints it as its own column.

A pipeline's `cpu_ns` is measured rather than summed. Saying the left hand side is pipelines is not enough on its own, because a pipeline whose time is the sum of its operators still leaves the loop out. So the loop that runs a pipeline has counters of its own and a pipeline reports what its driver charged itself, with the sum of the operators kept as the fallback for a pipeline nobody registered a driver for. Pipelines run inside each other, since a sort is drained by the loop above it while that loop is still running, so a plain span around each of them would count the same nanosecond twice. Every driver of one execution shares a running total and charges itself its span minus whatever was charged while its span was open. That subtraction is exact at any nesting depth and the exclusive times add up to the span around all of them.

Building the tree is off the right hand side. Opening the file, reading its schema, resolving the strategies and allocating the operators all happen before there is a pipeline to charge, so a check that left it in the denominator would read every microsecond of it as time that went missing. The document reports it as `build_cpu_ns` beside `cpu_ns` and the harness compares against the difference.

What that leaves is a check that is close to an identity while the engine is serial, and the design should say so rather than let a column of zeroes read as a passing test. At F0 every pipeline runs on one thread inside one loop and the outermost span is the execution, so exclusive times that add up to the outermost span is arithmetic rather than evidence. It is still worth having in this shape for three reasons: a pipeline nobody drove falls back to the operator sum and comes out short, time spent inside the execution and outside every driver still shows as a gap, and from F4 onwards the execution span stops containing the pipelines and the check has something to say again. The value of the number in the meantime is not the check, it is the `driver` column that the change made visible: on a count over ten million rows, between a third and a half of the query is the loop rather than the operators, which is a finding nobody had before the loop was measured.

The harness reads the external CPU from `/usr/bin/time` rather than from `wait4`, because `rudb-bench` has no dependencies and `wait4` is not in the standard library. Same number, different source. Its resolution is ten milliseconds on macOS, which is coarse enough that the `outside` column saturates to zero there, so that column is a Linux number.

This is also what keeps the internal numbers honest enough to reason from. An engine's self-reported profile is a story it tells about itself; the cross-check is what makes it evidence.

## 4. EXPLAIN ANALYZE

A rendering of the document above, not a separate mechanism.

```
Q32  1.323s wall   9.880s cpu   894 MB peak   1.42 GB read

Pipeline 1  (8 instances, 980ms wall, 7.60s cpu)
  Scan hits                          99,997,497 rows   620ms   4.80s cpu
      blocks 814 scanned, 0 skipped
      1.42 GB read, 210 MB decoded  ← ClientIP forced by TopK
      lazy materialisation: WatchID, ClientIP eager; 103 columns not read
  HashAggregate  WatchID, ClientIP       -> 41,983,110 groups   360ms
      hash.table = unchained (Birler et al., DaMoN 2024)
      agg.parallel = radix-partitioned, 64 partitions
      memory 894 MB high water, 0 spilled
Pipeline 2  (1 instance, 343ms wall, 2.28s cpu)  depends on 1
  TopK 10 by c desc                            10 rows   343ms
      topk = heavy-hitter-two-pass
  * Sort                                       10 rows     1ms   [reference]

warnings
  operator 7 forced a decode of ClientIP (210 MB)
  estimate 2,400,000 vs actual 41,983,110 groups at operator 5 (q-error 17.5)
```

Three conventions, all borrowed and all worth borrowing. Polars colours fallback nodes red; here `*` and `[reference]` mark a node running a reference implementation. Estimated is printed beside actual, so a bad plan explains itself. And the strategy at every seam is named with its provenance, so the first two questions anybody asks are already answered.

## 5. The benchmark harness

`rudb-bench` is 2,196 lines and already has the hard parts: the `Engine` trait, `Suite`, `Distribution` with quartiles and IQR and a `publishable` predicate, `memory.rs`, and `fleet.rs` with `Role::may_publish` false for every machine the project owns.

What F0 adds:

**A real `Rudb` engine**, driven as a subprocess through `rudb-cli`, not in-process. v1 made this decision for fairness and it is right: every rival is a subprocess, and an in-process engine skips process startup and gets a warmer allocator.

**Metrics ingestion.** `--metrics run.json` from the CLI, parsed into the run record, subject to the section 3 cross-check.

**The sweep.** `rudb-bench sweep --seam <seam> --suite <suite>` runs the suite once per registered implementation of one seam with everything else held fixed. This is [`04-modularity.md`](04-modularity.md) section 7 step 4 and it is what makes a new implementation of a paper produce a publishable comparison rather than an anecdote.

**The ledger.** One row per milestone per suite per machine, with the mechanism on and off, generated from committed runs.

## 6. The honesty rules

Inherited from v1's measurement document and from `rudb-bench`'s README, and not renegotiable.

No machine the project owns may publish. `fleet.rs` says so and the reporting machine is a rented `c6a.4xlarge`, because that is the board's machine.

Caches that make a repeated query fast are off in every published measurement. ClickHouse's query condition cache in particular, because it turns a benchmark into a measurement of the cache.

Cold and hot are reported separately, always. Cold is one run on a dropped cache; hot is the best of five with the full distribution, because a median without an IQR hides a bimodal.

Every per-query claim carries the DuckDB-to-Umbra ratio for that query. The median is 4.65, the minimum is 1.25 and the maximum is 24.5, and a 3x win on a query where Umbra is 24.5x ahead of DuckDB is not the same result as a 3x win on a query where it is 1.25x ahead.

A spilled run is marked. A run where the section 3 cross-check failed does not enter the ledger. A run on a strategy registered through the C ABI is marked, because it could not be inlined.

Load time and on-disk size are part of any result measured on the native format. The board already reports them for everyone else.

And the rule that the M1 experience earned: when a measurement contradicts a claim in a specification, the specification is amended and the amendment says what the old number was. [`../02-the-goal.md`](../02-the-goal.md) section 2.4 records the on-disk claim failing by 4.7x against its target, and that record is worth more to the project than the claim was.
