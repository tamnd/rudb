# The bucket pass, measured

## Why this document exists

Document 26 described an exact bounded top count, proved its certification in two lines, measured the one property of the file it depends on, and then priced it by arithmetic over document 25's scan floor. Pricing by arithmetic is what document 20 got wrong and what document 23 got wrong, and document 26 said so about itself.

So this one built it and ran it. `cargo xtask topcount` reads a column of a Parquet file through rudb's own reader and groups it two ways: `exact` holds every key, which is what the aggregate does today, and `buckets` is document 26's design. Both hash with the same function, read through the same decoder, and differ only in what they remember.

## What it is and what it is not

It is a harness, single threaded, with a plain hash table. The engine's aggregate is neither of those things, so the absolute times here are nothing like a query's and the ratio between the two modes is not the ratio the engine would see. Reading it as one would be the instrument error document 21 records.

What transfers is the memory. The harness's `exact` mode peaked at 4,124 MiB on this column and rudb's own query peaks at 3,884 MiB on the same column of the same file, which is six percent apart. A harness whose baseline lands on the engine's baseline is a harness whose other number is worth reading.

## The measurement

`Referer` of `hits.parquet`, 81,032,736 rows past the `<> ''` filter, sixteen million buckets, a budget of a thousand, the best of two runs of each mode on a host at load average between 13 and 29.

| | user | system | wall | peak resident |
| --- | ---: | ---: | ---: | ---: |
| exact | 310.30 s | 20.47 s | 557.77 s | 4,124 MiB |
| buckets | 81.70 s | 11.02 s | 128.86 s | 89.3 MiB |

Both printed the same ten rows with the same counts, ending on the 247,459 that document 23 and document 26 both name, and `buckets` printed what it did to get there:

```
81032736 rows, 11585953 buckets used, threshold 3540,
3394 candidate keys, two passes, certified exact
```

Three thousand three hundred and ninety four keys counted exactly out of 19,720,796. A threshold of 3,540 against an answer whose last row holds 247,459, so certification passed with seventy times the margin it needed, which is what document 26 predicted from DuckDB's view of the same buckets.

So the design works, it is exact, it says so, and it holds the whole query in 89.3 MiB.

## What this says about the engine

The harness's two modes differ by 3.80 times on user time, and that number does not transfer, because the harness's `exact` mode allocates a boxed key per group where rudb's table does not. rudb's engine reads and groups this column in 93.64 seconds of user time against the harness's 310.30, so the engine's table is three times better than the harness's baseline and the 3.80 is measuring that as much as anything.

The part that does transfer is the arithmetic underneath. `buckets` spent 81.70 seconds of user time on two decodes plus the hashing and the increments, and document 25 measured one of rudb's decodes at 31.77 seconds of user time. Two of those are 63.5, which leaves about 18 seconds for everything the design actually does, across both passes, over 81 million rows. That is the number worth having and it is small.

Carrying it into the engine gives about 82 seconds of user time against today's 93.64, and removes the table whose page work document 25 measured at 46 seconds of system time, giving about 96 seconds of processor time against today's 147. Against DuckDB's 66.81 that is 1.44 times behind, where today rudb is 2.20 times behind.

And it gives about 90 MiB against today's 3,884. **Against DuckDB's 4,869 MiB on the same query that is fifty four times less.**

## Where that leaves the target

The target asks for ten times better performance and ten times less resources in each of four quadrants. On this query, in the Parquet quadrant, one of those two is now a measured structure rather than a projection:

| | rudb today | DuckDB today | with the bucket pass |
| --- | ---: | ---: | ---: |
| processor time | 147.03 s | 66.81 s | about 96 s |
| peak resident | 3,884 MiB | 4,869 MiB | about 90 MiB |

Ten times less resources is cleared by more than five times over. Ten times faster is not, is not close, and document 25 proved it cannot be, because DuckDB's scan alone is more than four times the whole ten times budget and rudb's is more than six.

That is the honest position and it has not moved: **the target is met on one of its two axes, in one of its four quadrants, on one query, by a structure that exists in a harness and not yet in the engine.** Everything else in the target is either measured short or proved out of reach.

## What has to happen next

The operator. The harness proves the structure and the numbers; it does not make a single query faster, because nothing in the planner knows about it. What the engine needs is a source it can scan twice, which document 25 noted it does not have, and an aggregate that runs the bucket pass, picks a threshold, runs the exact pass over the survivors and refuses to certify when the margin is not there.

Then the shape has to be checked where it is hardest. Queries 32 and 33 group by a key close to unique across a hundred million rows and their top counts are a handful, so their margin is thin or absent and they are the queries most likely to fail certification and fall back. Document 26 named them and nothing has measured them.

## What this document does not claim

It does not claim the engine will see 96 seconds. That figure is the harness's 18 seconds of design cost added to document 25's measured decode and today's measured table removed, and every one of those three terms carries its own error. The memory figure is the one to trust, because the harness's baseline landed on the engine's.

It does not claim one column is a suite. Twenty of the forty three queries have the shape; one of them has been measured, and it is the one with the friendliest distribution in the file.

It does not claim two passes are necessary forever. Document 26 sketched a version that keeps the hashes and decodes once, at 712 MiB rather than 90, and this harness measured neither it nor the trade between them.

It does not claim the harness's hash is the engine's. Both modes here use the same one, which is what makes the comparison fair, and neither is the one `rudb-exec` ships.
