# 2. The catalogue

Every statistic and every piece of metadata rudb keeps, what class of truth it is, what it costs, and the column that governs whether it exists at all, the decision it is there to make.

## 2.1 The three classes

**Exact.** The number is the number. A count, a null count, a dictionary size, a minimum, a link's unmatched count. An operator may *answer* from it.

**Certified.** The number is exact over part of the domain and carries a proven bound over the rest. The frequency synopsis of `../storage-v3/11-certified-frequency-synopses.md` is the model: 512 exact leading counts plus `omitted_max`. An operator may answer from it **only when it can discharge the proof obligation**, and must fall back otherwise.

**Estimated.** A number with no proof. Usable for a plan decision that has a cheap fallback; never usable to produce an answer.

Every consumer interface returns the class along with the value, and `EXPLAIN` prints it. A planner that cannot tell an exact row count from a default guess produces bad plans that nobody can attribute, and the attribution is worth more than the accuracy.

## 2.2 The granularity ladder

The file is already parts of 1000 rows inside stripes of 64 parts. Statistics attach at four levels and the level is a cost decision, not a taste one:

- **Per part**. only what is already there: the zone-map minimum and maximum per column, built by `Zone::of` on write. A per-part structure costs 600,000 copies of itself on a 600-million-row `lineitem`, so nothing new goes here.
- **Per stripe**. 64,000 rows. Null counts already live here. Sketches and quantiles go here, because 10,000 stripes at SF100 is a tractable number of small objects and because a stripe is the unit a scan already skips at.
- **Per column, table-wide**. merged from the stripe level at checkpoint. The dictionary, the frequency synopsis, the merged sketch, the sortedness facts.
- **Per relationship**. document 07.

## 2.3 The catalogue

| statistic | level | class | size | consumer decision |
| --- | --- | --- | --- | --- |
| row count | table | exact | 8 B | everything; base cardinality |
| rows per part | part | exact | already stored | scheduling, parallel chunking |
| min / max | part | exact | already stored | zone skip, range selectivity, runtime min/max filter |
| null count | stripe | exact | 8 B | `COUNT(column)` answered without a scan; null-aware join and anti-join rules |
| distinct count | stripe + table | exact below `k`, else estimated | KMV of `k` u64 | `COUNT(DISTINCT)`, group-by hash presizing, join estimate, dictionary decisions |
| the value set | stripe + table | exact below `k` | the same KMV | exact `IN`-list runtime filter, group-by elimination, partition pruning |
| key-value overlap | column pair | estimated (Jaccard) | none new | join cardinality, which side builds |
| column dependence | column pair | estimated | pair sketch | multi-predicate selectivity without multiplying independent guesses |
| frequency leaders | table | certified | ≤512 entries + `omitted_max` | top-k group-by answered from metadata; skew detection; heavy-hitter join handling |
| quantile boundaries | stripe + table | certified (ε-bounded) | ~64 values | range predicate selectivity; sort and top-n partition boundaries; spill sizing |
| sortedness | column | exact | 2 B + run count | sort elimination, merge paths, link-join gather locality, zone-map worth |
| distinctness flag | column | exact | 1 bit | distinct elimination, group-by elimination, join uniqueness, outer-join simplification |
| value width | column | exact (sum, max) | 16 B | memory reservation, view vs inline, string kernel choice |
| dictionary | column | exact | already stored | exact `COUNT(DISTINCT)` on strings, code-space predicates, global codes |
| sample | table | exact rows, sampled | fixed reservoir | arbitrary conjunction selectivity, including correlations no sketch expresses |
| degree facts | relationship | exact | ~64 B | document 07 |
| validity certificate | relationship | exact | 1 bit + witness | document 07 |
| observed corrections | column, predicate class | observed | small, bounded | document 06 |

## 2.4 The rule that keeps maintenance cheap: mergeability

**Every persisted statistic is either mergeable or per-part re-derivable.** A statistic that requires a full-table pass to update is a statistic that becomes stale on the first append and stays stale, which is how statistics subsystems in other engines end up requiring a maintenance job nobody runs.

This is a real constraint and it decides several entries above:

- Counts and null counts add.
- Minimums and maximums merge.
- KMV sketches **union exactly**, `Sketch::union` is already there, which means a table-level distinct count is the union of stripe sketches, and appending a stripe means unioning one more, not recomputing anything.
- Misra-Gries summaries merge with a known error bound (*Mergeable Summaries*, Agarwal, Cormode, Huang, Phillips, Wei and Yi, PODS 2012), which is what allows the frequency synopsis to survive an append with its certificate intact rather than being invalidated by it.
- Quantile summaries are stored in a mergeable form for the same reason, which is the argument for a sketch with a bound rather than a plain equi-depth histogram, whose boundaries do not merge.
- Sortedness merges: a column is sorted table-wide when every stripe is sorted and the stripe boundaries do not overlap, and both facts are already there.

The one entry that does not merge is the **sample**, and it does not need to: reservoir sampling is defined over a stream and an append is more stream.

## 2.5 What the writer already does, and what is added

The honest accounting of build cost, because document 09 has to measure it and because a load that gets 30 percent slower to make queries 5 percent faster is a bad trade for a database people load every day.

**Free, already computed.** Minimums, maximums, null counts, dictionary sizes, string extremes. The zone-map build was measured at 260 ms and then 60 ms after optimization, over a million ClickBench rows.

**Nearly free, one hash per value, reusing a hash that already exists where one does.** The KMV sketch. The encoder already hashes values for dictionary construction and for the multi-column chooser; where it does not, `hash64` over a value that is already in a register is cheap next to writing it.

**Bounded second pass, only where a certificate requires one.** The frequency synopsis already works this way: a Misra-Gries pass over encoded pages, and an exact recount only for surviving candidates and only when the certificate looks viable. Quantiles are one streaming pass with no second.

**Explicitly budgeted.** Document 09 sets the target: statistics construction adds no more than 10 percent to write time and no more than 2 percent to file size. Those are gates, not hopes, and the response to breaching one is to drop the least-consulted statistic rather than to accept the cost.

## 2.6 Where the in-memory path stands, and why that is the gap

`Rows` in `crates/rudb-catalog` already exposes `top_frequencies`, `distinct_values`, `null_count`, `text_extremes` and `frequency_occurrences`. Every one of them answers `None` for a `Memory` table and a real number only for a `Native` one.

That asymmetry is the largest single gap in this directory, and it is not a small one: a table that has just been `INSERT`ed into, or a `CREATE TABLE AS`, or the intermediate of a CTE, is a memory table, has no statistics at all, and therefore gets the default guesses on every decision. Document 04 section 4.6 is the fix, and the shape of it is that a memory table computes the *cheap* half of this catalogue as it is built, counts, min/max, null counts and a sketch, because it is holding the values anyway.

Two present limitations of the exact path are worth recording because they are the first two things to fix:

**`distinct_values` returns `None` when the column has any null,** because a null row is written using the empty string's code and nothing persisted tells the two cases apart. That is a one-flag fix in the dictionary and it converts a whole class of column from estimated to exact.

**`distinct_values` only answers for string columns,** because it is really "dictionary size". Numeric columns have no exact distinct count today. The KMV sketch closes it as certified-or-estimated, and `is_exact` closes it as *exact* for low-cardinality numeric columns, which is most dimension keys, most enumerations, and most of what people group by.

## 2.7 What is deliberately not in the catalogue

**Multidimensional histograms.** `../planner/06-cardinality-and-cost.md` section 06.6 rules them out; the dependence term covers the case they exist for.

**Per-part anything new.** Section 2.2's arithmetic.

**Full histograms of high-cardinality columns.** `../storage-v3/11` measured it: at one million ClickBench rows `UserID` has 898,913 distinct values, and the complete histogram is larger than the column. The certified leaders plus a bound is the answer and it is already the answer.

**A statistic for a decision nobody makes.** Restated from the README because it is the rule that keeps this list from growing every time somebody has an idea: the catalogue's last column is mandatory, and an entry whose consumer is removed is removed with it.
