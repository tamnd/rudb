# Encoded execution

The milestone the project exists for. Everything before F7 is what makes F7 possible and attributable.

## 1. The claim being tested

[`../02-the-goal.md`](../02-the-goal.md) states the thesis: the order of magnitude is in the physical layout of the data, not in the execution of the operators. The Bespoke OLAP ablation prices operator work, compilation, fusion, prefetching, kernel quality, at about twenty-six per cent, and layout specialisation at 12.35x on TPC-H and 51.40x on CEB. The 2026 paper's end-to-end numbers are 11.78x and 9.76x over DuckDB.

Ten times DuckDB exists. This document is the general-purpose engine's version of how.

The difference between their mechanism and ours: they hard-code a layout and kernels for a known workload, which produces an artefact that cannot answer an unanticipated query. We choose a layout at load time from the data, and write kernels that consume the layouts we chose. We will get less than they do, a general engine always does, because it keeps the ability to answer the other query, and the planning number in [`../02-the-goal.md`](../02-the-goal.md) reflects that: 9x to 11x if three mechanisms land, 4x to 6x if one fails.

## 2. Where the time actually is

Seven ClickBench queries are 70.4% of Umbra's 8.10 seconds. Hot seconds, Umbra against DuckDB:

| Query | Umbra | DuckDB | Shape |
|---|---|---|---|
| Q28 | 1.393 | 6.478 | `REGEXP_REPLACE` over a hundred million URLs |
| Q32 | 1.323 | 2.035 | `GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10` |
| Q18 | 0.846 | 1.650 | High-cardinality group by, top k |
| Q34 | 0.732 | 2.198 | String group by |
| Q33 | 0.730 | 2.054 | String group by |
| Q16 | 0.368 | 0.892 | Group by `UserID` |
| Q13 | 0.309 | 0.781 | `COUNT(DISTINCT UserID) GROUP BY SearchPhrase` |

Three technical problems cover all seven. Sections 3, 4 and 5 are one each. If all three land the arithmetic reaches 2.4 to 2.9 seconds, which is 9x to 11x. If any one fails it lands at 4x to 6x.

## 3. Grouping without strings

**The problem.** Q33, Q34 and Q13 group by a string column. Every row hashes a string, compares a string on collision, and stores a string in the table. At a hundred million rows that is the query.

**The mechanism.** A column-scoped global dictionary from [`05-data-model.md`](05-data-model.md) section 7 makes the group key a `u32` code. The hash is a multiply. The comparison is an integer compare. The stored key is four bytes. The finalise gathers strings once, for the output rows only, a few thousand instead of a hundred million.

**The second-order win, which is larger.** When the code space is small enough, the hash table stops being a hash table. `agg.state=array-grouped` indexes an array by the code directly: no hashing, no collisions, no probing, perfect locality, and it parallelises by partitioning the code range with no merge step at all. For a column with a few million distinct values this is the fastest aggregate that exists.

**What has to be true.** The dictionary must be column-scoped rather than block-scoped, which is a write-path decision. It must fit in memory during the query, which bounds which columns get one. And the aggregate must accept `Form::Dictionary` with `DictSource::Global` through `FormAware`, which is the form negotiation in [`05-data-model.md`](05-data-model.md) section 8 doing its job.

**What could go wrong.** A column whose distinct count is close to its row count gets no benefit and costs the dictionary's space. `URL` on `hits` is close to that case, M1 found `URL → URLHash` held at 0.995 on a sketch and failed on 11,586,966 actual rows, which is a different fact but tells the same story about that column's cardinality. The heuristic that decides is the risk, and it is why the global dictionary is opt-in with an override and why `EXPLAIN` prints which columns have one.

## 4. Top-k without materialising the distinct set

**The problem.** Q32, Q18 and Q16 build a hash table with tens of millions of groups and return ten rows. The memory is O(distinct) and the time is dominated by cache misses on a table far larger than L3.

**The mechanism.** Two passes. The first uses a bounded Space-Saving or Misra-Gries sketch to find the candidate heavy hitters, in O(k) memory with no random access into a large table. The second counts the candidates exactly.

Exactness is not negotiable. This is a DuckDB-compatible engine, an approximate `COUNT(*)` is a wrong answer, and the guarantee a sketch gives, that the true top-k is contained in the candidate set when the candidate set is large enough, is what the second pass converts into an exact answer. The candidate set is sized generously and the operator falls back to the full hash aggregate when the sketch's error bound cannot certify containment.

**What has to be true.** The second pass has to be cheap, which means the first pass must not have destroyed the opportunity to re-read cheaply. On the native format with lazy materialisation, the second pass reads only the group key column and only the blocks the first pass flagged, which is a small fraction of the data.

**What could go wrong.** A distribution with no heavy hitters, a hundred million distinct values each appearing once, has no top-k worth finding, and the sketch cannot certify anything. The operator detects this from the sketch's own error bound and falls back. The cost of the fallback is one wasted pass, and the cost model has to be good enough that this is rare.

## 5. String transformation without a regex engine

**The problem.** Q28 is `REGEXP_REPLACE` over a hundred million URLs, and it is 1.393 seconds of Umbra's 8.10, the single most expensive query on the board. DuckDB takes 6.478. Sirius gets thirteen times on it with a GPU, which is a result about not running a general regex engine on a hundred million strings more than it is a result about GPUs.

**The mechanism.** Two things, and the second is the one that matters.

A pattern analyser, which examines the compiled regex and recognises the shapes that are not really regexes. The URL-authority pattern that Q28 uses compiles to a `memchr` for `/` plus two comparisons. `rudb-regex` is 2,483 lines and already has a parser to analyse. This is worth a large constant factor and it is ordinary engineering.

Running the transformation on the dictionary rather than on the rows. If `URL` has a column-scoped dictionary, `REGEXP_REPLACE(URL, ...)` is applied to each distinct value once and the result is a new dictionary over the same codes. A hundred million applications becomes a few million, and the output column is still a dictionary and still compact for whatever consumes it.

That second mechanism generalises to every pure scalar function of one dictionary-encoded column, which is a large fraction of the string work in ClickBench. It is the clearest single illustration of the whole thesis: the same operation, the same kernel, made an order of magnitude cheaper by the layout of the input.

**What has to be true.** The function must be pure and deterministic, which is a property the function registry declares. The dictionary must be column-scoped. The result must be materialisable as a dictionary, which is true for scalar functions and false for anything that can produce a different number of rows.

**What could go wrong.** A column with almost as many distinct values as rows gains nothing, which is the same risk as section 3 and is mitigated the same way.

## 6. The predicate layer, which is not one of the three but is most of the rest

Predicates evaluated on encoded data, each with a property test against the decoded reference path. This is not where the order of magnitude is; it is where the difference between 4x and 6x is.

**Dictionary.** A comparison against a constant becomes a comparison against the dictionary, a few million operations instead of a hundred million, producing a mask over codes, then a gather. With a sorted dictionary, a range predicate becomes a code range, and `LIKE 'prefix%'` becomes a code range too.

**Bit-packed with a frame of reference.** Transform the constant rather than the data: `x > c` on values stored as `(x - base)` at `w` bits becomes a packed comparison against `c - base`, with the out-of-range cases answered from the block's min and max without touching the data at all. FastLanes' interleaved layout is what makes this a comparison rather than a shuffle.

**RLE.** Evaluate once per run. A column with a thousand runs over a block of 122,880 rows costs a thousand comparisons.

**FSST.** Equality against a constant compresses the constant with the block's symbol table and compares bytes. Prefix matching works on the compressed form when the symbol table is prefix-preserving, and falls back when it is not.

**Block statistics.** Before any of the above, min, max and null count answer a great many predicates for a whole block without reading it. This is free and is the first thing the scan does.

## 7. The bridge

The mechanism that makes all of the above reachable rather than theoretical, restated from [`05-data-model.md`](05-data-model.md) section 8 because this is where it pays off.

Each kernel declares the forms it accepts. The planner propagates form sets from the scan upward. Where they do not intersect, a `Decode` node is inserted, named in `EXPLAIN`, counted in the metrics, and attributed to the operator that forced it.

The consequence is a work queue. Run ClickBench, read the metrics, sort the decode sites by bytes decoded, and the top of that list is the next kernel to write. That is a very different way to spend F7 than guessing, and it is the reason the metrics document from [`14-metrics.md`](14-metrics.md) is a first-class output rather than a debug feature.

## 8. Gate

F7 is the only milestone whose gate is the project's headline claim, so it is stated precisely.

**Primary.** ClickBench hot total on the reporting machine, under the published rules, below 2.63 seconds. That is ten times DuckDB's 26.25 and 3.1x past Umbra's 8.10.

**If it does not land, what lands instead.** The seven queries in section 2, individually, against Umbra and DuckDB, each with the DuckDB-to-Umbra ratio printed beside it per the axis-3 rule. A result of 4x to 6x is the outcome [`../02-the-goal.md`](../02-the-goal.md) predicts if one of the three mechanisms fails, and it is reported as such, with which one failed and why.

**The ablation, which is mandatory whichever way the primary goes.** The same suite with each mechanism switched off independently: `storage.dictionary=per-block`, `topk=bounded-heap`, `scan.materialisation=eager`, and the pattern analyser disabled. Four extra runs, and they are what turn "we got 10x" into "here is where the 10x came from", which is the difference between a result and a claim.

**The honest comparison.** Against Bespoke OLAP's numbers, because they are the closest thing to an upper bound anybody has published, and because a general engine reaching a meaningful fraction of a synthesised one is the interesting result regardless of which side of 2.63 it lands on.
