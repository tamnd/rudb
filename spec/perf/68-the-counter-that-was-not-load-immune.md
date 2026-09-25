# The counter that was not load immune

Every note in this directory leads with instructions retired, and the reason it does is written in [`13-what-tpch-costs-in-instructions.md`](13-what-tpch-costs-in-instructions.md): the shared boxes have a wall clock noise floor larger than the effects being chased, and instructions retired does not move. [`57-no-branch-a-row-in-the-cut.md`](57-no-branch-a-row-in-the-cut.md) puts it more strongly and calls the counter immune to what else the box is doing.

That is true of the counter the notes meant and false of the counter the harness was reading. `perf stat -e instructions` counts the kernel's instructions along with the program's, and on a box somebody else is also using, the kernel's share moves a great deal.

## What it costs to get this wrong

The test is to compare a binary against itself. Same file on both sides, so the honest answer is 1.000x on every query, and anything else is the instrument talking.

| | suite | worst query |
| --- | --- | --- |
| `-e instructions`, before first every round | 0.982x | q03 0.862x |
| `-e instructions`, order alternated | 1.011x | q08 1.106x |
| `-e instructions:u`, order alternated | 1.000x | q12 1.001x, every other query 1.000x |

The first row is the harness as it stood. A binary beat itself by 1.8 percent over the suite and by 13.8 percent on q03, which is larger than most of what this directory has ever reported.

## The two faults

**The counter included the kernel.** q08 run twelve times in a row, same binary, same corpus, `-e instructions`: eight runs landed within 0.6 percent of each other at about 525.7 M, and three landed at 615 M, 623 M and 643 M. The same query with `-e instructions:u` ten times in a row spanned 488.24 M to 488.37 M, which is 0.027 percent. q03 spanned 0.027 percent too.

So the low state is a hard floor and the outliers are additive on top of it, which is the shape of interference rather than of an engine that makes different decisions on different runs. That was the first guess and it was wrong: pinning `seam.chunk.compaction` to `fixed-threshold` and to `never` did not remove the outliers, and the compaction gauge learns from measured nanoseconds, so it was a fair suspect. What the outliers actually are is the corpus falling out of the page cache when something else on the box wants the memory, and the disk read that follows retires its instructions in the kernel where the counter was picking them up.

Note 13's claim was never wrong about the machine. It was measuring the right thing and saying so; nobody noticed that the counter's default scope includes ring 0.

**The harness ran the sides in a fixed order.** Each round ran `before` and then `after` on the same query, so `before` pulled the pages off the disk and `after` read them warm, every round, on every query. That is a systematic gift to whichever binary is named second, and since `after` is by convention the change being proposed, it is a systematic gift to every proposed change.

## What the harness does now

`-e instructions:u`, which is the whole of the first fault. The order of the two sides alternates round by round, and one run a side is thrown away before any round is counted. The last two matter much less once the counter is user only, but a first run still pays for its own lazy symbol binding and its own heap growth, and neither is what is being compared.

One more thing worth putting back. Note 13 says best of three and the harness had drifted to the median of three. When interference is additive and the floor is hard, the minimum is the estimator that wants finding, and the median of three throws away the one run in three most likely to be clean. So it takes the lowest of the rounds now, on the start up baseline as well as on the queries, since that baseline is subtracted from every query and reading it high would flatter every ratio under it.

It also prints the spread of each query's rounds beside the ratio, as the larger of the two sides' coefficient of variation. That is the only check the reader has that the run was clean, and it wants reading before the third decimal place of a ratio is quoted anywhere. Over the 22 it comes out at 0.00 or 0.01 percent on eighteen of them and never above 0.08 percent.

And the harness is in the tree, at [`scripts/instructions`](../../scripts/instructions), rather than in a home directory on the box it was last used on. That is the part of this that took longest to notice. The measurement every note in this directory rests on lived in one `/tmp`, so nothing could review it, nothing pinned it to a version of the engine it had measured, and the fix above could have been lost to a reboot. Run against one binary on both sides it reads 1.000x on 21 of the 22 and 1.001x on q12, whose spread is 0.02 percent, over a suite of 12.42 G either way. That is the test any change to it has to keep passing.

## Why this note exists rather than a one line fix

Because of what it does to the small results. A change to the integer decoder was in hand when this turned up: a chunk of packed values was being unpacked into a buffer that `vec![0i64; count]` had zeroed first, and writing it through a warm scratch block instead removes a pass over the answer. It read **0.994x over the suite** on the old instrument, which is a win worth keeping, and **1.003x on the fixed one**, with all 22 queries flat or worse and none better. It is a regression and it was reverted. The mechanism the profile suggested was real and the arithmetic was wrong: `vec![0; n]` goes through `alloc_zeroed`, a fresh mapping is already zero, and there was no second pass to remove.

That is one change, caught because the null test had just been run. The directory has sixty seven notes before this one and the ones to re-read are the ones that turn on a few percent, particularly on the join queries, which are where the page cache has the most to lose. Nothing large is in doubt. The `x86-64-v3` result in [`66-the-registers-we-already-have.md`](66-the-registers-we-already-have.md) is 0.872x over the suite and 0.748x on q01 with every query winning, which is an order of magnitude outside this, and note 66's own lesson stands unchanged and now has a sibling: a ratio cannot see what it divides by, and a ratio cannot see what it adds either.
