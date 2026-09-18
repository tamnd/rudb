# 11. Open questions

Seven things this design does not know. Each one says what would settle it and what the fallback is if it settles the wrong way.

## 11.1 Whether the exact bitmap actually beats the Bloom filter

The argument in document 05 section 5.4 is about information content: a bitmap over a dense id space is exact and a Bloom filter is not, and exactness is what turns a heuristic reduction into a full one. The argument ignores the memory hierarchy. An 18 MB bitmap probed six hundred million times in an order that is not sequential is six hundred million last-level cache misses, and a 2 MB Bloom filter probed the same way is not.

**Settled by** document 09 section 9.4, with cache miss counters and not only wall time. **Fallback** is the hybrid: Bloom for the first pass and exact for the last, which keeps the full-reduction property where the set is small and keeps the cache behaviour where it is large. That fallback is a worse design and it is still ahead of what is published.

## 11.2 Whether TPC-H is the wrong workload to prove this on

GRainDB's authors said TPC-H lacks selective many-to-many joins and that no large win should be expected there. They were describing a weaker mechanism, but they were describing the same schema. It is entirely possible that this layer is worth 1.3x on TPC-H and 5x on JOB and 20x on LDBC, and that the goal document's ordering of suites is therefore the wrong ordering for this particular piece of work.

**Settled by** document 09 sections 9.6 and 9.7, the isolation measurement and then the graph workload. **Fallback** is to keep the layer and move its justification: it would still be the right structure for JOB and CEB, which `../02-the-goal.md` puts at 5x, and the TPC-H 10x would then have to come from the scan and the aggregate, which is where `../perf/` already says most of ClickBench's time goes.

## 11.3 Where the update rate breaks the format

Document 07 treats a link as immutable per stripe and rebuilds at checkpoint, which is fine for a load-then-query workload and is not fine for a workload with a steady insert rate into the middle of a parent's children. The mutable-CSR literature, Bw-Graph's paged CSR, BACH's LSM bridge, VCSR, exists because there is a rate at which this matters.

**Settled by** a measurement nobody has designed yet: an insert rate sweep against link staleness and query time. **Fallback** is the paged CSR, which is a real project and should not be started without the sweep.

## 11.4 Whether branched factorization is needed

Document 08 section 8.3 declines FFX's selector and cascade update on the grounds that TPC-H's expansions are chains rather than branched trees. A query joining one fact table to two independent dimension tables and expanding both produces a branch, and TPC-DS has more of those than TPC-H does.

**Settled by** running TPC-DS with the expanded body and counting how often a flatten was forced by a branch. **Fallback** is the cascade update, which is the part of FFX this design skipped and which would then need to be implemented as specified rather than reinvented.

## 11.5 The minimal perfect hash

The sorted key map costs a binary search per probe. A minimal perfect hash costs one probe and a much longer build. There is no measurement yet saying the search is where the time goes, and building an MPH before that measurement exists would be optimizing a guess.

**Settled by** the profile of a link build on a string-keyed relationship, which does not exist in TPC-H and does in JOB. **Fallback** is that the sorted form was always adequate, which is the likely outcome for integer keys since those are mostly identity anyway.

## 11.6 Precomputed transitive links

Parachute precomputes join-induced fingerprint columns so that a filter on a primary-key table prunes a table two joins away with no passes at all, at fifteen percent space for 1.54x on JOB. Document 05 section 5.5 composes links at query time instead, paying passes to save space. Which is right depends on how often the same chain recurs, which is a property of a workload and not of a design.

**Settled by** counting chain recurrence in a real query log, which this project does not have, and by comparing the pass cost at SF100 against the space. **Fallback** is to build it for chains the query log names, which is the same inference machinery document 02 section 2.5 already specifies for relationships, applied one level up.

## 11.7 What happens to all of this under the DuckDB format

`../12-duckdb-compat.md` requires that rudb read and write DuckDB v2.0 files losslessly. A DuckDB file has no place to put a section table, so a table attached from a DuckDB file gets no links and no reduction, and every query against it takes the fallback path.

That is correct and it is also the configuration a lot of users will be in, which means the sections-off numbers of document 09 section 9.8 are not a control, they are a product. The open question is whether links should be buildable *in memory only* for an attached DuckDB table, held for the session, paid for once per connection. For a long-running analytical session over a 200 GB file that is plainly worth it; for a one-shot query it is plainly not; and the thing in between is where the decision is.

**Settled by** the build-cost measurement of document 03 section 3.8 against the session length distribution, which is a product question as much as an engineering one. **Fallback** is `CHECKPOINT INTO`, which converts the file and gives the user everything, and which is already the documented path.
