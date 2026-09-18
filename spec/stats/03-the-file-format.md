# 3. The file format

## 3.1 The invariant, which is the same one

**Delete every statistics section from a rudb file and no query changes its answer.** Some get slower. None gets wrong.

This is `../graph/03-the-file-format.md` section 3.1 applied to a second kind of payload, and it is the property that makes the whole thing testable: the differential run of document 09 section 9.3 turns the sections off and compares answers, and a disagreement is a bug found by a machine rather than by a user.

It has teeth precisely because these sections *do* answer queries. The frequency synopsis path already returns a top-k result out of metadata without touching the column. The invariant is what forces every such path to carry a proof obligation and a fallback, which is the discipline `../storage-v3/11-certified-frequency-synopses.md` established and this document generalises.

## 3.2 They are sections, not a new mechanism

`../graph/03-the-file-format.md` section 3.2 adds a section table to the version 11 directory: fifty-six-byte entries with `kind`, `id`, `generation`, extents of at most sixty-four megabytes each independently checksummed, an unknown `kind` skipped by a reader that does not know it, and everything committed by the existing two-generation header swap.

Statistics use it unchanged. That is the entire format design and it is deliberately boring: no new page discipline, no new recovery path, no second file, and no version bump beyond the one `../graph/` already takes.

Six kinds:

| kind | scope | holds |
| --- | --- | --- |
| `RUDBCS1` | one per column | the column summary of section 3.3 |
| `RUDBSK1` | one per column | the sketches of section 3.4 |
| `RUDBQT1` | one per column | the quantile summary of section 3.5 |
| `RUDBSM1` | one per table | the sample of section 3.6 |
| `RUDBDP1` | one per column pair | the dependence sketch of section 3.7 |
| `RUDBFB1` | one per table | the observation log of document 06 |

The existing `RUDBFQ2` frequency synopsis keeps its directory extension and gains a section entry pointing at the same payload, so that version 11 has one enumeration of what a file knows about itself. Readers keep accepting `RUDBFQ1` and `RUDBFQ2` in the old position, because files exist that have them there and `../storage-v3/11`'s reader rules already handle both.

## 3.3 `RUDBCS1`, the column summary

One per column, small, always present, and the thing a planner reads first. Fixed layout, a few hundred bytes, no extents beyond the header:

- row count, null count, and both exact
- minimum and maximum, merged from the part zone maps, with a flag saying whether they are exact values or bounds
- distinct count, with its class: exact when the KMV sketch never overflowed or when the column is a dictionary with no nulls, certified when it carries a bound, estimated otherwise
- the distinctness flag: whether every non-null value is unique, which is exact and which is what makes a column a key candidate
- sortedness: ascending, descending, or neither; the number of ascending runs; and whether stripe ranges overlap
- value width: total bytes and maximum bytes, exact
- the generation this summary describes, and the generation of the newest data it has seen

That last pair is what makes staleness visible rather than assumed, and document 08 is about what a reader does when they differ.

The distinct count's class is stored rather than inferred because the difference matters at the consuming end. An exact distinct count lets `COUNT(DISTINCT c)` be answered from metadata; an estimate does not, and an engine that confuses the two returns a wrong answer very fast, which is the worst available outcome.

## 3.4 `RUDBSK1`, the sketches

A KMV sketch per stripe and one merged sketch for the column. `k` is a setting with a default of 4096, which is a few tens of kilobytes per column table-wide and which puts `is_exact`, the "this is not a sketch, it is the whole set" case, over the range that covers most dimension keys, most enumerations and most grouping columns anybody writes.

Per-stripe sketches are kept as well as the merged one for three reasons: they union to the merged one so nothing is duplicated conceptually, they let an append add a stripe without recomputation, and they let the planner ask a question about a *range* of stripes, which is what a query with a selective range predicate on a clustered column actually wants.

The header records `k`, the hash function's identity and the exactness flag. The hash identity is stored because a sketch built by one hash and merged with a sketch built by another is silently wrong, and this is a file format that will outlive a hash choice.

## 3.5 `RUDBQT1`, the quantiles

A mergeable quantile summary per stripe and one merged per column, with a stated ε. Default sixty-four boundaries, which answers a range predicate's selectivity to within about 1.5 percent and costs under a kilobyte.

Stored mergeable rather than as a plain equi-depth histogram for the reason in document 02 section 2.4: equi-depth boundaries do not merge, and a statistic that cannot survive an append is a statistic that is wrong by the second week.

The ε is part of the payload, which makes this a certified statistic rather than an estimated one, a selectivity that comes back as "between 0.11 and 0.14" is a different and better input to a plan decision than one that comes back as "0.125", particularly for the decisions in document 05 whose cost is asymmetric.

## 3.6 `RUDBSM1`, the sample

A fixed-size reservoir per table, default 4096 rows, stored as an ordinary set of rudb column pages so that evaluating a predicate against it is the ordinary expression evaluator over an ordinary chunk rather than a special path.

This is `../planner/06-cardinality-and-cost.md` section 06.3's second mechanism for multi-predicate selectivity, and the reason it earns its bytes is that it catches correlations **no sketch expresses**, an arbitrary conjunction over three columns, a `LIKE` against a string, a date range crossed with a category. The known weakness is sparsity after joins have cut the cardinality down, which is why it is used for the single-table conjunction and never for the join case.

The sample is a place where a rule has to be stated rather than assumed: **the sample is data.** It is subject to the same access control, the same encryption at rest and the same redaction as the column it was drawn from, and a tool that dumps a file's metadata does not print sample rows by default. A statistics structure that quietly exfiltrates values is a security bug with a performance justification.

## 3.7 `RUDBDP1`, the dependence pairs

A pair sketch per column pair, holding hashes of the concatenated pair per `pair_hash`, which is what `dependence` consumes. Written only for pairs that earn it, and the selection rule is the one genuinely awkward decision in this document, because the number of pairs is quadratic and the number that matter is small.

Three sources of pairs, in order:

1. Pairs the multi-column encoding chooser already evaluated. Free; the sketch exists already and is discarded today.
2. Pairs named by a declared relationship or a declared key, which are the pairs the schema says travel together.
3. Pairs the observation log of document 06 has seen in a conjunction more than a threshold number of times. This is the one that makes the set workload-shaped, and it is also the one that makes the file's contents depend on history, so document 06 section 6.6 states the rule: it may change what is *stored*, never what a given stored statistic *says*.

A hard cap of thirty-two pairs per table, because the value falls off fast and the budget does not.

## 3.8 The budget

Two percent of the file's column bytes for everything in this document, measured and enforced the way `../graph/03-the-file-format.md` section 3.7's ten percent is.

The arithmetic that says it fits, for TPC-H `lineitem` at SF100, sixteen columns, six hundred million rows, about ten thousand stripes:

- column summaries: sixteen, a few hundred bytes each. Negligible.
- sketches at k=4096: the merged sketch is 32 KB per column; the per-stripe sketches are the real cost and are kept at a smaller `k` of 256, which is 2 KB per stripe per column, so about 320 MB across sixteen columns. **That is too much**, and the resolution is the rule below rather than a smaller number pulled out of the air.
- quantiles: 64 boundaries per stripe per column, under 1 KB, about 160 MB across sixteen columns. Same problem.
- sample: 4096 rows of sixteen columns. Under a megabyte.

**The rule that follows: per-stripe statistics are written only for columns that are read.** A column with no predicate ever pushed to it needs a summary and a merged sketch and nothing per stripe. The writer does not know which those are, so the default is per-stripe sketches for the columns the encoding chooser already sketched plus any column in a declared relationship or key, and everything else gets the table-level summary only. Document 06's observation log is what promotes a column into the per-stripe set on the next checkpoint, and document 08 says when that rewrite happens.

That brings `lineitem` at SF100 to well under the two percent of a file whose column bytes are in the tens of gigabytes, and it is an honest example of the budget forcing a design decision rather than being asserted after one.

## 3.9 Ordering and commit

Sections are written after the columns and before the directory, and the directory is committed by the header swap that already exists. A crash leaves unreferenced bytes, which is what a crash already leaves.

Statistics sections are written **last**, after the graph sections, for one reason: the degree facts of document 07 are computed while the link is built, so the link has to exist first, and a writer that interleaved them would compute some of them twice.
