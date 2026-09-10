# Layer six: join

This is sub-milestone 2h. It sits on the hash table from document 06 and it is the layer that closes the largest single gap between rudb and every other engine, because the join today is a nested loop and a nested loop is not slow, it is quadratic.

The module doc in `crates/rudb-exec/src/join.rs` already says this in the right words: this is the one place where the first implementation is not merely slower than what replaces it but asymptotically worse, and a join is what every query past the simplest one is mostly made of. This document is the replacement, and it also says why the nested loop is kept rather than deleted.

## 8.1 What exists today

330 lines. Eight join kinds are implemented and their rules about unmatched rows are all correct: inner, left, right, full, semi, anti, single, and DuckDB's positional. The right side is fully materialized into a `Vec<Chunk>`, and then for each left row the condition is evaluated against a whole chunk of the right side at a time, which keeps the expression evaluator on its batch interface and makes the left row's columns constant vectors.

That is a careful nested loop. It is still a nested loop, so a join of ten thousand rows against fifty thousand is five hundred million condition evaluations, and each evaluation before layer two allocated two `Value`s. That is the measurement from document 00: four corpus files timed out at ten seconds, all four were joins at exactly that scale, and it was the first honest number this project produced about its own execution speed.

`crates/rudb-opt/src/lib.rs` is nine lines. There is no optimizer, which means no join reordering, no filter pushdown and no build side selection, and section 8.5 is about what to do in a layer that arrives three layers before the thing that would normally decide these.

## 8.2 The hash join

Build the right side into the unchained table from document 06, probe with the left side. That is the shape and most of the work is already done by layer four.

What this layer adds on top of the table:

The build side is materialized into a row layout rather than kept as chunks, because the probe emits matched rows by gathering from the build side and a row layout makes that one contiguous read per match rather than one read per column. That is the standard tuple layout for a hash join and it is the same layout the sort in document 09 wants, so it is written once and shared. The layout is column order with fixed-width values inline and variable-length values as views into an arena owned by the operator.

The probe produces, per chunk, a pair of selection vectors: which left rows matched and which build rows they matched. Emitting the result is then two gathers, one from the left chunk and one from the build rows, into an output chunk. That is the whole inner join and it is entirely made of operations that already exist by layer six.

A match count greater than the chunk size has to be handled, because one left row can match many build rows and a chunk of a thousand left rows can produce far more than a thousand output rows. The probe therefore has to be resumable in the middle of a chunk, which is a state machine of two indices, and it is the single most common source of bugs in a hash join implementation. It gets a targeted test with a build side of many duplicates per key.

## 8.3 The eight kinds

Inner is the base. The other seven are the base plus one of two additions.

A match flag per build row, for the kinds that have to emit unmatched build rows at the end: right, full. This is a bitmap over the build side, set during the probe, and scanned after the probe finishes. It has to be atomic once the probe is parallel, and since document 06 already requires concurrent build, making it atomic now is consistent and costs a byte per row rather than a bit if that turns out to be faster, which it usually is because a bit needs a read-modify-write.

A first-match-only rule, for the kinds that emit each left row at most once: semi, anti, single. These do not need the build payload at all, only the presence of a key, which makes them much cheaper and which the table should exploit rather than treating them as an inner join with deduplication afterwards.

Left and full also need unmatched left rows padded with nulls, which is known per probe chunk and needs no extra state.

Single is the one that carries a semantic obligation beyond performance: `SELECT (SELECT x FROM t WHERE t.k = s.k)` must error when the subquery returns more than one row, so the single join has to detect a second match rather than silently taking the first. That is currently correct in the nested loop and it is exactly the kind of rule a rewrite drops.

Positional is not a hash join at all and stays as it is.

The null rule is the thing to be most careful about. A null key matches nothing in an equi-join, so nulls are excluded from the build table and excluded from the probe, and that is different from `IS NOT DISTINCT FROM`, which is what `key.rs` implements for grouping and which does match nulls to nulls. Two rules, one type, one place where the distinction lives, and the join says which one it is asking for. `NOT IN` with a null on either side is the classic wrong answer in every database that has ever been written, it unnests to an anti join with a special null rule, and it gets its own test suite checked against DuckDB.

## 8.4 Which side builds

The build side should be the smaller one, and getting that backwards on TPC-H Q9 is the difference between a fast query and a query that runs out of memory.

There is no cost model until document 11 and there are no table statistics until then either, so this layer uses what it can get and records that it is a placeholder. The catalog knows row counts for base tables. The scan knows block counts and can estimate after pruning. A filter's selectivity is a guess. That is enough to be right on the easy cases, which is a dimension table joined to a fact table, and that is most of TPC-H.

Two things make the placeholder safe rather than reckless. The choice is recorded in the plan and shown by `EXPLAIN`, so a bad choice is visible. And the operator can switch sides after the build has started if the build side turns out much larger than estimated, which is expensive but bounded, and which is the adaptivity hook that document 12 later generalizes. The second is optional at 2h and the first is not.

## 8.5 The Bloom filter, which is the largest win in this layer

After the build side is materialized, the set of build keys is known exactly. A Bloom filter over those keys, pushed down to the probe side's scan, lets the scan reject rows before they are read out of the file rather than after they have been probed.

On a star schema query, meaning a fact table joined to several filtered dimension tables, this is the difference between reading the whole fact table and reading the part of it that can possibly join. On TPC-H Q9 and Q21 it is worth a large factor. Every serious engine does this and DuckDB does it, so not doing it is not a neutral choice.

The mechanics: build the filter during the hash table build, which costs one extra hash per build row and the hash is already computed. Size it from the exact build cardinality, which is known. Push it into the probe pipeline's scan as a runtime filter, which requires that the scan can accept a filter after the plan is built, which is a small change to the scan operator and which has to be made at this layer. Evaluate it against the encoded data where possible, which document 05 section 5.8 already provides for dictionary columns and which turns the filter into a lookup per dictionary entry rather than per row.

The filter also composes with zone maps. The minimum and maximum of the build keys are known exactly, and a range predicate derived from them prunes whole blocks of the probe side before the Bloom filter is consulted. That is cheaper than the filter and it is often just as effective, particularly on clustered fact tables where the join key correlates with the physical order.

## 8.6 Predicate transfer

The Bloom filter in section 8.5 is per join and it flows one way. Predicate transfer, from Yu et al. at CIDR 2023, generalizes it to the whole join graph and runs it as a phase before execution.

The idea: build a Bloom filter for every join edge, propagate them through the join graph in a forward pass and then a backward pass, so that every table's scan is filtered by the transitive consequence of every predicate in the query, not only the one on the join it participates in directly. A predicate on a small dimension table reaches the fact table even when they are not directly joined. The reported results on TPC-H and on the join order benchmark are large, and the technique is a semijoin reduction done with modern data structures, which is an old idea whose costs finally came down.

Robust Predicate Transfer is the follow-up that addresses the case where the transfer costs more than it saves, which happens when the filters are not selective, and it makes the decision adaptive rather than always-on.

The parent spec has this as M4 and it is the right place for it, which is after this layer rather than in it. What this layer owes is the mechanism: a Bloom filter that can be built from a key set, pushed into a scan, evaluated on encoded data and combined with another filter. Once those exist, predicate transfer is a plan rewrite plus a scheduling change, and it is a document 11 and document 12 item.

Parachute, from 2025, is the precomputed variant, where the transfer structures are built ahead of time from the data rather than per query. It is noted as a possibility and not scheduled, because it changes what the storage format holds and that is a larger commitment than this directory is making.

## 8.7 The joins that are not hash joins

Non-equi joins have no key, so there is nothing to hash. The nested loop stays and is the fallback for them, and this is the second reason not to delete it. It gets one improvement, which is blocking both sides rather than one so that the inner loop works over a cache-resident block, and that is worth several times on the cases that reach it.

Inequality joins, meaning a join whose condition is a pair of range predicates, have a real algorithm. IEJoin sorts both sides and uses a permutation array and a bit array to find matches in close to linear time, DuckDB implements it, and it turns a class of query that is quadratic into one that is not. It needs the sort from document 09 and so it is scheduled after it.

Merge join, for inputs already sorted on the join key, is cheap once the sort exists and is the right choice when the sort is free because the data is already ordered that way. On a clustered fact table it often is.

`AsOf` join is a DuckDB feature, it appears in real workloads and in the corpus, and it is a sorted-input algorithm rather than a hash one. It is a compatibility obligation rather than a performance one, and it is scheduled with the other sorted joins.

## 8.8 Spilling, the seam again

A hash join whose build side does not fit is the canonical out-of-memory failure in an analytical database, and the standard answer is a grace hash join: partition both sides by hash, spill partitions that do not fit, and process them one at a time.

Document 06 section 6.9 already requires the table to be partitionable by the top hash bits without rehashing, which is exactly what this needs. What this layer adds is that the probe side must also be partitionable the same way, which means the probe pipeline can be told to write to partitions instead of to the next operator. That is a sink shape and it is the same one the aggregate has.

Nothing spills at this layer. The build side that does not fit fails with a clear error naming the operator and the size, which is better than the current behaviour and is not a solution.

## 8.9 The test gate

The nested loop is the oracle, and this is not a compromise, it is what its module doc says it was written for: eight join kinds each have their own rule about what happens to a row with no match, getting those right in a nested loop is a page of code that can be read against the standard, and the way to find out whether the hash join has them right is to run both and diff.

So the nested loop is kept, permanently, as the reference implementation and as the non-equi fallback. Every hash join is checked against it over generated inputs: random key distributions including all-duplicates and all-distinct, random null density on both sides, every join kind, empty build side, empty probe side, and build sides that are much larger and much smaller than the probe. Output row order is not compared because a hash join does not promise one, so the comparison is a multiset comparison, and that is stated in the test rather than being a silent sort.

The resumable probe from section 8.2 gets a directed test: a build side with a thousand rows sharing one key, probed by two rows with that key, which produces two thousand output rows from a two row probe chunk and exercises every boundary in the state machine.

The Bloom filter must never remove a matching row, which is a property test with the filter forced to a small size so that the false positive rate is high and any false negative is immediately visible.

`NOT IN` with nulls, `EXISTS`, `NOT EXISTS` and correlated scalar subqueries all unnest to joins and all have null rules, and they are checked against DuckDB through the corpus rather than against a reading of the standard.

## 8.10 The benchmark gate

TPC-H is the join benchmark and this is the layer it is for. At SF100, on `server1` because that is the machine the data fits on, all twenty-two queries.

The specific queries that measure this layer: Q9 and Q21 for large joins with large build sides, Q17 and Q18 for correlated subqueries that unnest to joins, Q3 and Q5 for star-shaped multi-way joins where the Bloom filter should show, Q13 for a left outer join with a group by on top, and Q16 for a `NOT IN`.

Microbenchmarks: build and probe throughput separately at build sizes spanning L2 to far past L3, match rates from 1 percent to 100 percent, and a duplicates-per-key sweep from one to a thousand, which is what the resumable probe costs. The Bloom filter's effect measured as bytes read on the probe side with it on and off, which is the number that makes its value obvious and which document 02 section 2.5 already collects.

The target at 2h is the honest one and it is not yet a win. rudb should be within a factor of two of DuckDB on TPC-H at SF100 single threaded, having been two to three orders of magnitude behind at 2a. The reason a win is not the target is that TPC-H is a join ordering benchmark as much as a join execution benchmark, and there is no optimizer until document 11, so several of the twenty-two queries will be running a plan that a cost model would not have chosen. Claiming a join execution win while running a bad plan would be measuring the wrong thing, and claiming a loss caused by the missing optimizer as a loss of the join operator would be reading it wrong in the other direction.

What is separately reported at 2h, and what does have to win, is the per-operator number: probe throughput in rows per second per core against DuckDB's, on the same build size and the same match rate, measured directly. That number has no plan in it.

## 8.11 Exit criterion for 2h

**Every equi-join runs on the hash table from document 06 with a row layout build side and a resumable probe, all eight join kinds agree with the nested loop oracle on generated inputs including nulls and duplicates, semi and anti skip the payload, a Bloom filter and a derived range predicate are pushed from the build side into the probe scan and evaluated on encoded data where possible, the non-equi fallback is blocked on both sides, TPC-H SF100 runs all twenty-two queries correctly on `server1` within a factor of two of DuckDB single threaded, and probe throughput per core beats DuckDB's at every build size measured.**

Named as deferred: predicate transfer across the join graph, which needs the optimizer and is document 11 and M4, IEJoin and merge join and `AsOf`, which need the sort in document 09, spilling, which has its seam here, and join reordering, which is the optimizer's job and whose absence is the stated reason the TPC-H target is a factor of two rather than a win.
