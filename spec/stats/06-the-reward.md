# 6. The reward

Execution knows things planning guessed at. This document is about what happens to that knowledge, and it is the one place in this directory that contradicts an existing decision, so it starts with the contradiction.

## 6.1 The prohibition

`../planner/09-runtime-filters-and-adaptivity.md` section 09.5:

> **What not to build:** mid-query re-planning, a feedback loop that writes observed cardinalities back into a catalog for the next query, or anything that makes the same query run differently the second time. All three make the engine's behaviour depend on history, and `spec/02-the-goal.md`'s floor is a promise about every run, not about the average one. A learned-from-history optimizer is also a benchmark-scoring device, the second run of a benchmark query is not what a user experiences.

Every clause of that is right and this document does not overrule it. In particular the last sentence is the one that matters most to a project whose entire claim is a performance claim: a system that gets faster the second time is a system whose benchmark numbers are about the benchmark.

What follows is the smallest thing that captures the value of feedback while keeping that promise, and the test it has to pass is stated first so it can be checked rather than argued about:

**The test.** A read-only workload, run twice against the same file, produces byte-identical plans with byte-identical statistics provenance, in the same order, forever. If a design fails that test, it is not in this directory.

**One clarification, because it looks like a violation and is not.** Document 04 says no query ever waits on a statistic: a statistic that is written but not yet resident returns `Unknown` for this statement and schedules its own load. So the first execution against a cold database can get a different plan from the second, once the load has landed. That is a fact arriving late, not a preference learned from a timing. The second plan is the one the data always justified and the first was made with part of the data unread, so the plan is still a function of the data and not of history. The consequence for the test above is procedural rather than substantive: a plan stability corpus warms the statistics service before it records, and `EXPLAIN` prints `Unknown` facts rather than hiding them, so that a diff caused by a cold load is attributable on sight. `../planner-v2/04-facts-not-estimates.md` section 4.4 is the planner side statement of the same rule.

## 6.2 Three tiers, and only the first is on

**Tier 0, observe and report. Always on.** Every operator records estimated against actual. The numbers go into the metrics stream `rudb-metrics` already carries, into `EXPLAIN ANALYZE` as estimate, actual and ratio, and into the q-error distribution `../planner/06-cardinality-and-cost.md` section 06.5 requires be published. Nothing is written to any file and no future plan changes. This tier is pure instrumentation and it is most of the value, because the thing wrong with a bad estimate is usually that nobody knew it was bad.

**Tier 1, verified facts, committed at explicit boundaries. On by default.** Execution sometimes *proves* a statistic. A full aggregate over a column establishes its exact distinct count. A completed join establishes the exact match fraction between two columns of a given generation. A completed scan with a predicate establishes that predicate's exact selectivity over that generation. These are not learned preferences; they are measurements of data that the engine happened to pay for already.

They may be kept. They may **not** be committed as a side effect of the query that discovered them. They accumulate in the in-memory log of section 6.4 and are written into the file only at a point the user can see and name: a checkpoint, an explicit `ANALYZE`, or a write transaction that is already rewriting the table.

That restriction is what preserves the test in section 6.1. A read-only workload never crosses a commit boundary, so its plans never change. A workload that writes gets better statistics at the moment it writes, which is the moment its data changed anyway.

**Tier 2, timing-derived preference. Off by default.** A contextual bandit over the registered alternatives at a seam, which is `../engine-v2/`'s `Policy::Adaptive` at F10, following Piece of CAKE. Choices are made from observed *timings*, which is the thing section 6.1 forbids by default and permits under a setting. Its rules are in section 6.7 and the first one is that it carries the replay guarantee `../engine-v2/16-milestones.md` already specifies: an adaptive run records its choices and re-running with them pinned reproduces it exactly.

## 6.3 What is observed

Keyed by a **property of the data**, never by query text. This is LEO's design decision and it is the one that makes feedback generalise: a correction attached to a query helps that query, and a correction attached to a column and a predicate class helps every query that touches it.

| observation | key | what it proves |
| --- | --- | --- |
| actual rows out of a scan with a predicate | column, predicate class, constant bucket, generation | the exact selectivity for that generation |
| actual distinct groups from a completed aggregate | column set, generation | the exact distinct count |
| actual match fraction and output rows from a completed join | column pair, generation | the exact join cardinality |
| unmatched rows in an outer join | column pair, generation | whether a declared relationship is total |
| peak memory against the reservation | operator kind, schema shape | the reservation's error and its direction |
| filter effectiveness measured in the bail-out counter | filter site | whether building it was worth it |
| chosen algorithm and its wall time | seam, context | tier 2 only |

**Predicate constants are bucketed, never stored.** A statistics section that recorded the literal values a user filtered on would be a file that leaks query history to anyone who later receives the file, and the performance justification does not survive that sentence. What is stored is which quantile bucket the constant fell in, which is what a selectivity correction needs and is not reconstructible into the original value.

## 6.4 The log

`RUDBFB1`, one per table, from document 03 section 3.2. Bounded: a fixed number of entries per column, a ring that overwrites the oldest, and an exponential decay on the weight of an observation by generation distance, so that a fact about data from twenty generations ago does not outvote one about the data as it is now.

Entries older than a configurable number of generations are dropped rather than decayed to irrelevance, because the file has a budget and a log that only grows is the thing that makes people turn a subsystem off.

The log is read at plan time exactly like any other statistic, through the same `Known`/`Unknown` interface of document 04 section 4.1, and an observation-derived number is reported with source `observed` so `EXPLAIN` distinguishes it from a sketch-derived one.

## 6.5 The correction rules

Directly from memory grant feedback, which is the member of this family with the best operational record:

**Correct one scalar at a time.** A selectivity, a distinct count, a reservation. Never a plan, never a join order, never an algorithm, those follow from the corrected scalar through the ordinary cost model, which keeps one decision-making path rather than two.

**Clip.** A correction may move an estimate by at most a bounded factor per application. An observation that says the estimate was wrong by 10,000x moves it by the cap, and if the observation is real it will be made again.

**Damp and detect oscillation.** Corrections in alternating directions on successive commits are the documented failure mode of this entire literature. Two reversals disable feedback for that key and record why, and the record is visible in `EXPLAIN` and in the system view of document 08. An automatic off switch that says why it fired is worth more than a cleverer update rule.

**Never overwrite a better class.** An observation is `Exact` only for the generation it was taken on. Applied to a later generation it is `Estimated`, and it may never replace a number that is `Exact` for the current generation. An observation is a correction to a guess, not a promotion to a proof.

**Never correct toward a more optimistic plan without a floor.** Under-estimation is the direction that produces plans that fall over. A correction that reduces an estimate is applied with a tighter clip than one that raises it.

## 6.6 What feedback may and may not change

**May change: which statistics exist.** The observation log is what decides that a column deserves per-stripe sketches (document 03 section 3.8) or that a column pair deserves a dependence sketch (section 3.7). Those decisions are made at the next checkpoint, they change what is stored, and they make the file's contents workload-shaped over time. That is intended.

**May not change: what a stored statistic says.** A sketch says what it says. A certificate certifies what it certifies. Feedback lives in its own section and is combined with the others at plan time, visibly.

**May not change: an answer.** Same invariant, same ablation, same test on every commit.

## 6.7 Tier 2's rules, if it is ever switched on

1. Only over alternatives that are all correct, at a registered seam. `crates/rudb-seam` is where that registry lives.
2. Never across the never-slower floor: an arm that has lost badly is not re-explored during a query whose plan has a known-good alternative.
3. Every choice recorded, every run replayable by pinning, per F10's guarantee.
4. `EXPLAIN` shows the choice and its provenance, default rule, pin, or policy, which `../engine-v2/04-modularity.md` already requires.
5. **No published benchmark number is produced with tier 2 on unless the report says so in its header and also reports the pinned run.** `../bench/tpc-h/05-the-measurement.md` and `../15-rudb-bench.md` are the governing rules; this clause exists because tier 2 is exactly the mechanism that makes a benchmark's second run flatter the engine.

## 6.8 What `EXPLAIN` has to show

The statistics generation the plan was made against. For each cardinality: the estimate, its class, and its source. After execution: the actual, and the ratio. And a line naming any key whose feedback is currently disabled by section 6.5's oscillation rule.

That output is the deliverable of this document as much as the mechanism is. A feedback system nobody can see is a feedback system nobody can debug, and the difference between this and a learned model is precisely that every number here can be pointed at.

## 6.9 Excluded, with reasons

**Learned cardinality and cost models.** `../planner/06-cardinality-and-cost.md` section 06.6, unchanged, for the product reason.

**A plan cache keyed by query text.** It makes the second run fast, which is the thing section 6.1 is about.

**Sharing observations between files or sending them anywhere.** The log lives in the file it describes. Nothing about statistics is transmitted off the machine, and the bucketing rule of section 6.3 exists so that even the file does not carry query constants.

**Mid-query re-planning.** `../engine/12-adaptivity.md`, unchanged.
