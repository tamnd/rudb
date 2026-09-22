# Taking the inventory

## Why this document exists

Document 28 named the eleventh error class, the inventory error, which is pricing a structure the system already has. It then committed a smaller version of the same error in its own closing section. It wrote "that is query 12" and stopped, having checked which query needed the code change it had just made and not which queries the reader served before that change.

Four more did, and none of them needed anything. This document walks the whole suite against the shape the reader accepts, measures every query that fits, measures one that does not so the refusal is on the record too, and classifies what blocks the rest. The classification is the part that matters, because it is the first statement in this series of what would have to be built rather than what would be nice to have.

## The shape, stated as a predicate

The reader answers a grouped count from the synopsis when all of the following hold. They are read off `native_frequencies` and its callees rather than restated from memory:

- Every grouping expression resolves to the same column, to a constant, or to that column minus a constant. `frequency_column` folds all three into one binding, so `GROUP BY 1, URL` and `GROUP BY ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3` are single column groupings that happen to be written with more than one key.
- There is exactly one aggregate and it is a plain `COUNT(*)`, not distinct and not filtered.
- There is one ordering key and it is that count, descending.
- Any filter under the grouping names the grouped column and compares it against a constant.
- With a `LIMIT` the synopsis prefix suffices, because the boundary can be proved against `omitted_max`. Without one the synopsis must be complete.

## What the suite looks like from here

Twenty nine of the forty three queries group. Fourteen do not and are out of scope for this document. Of the twenty nine, six fit the predicate and twenty three do not:

| | grouping | what it needed |
| --- | --- | --- |
| q8 | `AdvEngineID`, filtered on itself, no limit | complete synopsis, which a twenty value column has |
| q13 | `SearchPhrase`, filtered on itself, limit 10 | document 28's change |
| q16 | `UserID`, limit 10 | nothing |
| q34 | `URL`, limit 10 | nothing |
| q35 | `1, URL`, limit 10 | nothing, the constant key was already folded |
| q36 | `ClientIP` and three differences of it, limit 10 | nothing, the differences were already folded |

So one of the six needed the work document 28 did, one was the low cardinality case that path always served, and four had been answerable from the directory since the frequency section landed without anyone checking.

## The measurement

One table per column group, loaded from `hits.parquet` at 99,997,497 rows, warm runs, both engines on their own native format. Processor is user plus system, because the synopsis path is single threaded and DuckDB's is not, and wall clock on a shared host measures the other tenant:

| | rudb processor | DuckDB processor | ratio | rudb peak | DuckDB peak | ratio |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| q13 `SearchPhrase` | 0.28 s | 19.21 s | 68.6x | 48.2 MiB | 1,434 MiB | 29.8x |
| q16 `UserID` | 0.03 s | 17.24 s | 575x | 10.3 MiB | 856 MiB | 83.5x |
| q34 `URL` | 0.67 s | 121.65 s | 182x | 119.5 MiB | 8,558 MiB | 71.6x |
| q35 `1, URL` | 0.41 s | 102.90 s | 251x | 119.8 MiB | 8,816 MiB | 73.6x |
| q36 `ClientIP` | 0.04 s | 13.87 s | 347x | 10.4 MiB | 753 MiB | 72.6x |
| q8 `AdvEngineID` | under 0.01 s | 0.41 s | over 41x | 10.5 MiB | 43.5 MiB | 4.1x |

Five of the six clear ten times on both axes, by between 29 and 575 times. The sixth does not, and it is worth being clear about why: q8 groups twenty values, DuckDB answers it in 43.5 MiB, and rudb's 10.5 MiB is an empty process rather than an achievement. A ratio needs something to be large, and on this query nothing is.

Every one of the six returns rows identical to DuckDB's. The four with a tie free ordering were also checked against rudb's own row path on the same file, by adding a second sort key, which disables the push down and forces the scan.

The files the two engines wrote:

| | rudb | DuckDB | ratio |
| --- | ---: | ---: | ---: |
| `AdvEngineID` | 1.86 MiB | 3.26 MiB | 1.76x |
| `ClientIP` | 134 MiB | 162 MiB | 1.20x |
| `SearchPhrase` | 283 MiB | 479 MiB | 1.69x |
| `WatchID` | 836 MiB | 886 MiB | 1.06x |
| `UserID` and `URL` | 1,609 MiB | 3,643 MiB | 2.26x |

Disk is the axis where nothing here helps. The synopsis is a cost on the file, not a saving, and the range from 1.06x to 2.26x is ordinary column compression doing ordinary work.

The two numeric columns are worth calling out because document 28 said their margin was unmeasured and would be worse. Their synopsis really is a bounded pass with a bound in the ordinary sense rather than an exact count of the 513th value. Both certified anyway, and `UserID` at 575 times is the largest ratio in the table. A bound only has to beat the boundary count, and being loose costs nothing when the distribution is not close.

## The refusal, also measured

A fast path that cannot refuse is not a fast path, so the refusal needs a number too. `WatchID` is the near unique column document 26 named as the hard shape, and `SELECT WatchID, COUNT(*) FROM hits GROUP BY WatchID ORDER BY COUNT(*) DESC LIMIT 10` fits the predicate above perfectly:

| | processor | peak |
| --- | ---: | ---: |
| rudb, refused and scanned | 182.24 s | 6,356 MiB |
| DuckDB | 39.62 s | 3,322 MiB |

The top count in that column is 2 and the tenth is 1, so the answer's last row ties with most of a hundred million others and there is no boundary to prove anything against. The synopsis says so, the operator believes it, and the query pays for a full table.

That is the design working. It is also the honest other side of every ratio in the previous section: outside the shape, rudb is 4.6 times slower than DuckDB on processor and takes 1.9 times the memory, and nothing in documents 28 or 29 changes that. The refusal is cheap to decide, being a walk over at most five hundred entries. What follows it is not.

## What blocks the other twenty three

Every grouped query that does not fit is blocked by at least one of four things, and several are blocked by more than one:

**The aggregate is not a plain count.** Twelve queries: q9, q10, q11, q12, q14, q22, q23, q28, q29, q31, q32, q33. Either a `COUNT(DISTINCT UserID)`, which is not additive over rows and so has no counterpart to document 26's argument, or a count sitting beside `SUM`, `AVG` or `MIN`, which the synopsis holds no values for.

**The key is genuinely more than one column.** Nine queries: q15, q17, q18, q19, q31, q32, q33, q41, q42. The format stores one synopsis per column and no product of two of them bounds a pair's count from below. It does bound from above, since a pair occurs no more often than its rarer half, and that upper bound is what any future design here would have to work with.

**The filter names a column other than the one being grouped.** Eight queries: q22, q23, q37, q38, q39, q40, q41, q42. Document 28's proof turns on a filter deciding whole groups. A predicate on `CounterID` decides rows inside a `URL` group, and the `URL` synopsis says nothing about which rows those are.

**The key is an expression the synopsis cannot be mapped through.** Four queries: q19, q29, q40, q43. `REGEXP_REPLACE(Referer, ...)`, `extract(minute FROM EventTime)` and `DATE_TRUNC('minute', EventTime)` each group by a function of a column, and a function can merge two of that column's values into one group. Counts then have to be added, which the prefix cannot do for the values it omitted.

## Which of the four is worth attacking

Not the fourth. Four queries, and the functions involved are many to one in a way that merges omitted values into surviving groups, which breaks the bound rather than loosening it. The exception is a monotone injective function such as the subtraction the reader already folds, and the reader already folds it.

Not the third on its own. A selective filter over other columns is a scan problem and not a synopsis problem. q37 through q42 all filter `CounterID = 62` and a two week range out of a year, which is a zone map question, and the right answer for them is a much smaller scan rather than no scan. That is worth doing and it is a different document.

The first is where the queries are, and it splits cleanly. A count beside a `SUM` or an `AVG` is not hopeless: the synopsis identifies the surviving groups, and a second pass could compute the other aggregates for those groups alone. That is document 26's two pass structure with its first pass already paid for at checkpoint, which is the cheapest version of that design anyone in this series has had available. A `COUNT(DISTINCT)` in the `ORDER BY` is hopeless by this route, and four queries have one.

The second is where the honest argument is, and it is worth stating because this series keeps rediscovering it as future work. For `WatchID, ClientIP` the marginal synopsis of `WatchID` proves that every pair is rare, which is true and useless: the query asks which of a hundred million near ties comes first. No summary of either column separately can answer that, and the measurement in the previous section is what it costs to find out. Queries 32 and 33 are not a gap in the design. They are outside what any per column synopsis can do, and the next document that lists them as future work should say so instead.

## Where that leaves the target

Five of forty three queries clear ten times on both axes in the native quadrant, with margins between 29 and 575 times, measured against DuckDB's own native format rather than against a model. That is five times what document 28 claimed, and four of the five were obtained by reading the code rather than by changing it.

It is still five of forty three, in one of four quadrants. The Parquet quadrant is untouched and document 25's arithmetic on the time axis still stands there. Disk is untouched everywhere. Of the twenty three grouped queries that do not fit, four are provably out of reach of a per column synopsis, four more are nearly so, and the rest would need either a second pass or a scan that skips stripes, neither of which exists. And on a query that fits the shape but has no skew to prove anything with, rudb is currently several times worse than DuckDB rather than better.

## What this document does not claim

It does not claim the six are representative. They are the queries whose grouping key is one column and whose only aggregate is a count, which is the easiest shape in the suite, and the classification above exists precisely to say how much of the suite that is not.

It does not claim the ratios survive a full width table. Each measurement is a table holding only the columns its query needs, because that is what the host's disk allowed. The synopsis path touches one column's directory and would not change, but both engines' files and both engines' load times would.

It does not claim the fallback is acceptable. `WatchID` says it is not, and improving it is untouched by anything here.

It does not claim the host was quiet. Another tenant held the load average between 15 and 23 throughout, and freed fifty gigabytes of disk in the middle of it. That is why the tables above report processor time and the wall clock figures are not in them.
