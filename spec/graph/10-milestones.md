# 10. Milestones

Eight, each with one exit measurement, each shippable on its own. The ordering is by dependency and then by measured value, and the first two deliver nothing to a user, which is stated rather than disguised.

The dependency outside this directory is `../engine/08-join.md`: the hash join is a prerequisite for G3 and not for G1 or G2. That ordering is deliberate. The hash join is what makes TPC-H run at all, this layer is what makes it fast, and a layer that is fast against a suite that does not run is not measurable.

## G0 (prerequisite, not owned here): the hash join

Issue #351, F6. Until it lands, `rudb-bench` refuses TPC-H and there is no baseline to improve on. `../bench/tpc-h/07-the-harness.md` specifies how the harness stops refusing before the engine is fast, which is what makes G1's exit measurable.

**Exit:** TPC-H SF1 runs all twenty two queries to correct answers, at any speed.

## G1: row ids and key maps

Document 02 and document 03 section 3.3. The `rid` type, the prefix sum, the three key map forms, the section table in the format, and `rudb_links()`. No join changes anything yet.

**Exit:** a native TPC-H SF10 file with key maps on all eight tables, under the budget, with the build time and bytes reported per table, and a version 11 reader that opens a version 10 file unchanged.

## G2: the forward link, and the `rid` annotation

Document 03 section 3.4, including the monotone form, and the plan annotation of document 05 section 5.1. Still nothing uses a link to answer a query.

**Exit:** the SF100 size claim, C1 in document 09 section 9.1, measured on a real file. And the annotation pass with a test per plan node asserting which of its outputs carry a `rid`, since that pass is the correctness foundation for everything after.

## G3: the link join

Document 05 section 5.2, the `Gathered` body of document 08 section 8.2, and the planner rule of document 06 section 6.4. Inner, left, semi and anti only.

**Exit:** claim C3, per query, with the hash join forced as the control, and the sections-off differential of document 09 section 9.2 green across the whole suite.

## G4: single-edge reduction

The `Rids` type of document 04 section 4.3, the push-through-a-relationship primitive, the scan's ability to take a bitmap, and the link column's zone-map part skip. One edge at a time, always applied where the parent has a predicate.

**Exit:** the rows removed per scan on TPC-H Q3, Q5, Q10 and Q14, against the Bloom filter on the same plans. This is the first half of claim C2 and it is the milestone where the design stops being speculative.

## G5: full reduction

Document 06 section 6.5: the join tree by LargestRoot, the forward and backward passes, the runtime gate that stops a chain when it stops paying, and the early-stop adaptivity of document 06 section 6.3.

**Exit:** claim C2 in full, and the isolation measurement of document 09 section 9.6, single-edge filters against full reduction, which is the one that says whether this directory found something GRainDB did not.

## G6: factorization

The `Expanded` body of document 08 section 8.3, the aggregate and predicate rules, and the direct-addressed group-by on a `rid` key from document 05 section 5.6.

**Exit:** claim C4 on Q9, Q10, Q18 and Q21, and the peak RSS of those four queries against the flattened path, because the entire argument for factorization is memory before it is time.

## G7: the backward adjacency and LDBC SNB

Document 03 section 3.5, the right and full outer link joins, and the addition of LDBC SNB to `rudb-bench` per document 09 section 9.7.

**Exit:** SNB interactive short and complex reads running, with a published comparison against DuckDB and against whatever Kùzu fork is current, and an honest statement of where a purpose-built graph engine is still ahead.

## G8: multiway intersect

Document 05 section 5.8, on JOB and CEB, as an operator the ordinary planner emits rather than a mode.

**Exit:** the JOB queries whose plans contain a cycle, with the maximum per-query ratio reported prominently per `../15-rudb-bench.md`'s rule about means hiding catastrophes.

## The order, and what could reorder it

G4 before G5 and G3 before both, because a reduction is measured by the join it feeds and a join is measured against a control. G6 after G5 because factorization's win is largest on the rows that survive reduction, and measuring it before would overstate it. G7 and G8 last because they serve workloads the goal document names second and third.

Two things would reorder this. If G4's measurement shows the exact bitmap losing to the Bloom filter on cache behaviour, G5 becomes the hybrid of document 09 section 9.4 and moves after G6. And if the isolation measurement at G5 shows GRainDB's prediction was right for TPC-H, the whole directory's centre of gravity moves to G7 and G8, where the workload actually has the shape this design is for. Both of those are outcomes to plan for rather than to fear, and writing them down now is what stops the project from discovering them and then arguing about them.

## What is explicitly not scheduled

A graph query language. Transitive link materialization, including Parachute's precomputed columns. Mutable CSR structures and the LSM arrangements of document 01 section 1.7. A minimal perfect hash key map. Learned gating of the reduction decision. Every one of those has a paragraph in document 11 saying what would put it on this list.
