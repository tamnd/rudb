# v1 against v2

The comparison the user asked for, written down rather than implied. v1 is [`../engine/`](../engine/), fifteen documents, layers 1 through 10, sub-milestones 2a through 2m.

## 1. What v2 takes from v1 unchanged

More than it changes. v1's findings are facts about the code and they survive any rearrangement of the plan.

The `Vector::value_at` per row across 31 call sites in five kernel files, and the judgement that it is the single most valuable finding in six months of building. The `Key(Vec<Value>)` grouping cost. The `Vec<Vec<Value>>` sort that is a billion allocations at 100M × 10. The nine-line optimizer, now grown to 4,606 lines. The observation that rudb could not read a file, now partly fixed.

The design decisions: `Validity` as three cases. `Selection` threaded rather than applied. `Holds` as a refcounted pin handle rather than a lifetime parameter, so chunks stay `Send`. `Form` non-exhaustive. The four blocked reasons as a closed set, chosen over Polars' tokens because deadlock becomes enumerable. Two thread pools, compute and I/O. Cancellation at chunk granularity. The pull root for the C API. The nested loop join kept permanently as oracle and non-equi fallback. Float determinism per thread count rather than absolute. `COUNT(DISTINCT)` as a second aggregation. Cascades rejected. The `AggregateFunction` interface with fixed state size and a serialise pair. Async I/O as not-optional and not-late. Chunk compaction as not-a-constant. The whole of the measurement discipline: subprocess fairness, `getrusage`, `/proc/<pid>/io`, `Role::may_publish`, distributions with IQR, the DuckDB-to-Umbra ratio on every per-query claim.

And the rule that a layer is not done until it is measured and published and better.

## 2. Where they differ

| | v1 | v2 |
|---|---|---|
| **Shape** | Ten layers, bottom-up | Twelve milestones, skeleton-first |
| **Naming** | 2a to 2m | F0 to F11 |
| **First deliverable** | The data plane (layer 1) | A running query engine (F0) |
| **First measurement** | End of layer 1, as a ratio against a baseline the engine cannot produce | F0, as an absolute number on the published board |
| **Operator interface** | Pull `next()`, converted to push at layer 8 | Push `Source`/`Stream`/`Sink` from F0, serial driver behind it |
| **Scheduler** | Layer 8 | Interface F0, implementation F4 |
| **Memory** | Buffer manager at layer 8 | `Budget` at F3, and no allocation outside one, ever |
| **Spilling** | Buffer manager evicts | Manager asks, operator chooses (Umami/Otaki) |
| **Storage** | Layer 3 is a Parquet reader; native format is M-level work | F2 is the native format, because layout is the thesis |
| **Encoded execution** | Inside layer 3, as predicate pushdown | F7, its own milestone, the project's headline gate |
| **Hash structures** | Decided: unchained for join, open addressing for aggregate | Four registered, default chosen by sweep |
| **Optimizer** | Layer 9 | F8, after F7, deliberately |
| **Adaptivity** | Layer 10, a framework designed from scratch | F10, a learner over a registry that already exists |
| **Distributed** | Not addressed | Interfaces from F0 (`Exchange`, pointer-free state), code at F11 behind a trigger |
| **Modularity** | Implicit; decisions written into the plan | The spine; 27 seams, registries, policies, sweeps |
| **Testing** | Per-layer, plus property tests | One differential oracle over a never-deleted reference, times a sampled configuration space |
| **Metrics** | External, via the harness | External via the harness *and* internal via a versioned document, cross-checked within 5% |

## 3. The three disagreements that actually matter

Most of the table above is rearrangement. Three rows are substantive and one of them might be wrong.

**Skeleton-first versus bottom-up.** v2's position is that a plan whose gates are ratios against a baseline the engine cannot produce will discover its integration problems last, and that the throwaway cost is smaller than it looks because the thrown-away code becomes the oracle. v1's position, which its README states, is that the interface has to be right and the implementation does not, and that building carefully upward gets the interfaces right.

The honest assessment: v1's argument is good and v2 does not refute it, it redirects it. If the interface is what matters, write the interface and run the worst possible implementation behind it. That is F0. The disagreement is not about interfaces, it is about whether you can validate one without a consumer.

**Push from the start.** This one v2 is confident about. The pull-to-push conversion at layer 8 touches every operator, and the serial push driver is twenty-two lines. There is no argument for paying that conversion cost, and v1's own operator doc comment already describes the push interface while sitting above a pull one.

**Layout as its own milestone.** v1 treats encoded predicates as a sub-part of the scan layer. v2 makes it F7 with the headline gate on it. The evidence arrived after v1 was written: the Bespoke OLAP ablation, 12.35x from layout against 1.26x from code on TPC-H, and 11.78x end to end. If that evidence is right, then an engine that treats encoded execution as a scan optimisation has misallocated its effort, and if it is wrong then v2 has bet the plan on one paper.

**Where v2 might simply be wrong:** the seam count. Twenty-seven seams, each with two or more implementations, is a lot of surface. If the per-chunk dispatch rule leaks, if even three or four seams end up being crossed per row, the tax is not bounded and the design is worse than v1's, which just picks. The lint in [`17-code-layout.md`](17-code-layout.md) section 4 is the defence and it is a crude one. This is the single largest risk v2 carries that v1 does not, and it should be checked at F1 with the instrumentation overhead measured rather than assumed.

## 4. What each is better at

**v1 is better at** getting each layer right in isolation, at not writing code twice, and at being a plan that a small team can execute without holding the whole system in mind. It is also a more conservative plan, which for a project with a claim this aggressive is a real virtue.

**v2 is better at** producing a number early, at attributing every later number to a mechanism, at larger-than-memory and multi-core because both are constraints from the start rather than features later, and at being a thing a researcher can contribute to. It is a more expensive plan and a riskier one.

## 5. How to compare them for real

Not by reading. Both directories describe an F0-or-2a-shaped first step that is small enough to build.

v1's 2a is a baseline: five engines measured against each other, no rudb. That already exists in `rudb-bench`.

v2's F0 is a running engine, slow, on the board. That is six to eight weeks.

The comparison that decides is: at the end of the first quarter, which plan has a number, and is that number attributable to anything. Everything else is preference.
