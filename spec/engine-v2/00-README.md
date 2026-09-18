# The engine, version two

A second design for the rudb execution engine, written from first principles on 12 September 2026.

This directory is a sibling of [`../engine/`](../engine/), not a replacement for it. That directory holds v1: a ten-layer bottom-up plan with sub-milestones 2a through 2m, being built by other people right now. Nothing here edits anything there. The two exist side by side so that they can be compared on their merits, and so that whichever one is wrong can be seen to be wrong against something concrete rather than against a feeling.

## Why there is a second design

v1 is a careful plan. Its findings are real and this design inherits every one of them: the `Value`-per-row kernel dispatch, the `Key(Vec<Value>)` grouping cost, the `Vec<Vec<Value>>` sort, the nine-line optimizer. Those are facts about the code and they do not change because the plan around them changes.

What v2 disagrees with is the shape of the plan, in five places.

**v1 measures at the end of each layer. v2 measures from the first commit.** v1's layer 1 gate is "CPU seconds down 5x versus 2a on ClickBench Q1 through Q5", and v1's own baseline document records that `Rudb` in the harness is a stub which declares it cannot run. A plan whose first gate is a ratio against a number the engine cannot produce is a plan that discovers its integration problems last. v2's first milestone, F0, is an end-to-end skeleton that is allowed to be the slowest engine on the board as long as it is on the board.

**v1 puts the scheduler at layer 8. v2 puts it at F4 and puts its *interface* at F0.** v1 argues, correctly, that the interface has to be right and the implementation does not. v2 agrees and draws the opposite conclusion: if the interface has to be right, write the interface first and run the single-threaded implementation behind it, rather than writing a pull-model `next()` for eight layers and converting later. v1's own scheduler document describes that conversion as touching every operator.

**v1 defers the buffer manager to layer 8. v2 makes memory a currency at F3 and never lets an operator allocate outside it.** Larger-than-memory is one of the four things the user asked to be designed properly. It is not a late feature. It is a constraint on the shape of every stateful operator, and constraints of that kind are cheap to honour early and expensive to retrofit.

**v1 treats layout as storage and execution as execution. v2 treats them as one problem.** [`../02-the-goal.md`](../02-the-goal.md) already contains the argument: the order of magnitude is in the physical layout of the data, and the Bespoke OLAP ablation prices the entire "we made the operators fast" story at about twenty-six per cent. In 2026 that argument acquired an existence proof, Bespoke OLAP v2 reports 11.78x on TPC-H and 9.76x on CEB against DuckDB, obtained by hard-coding workload-specific columnar layouts and per-template kernels. Ten times DuckDB is reachable. It is not reachable by writing better hash joins. v2 is organised so that the milestone where the layout meets the kernels, F7, is the milestone the project exists for, and F0 through F6 are what make F7 possible and attributable.

**v1 is a plan for building an engine. v2 is a plan for building a laboratory that happens to be an engine.** This is the request that arrived last and changed the most: every mechanism that a paper could improve must be swappable, and swapping must be cheap enough that a researcher can test a new paper in an afternoon and get an attributable number out. That is not a nice property to have at the end. It is the spine, and [`04-modularity.md`](04-modularity.md) is the document the rest of this directory is written around.

## How to read this

Nineteen documents, in three groups.

The argument, which is short and which everything else follows from:

- [`01-first-principles.md`](01-first-principles.md), eight principles, and what each one rejects
- [`02-research-2026.md`](02-research-2026.md), the papers, what we take from each, and what we deliberately do not take
- [`03-end-to-end.md`](03-end-to-end.md), the walking skeleton: SQL to plan to `EXPLAIN` to execution to metrics to the board
- [`04-modularity.md`](04-modularity.md), seams, the strategy registry, policies, variant sweeps, the conformance oracle

The engine, bottom to top, but every one of these is a description of an interface first and an implementation second:

- [`05-data-model.md`](05-data-model.md), sequences, forms, compactness, the global dictionary, form negotiation
- [`06-storage.md`](06-storage.md), blocks, the layout contract, the write path, zone maps and sketches
- [`07-memory.md`](07-memory.md), one buffer manager, operator-owned eviction, spilling, accounting
- [`08-execution.md`](08-execution.md), push pipelines, morsels, the three operator traits, backpressure, cancellation
- [`09-parallel-and-distributed.md`](09-parallel-and-distributed.md), the scheduler, exchange as a plan node, the distributed seam
- [`10-expressions.md`](10-expressions.md), expression programs, selection threading, the kernel registry
- [`11-operators.md`](11-operators.md), the operator catalogue and what each one is allowed to assume
- [`12-optimizer.md`](12-optimizer.md), the plan IR, the rule registry, statistics from sketches, join ordering
- [`13-encoded-execution.md`](13-encoded-execution.md), the layout-to-kernel bridge, where the order of magnitude is

The apparatus, which is not support work:

- [`14-metrics.md`](14-metrics.md), the metric schema, `EXPLAIN ANALYZE`, benchmark integration, the honesty rules
- [`15-testing.md`](15-testing.md), the differential oracle, property tests, deterministic simulation, fuzzing
- [`16-milestones.md`](16-milestones.md), F0 through F11, dependencies, and the gate for each
- [`17-code-layout.md`](17-code-layout.md), crates, module boundaries, and the lints that enforce them
- [`18-comparison-with-v1.md`](18-comparison-with-v1.md), a point-by-point table, so the comparison the user asked for is written down rather than implied
- [`19-open-questions.md`](19-open-questions.md), what this design does not know

## The rule

v1's rule was that a layer is not done when it works, it is done when it is measured against DuckDB and ClickHouse and the number is better. That rule is right and v2 keeps it, with one addition that follows from the modularity requirement:

> A milestone is not done when it is measured. It is done when the mechanism it introduced can be switched off, the engine still answers every query correctly with it off, the harness has a number for both, and the difference between the two numbers is in the ledger with the milestone's name on it.

An unswitchable improvement is an unattributable improvement, and an unattributable improvement is indistinguishable from noise the day somebody asks where the ten times came from.

## Where this was written against

DuckDB `v2.0.0-alpha39998`, GA projected for the second half of October 2026. ClickHouse 25.x with lazy materialization on by default and the query condition cache available. Polars with the rewritten streaming engine, out-of-core group-by, equi-join and sort landed. DataFusion post-StringView. The board on `c6a.4xlarge` as recomputed on 10 September 2026, where Umbra is at 8.10 seconds hot, DuckDB at 26.25, and ten times DuckDB is 2.63.

The rudb workspace at commit `ab1510c`: twenty-nine crates, 87,051 lines, zero external dependencies, edition 2024, Rust 1.85.
