# What the writer already knew

## Why this document exists

Documents 23 through 27 worked one query in one quadrant and settled two things about it. Ten times faster is out of reach in the Parquet quadrant by arithmetic rather than by effort, because DuckDB's scan alone is four times the whole budget. Ten times less resources is reachable there by bounding what the grouping remembers, which document 26 designed, document 27 built in a harness, and the measurement put at 89.3 MiB against DuckDB's 4,869.

Every one of those five documents was about a file rudb did not write. The native quadrant appeared in them twice and both times in a sentence excusing itself: document 25 did not measure its floor because the disk would not hold DuckDB's copy, and document 26 noted that the global dictionary "already answers this query in 825 MiB" and moved on.

This document asks what those five did not, which is what the writer of rudb's own format already knows. The answer is that it has held an exact count of every distinct value of every string column since the frequency section landed, and that the reader would not use it for the columns where using it is worth anything.

## What the writer stores

A varchar column of this format is written against one global dictionary. A code is handed out the first time a value is seen and nothing ever removes one, and `GlobalDictionary::observe` adds one to that code's counter for every non-null row that uses it. So by the last stripe the writer is holding an exact count for every distinct value in the column, not a sample of one and not an estimate.

`code_frequency` then sorts those counts descending, keeps the leading `FREQUENCY_ENTRIES` of them, which is 512, and sets `omitted_max` to the count of the 513th. That last number deserves attention. For the numeric columns `omitted_max` is what a bounded Misra-Gries pass could not rule out, which is a bound in the ordinary sense of something the pass had to be careful about. For a string column it is the exact count of a real value, and it is the tightest number that could possibly go there.

So every string column of every native file carries a certified top 512 with an exact cutoff, written once at checkpoint, costing 512 entries of directory. None of that is new and none of it was built for this. It has been in the format since the frequency section, and section 3.8's budget has been paying for it all along.

## What the reader would not do with it

`Reader::top_frequencies` proves a prefix: it takes the `top`th entry's count, compares it against `omitted_max`, and answers when the boundary wins. That is exactly the certification document 26 spent a page proving, written down and shipped.

`Reader::exact_frequencies` is the other one, and it answers only when `omitted_max` is zero, which means the synopsis never had to drop a value, which means the column has at most 512 distinct values.

The grouped path in `rudb-exec` used both, and the branch it took had nothing to do with which one the query could use:

```
Some(certain) if certain.column == column => certain.kept(),
Some(_) => return Ok(None),
None => match top {
    Some(top) => table.rows().top_frequencies(column, top)?,
    None => table.rows().exact_frequencies(column)?,
},
```

A filter over the column being grouped took `certain.kept()`, and `CertainFilter` was built from `exact_frequencies`. So the moment a query had a filter, the bound it also had stopped mattering and the complete list was demanded instead.

A column with at most 512 distinct values is a column where building the groups was never expensive. A column with nineteen million is where the shortcut would be worth taking and is precisely where it could not fire. The set of columns the filtered path served and the set it would have been worth serving were disjoint, and had been since it was written.

## The proof, which is document 26's

The same two lines, against a different structure.

Take any value the synopsis left out. It holds at most `omitted_max` rows. The filter names the column being grouped, so it decides whole values: an entry it keeps keeps all of that value's rows, and a value it removes is gone entirely rather than made smaller. Filtering cannot raise anyone's count. So an omitted value that survives the filter still holds at most `omitted_max` rows, and if the `top`th surviving entry holds more than that, no omitted value can reach the answer and the leading `top` survivors are exact.

Two things are worth saying about how this compares with document 26's version. The bound here is tighter, because it is the exact count of one real value rather than a sum over a hash bucket that several values share. And it costs nothing at query time, because the counting already happened at checkpoint, where document 26's first pass is a whole extra read of the column.

What the two share is the discipline. The proof is checked and not assumed, and a query where it does not go through reads the rows, which is what it would have done anyway.

## The margin, measured

The proof is only worth having if real columns clear it, and this series has been wrong before about what a real column looks like. So the distribution was measured on the hundred million row file rather than reasoned about:

| | boundary count | `omitted_max` | margin |
| --- | ---: | ---: | ---: |
| `Referer`, tenth of 19,720,796 groups | 247,459 | 6,445 | 38.4x |
| `SearchPhrase`, ClickBench query 12 | 12,317 | 791 | 15.6x |

The `Referer` boundary is the 247,459 documents 23, 26 and 27 all name, arrived at a fourth time and by a different route. The `SearchPhrase` numbers are the tenth and the 513th of that column once the empty string, which holds 86,825,105 rows on its own, is taken out by the query's own filter.

Both clear the bound by more than an order of magnitude. Neither is close to the tie that would send it back to the rows.

## What this changes

ClickBench query 12 is `SELECT SearchPhrase, COUNT(*) AS c FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10`. On a native table it built a hash table over every distinct value of the column in order to print ten rows. It now reads 512 entries of directory, drops the ones the filter removes, checks one comparison, and returns. No rows are read at all.

That is not a faster scan. It is the absence of one. The work left is proportional to the synopsis, which is a constant the file chose, rather than to the rows or to the column's cardinality, neither of which the query has any say over.

## The measurement

A native table of `SearchPhrase` alone, 99,997,497 rows loaded from `hits.parquet`, with query 12 run against it. Every figure is a warm run and the two engines returned the same ten rows down to the last count:

| | wall | processor | peak | file |
| --- | ---: | ---: | ---: | ---: |
| rudb, synopsis | 0.54 s | 0.28 s | 48.2 MiB | 283 MiB |
| rudb, rows | 5.09 s | 4.54 s | 247 MiB | 283 MiB |
| DuckDB | 12.33 s | 19.21 s | 1,434 MiB | 479 MiB |

Against DuckDB that is 68.6 times less processor time and 29.8 times less memory, on a file 1.7 times smaller. The host was carrying a load average of 22 from another tenant while this ran, so the wall column is noisy and the processor column is the one to read.

The middle row is the same file and the same query with a second sort key added, which disables the push down and sends the query back to the rows. It is there because it is the only honest control: it isolates what the synopsis saved from everything else the native format does, and what it saved is sixteen times the processor and five times the memory.

The two engines agreeing on all ten rows is worth more than the ratios. The proof says the leading ten survivors are exact when the tenth beats `omitted_max`, the tenth is 12,317 and `omitted_max` is 791, and a full pass over a hundred million rows returns the same ten counts.

## The error this series made, which is the eleventh

Document 26's own summary of its position was that its design "does not touch the native quadrant". That sentence was written as a limitation. It was actually a description of where the design already lived.

Five documents designed a bounded certified top count, proved its certification, priced it, built it in a harness and measured it, and the format had been carrying one the whole time. Not something similar. The same structure, with the same proof, with a tighter bound, already written to disk and already read by `top_frequencies` for the unfiltered case.

Name it the eleventh error class, after the ten documents 20 through 24 collected: **the inventory error, which is pricing a structure the system already has.** Its signature is a design document that opens by describing what the engine cannot do without checking what the engine stores.

What kept it hidden is worth writing down too, because it will recur. The series measured the Parquet quadrant because that is where the gap to DuckDB was largest, and it read that gap as a missing capability. From the outside a reader that refuses a fast path and a system that has no fast path produce the same measurement. Nothing in a timing run distinguishes them. The only thing that would have was reading what the writer wrote, which is cheaper than every measurement in documents 25 through 27 put together.

## Where that leaves the target

Unchanged in the Parquet quadrant, where document 25's arithmetic still stands and nothing here applies. A Parquet file carries no such synopsis, its dictionary pages are per column chunk rather than per table, and document 24 measured this column arriving seven eighths as plain bytes anyway.

In the native quadrant, on the shape this serves, the query no longer reads rows, and the section below measures that at 68.6 times the processor and 29.8 times the memory against DuckDB on the same data. Both axes clear ten times, by an argument about work that does not happen rather than work done faster, and it is worth being exact about how narrow "those queries" is. One query of forty three is not the target, which asks for both axes in all four quadrants across the suite. What it does settle is that the target is not arithmetically closed in the native quadrant the way document 25 closed the time axis in the Parquet one, because a synopsis the writer already pays for can put a whole query's work on the other side of the ledger.

## What this document does not claim

It does not claim the measurement covers the table. The measured column is `SearchPhrase` on its own, because that is what the host's three remaining gigabytes would hold. A full width hits table would not change the synopsis path, which touches one column's directory and nothing else, but it would change both engines' load time and DuckDB's file, and neither of those was measured.

It does not claim the shape is wide. What the path serves is one grouping column that is also the filtered column, one `COUNT(*)`, one ordering key on that count, and one equality or inequality against a constant. That is query 12. It is not query 23 or 24, whose filters are five predicates over other columns. It is not queries 32 and 33, whose keys are composite and close to unique, which document 26 named as the hard shape and which nothing has measured yet. It is not the four queries ordered by a `COUNT(DISTINCT)`, which is not a count in the sense this proof needs.

It does not claim 512 is the right number. It is what the format chose before any of this was a use for it, and a column whose leading counts sit close together will fail the comparison at any budget. The failure is safe and it is not free: the query pays a walk over a few hundred entries and then reads the rows.

It does not claim the numeric columns get the same thing. Their synopsis really is a bounded pass with a bound in the ordinary sense, so `omitted_max` there is looser and the margin will be worse. `UserID` and `ClientIP` are the columns that matters for and neither has been measured.
