# A push a parent at a time

With `graph_sections` on, a join whose build side is a set of parent rows turns that set into the exact child rows the scan on the other side should read. This is the reduction of spec/graph/05-execution.md section 5.4, and it runs once the build side is complete, on one thread, before the probe side can start. Measured against the same file with the setting off, it made most of TPC-H slower rather than faster, so this note looks at where that time went.

## What it cost

At SF1 on gamingpc, with every query repeated ten times in one process so that opening the file is not in the number, q03 took 10 ms with the setting off and 34 ms with it on. q05 went from 12 ms to 39 ms and q10 from 16 ms to 39 ms. Turning `graph_reduction` off while leaving `graph_sections` on brought all three back to within a millisecond of off, so the loss was the reduction and not the link join or anything else the layer does.

Timers around the reduction split it into three parts: reading the key map and the link out of the file, looking up every build row's key in the parent's key map, and pushing the resulting set of parent rows through the link to the child. Reading the sections cost about half a millisecond. The push cost 23 ms in q03 and 24 ms in q05, which is the whole regression.

## Why the push was slow

The push read the link a child at a time. For each of the six million `lineitem` rows it decoded the parent row and tested it against the set, which is about four nanoseconds a row, and it did that even though the link from `lineitem` to `orders` is stored in its monotone form. That form is a bit vector with one run of ones per parent, one bit per child, and a zero after each run. It already says which children belong to each parent as one range, so there is no need to ask about each child separately.

## The change

When the link is monotone, the push now walks the runs. Each parent is one step: the length of its run is counted a word at a time, and if the set holds the parent the whole range of children is set in the output at once. That is a million and a half steps for `orders` rather than six million, and a set bit range rather than a test and a set per child. A sparse set is read with a cursor that moves forward as the parents do, since the walk asks about them in ascending order.

Two things behave a little differently. A part now counts as skipped when none of its rows is kept, which for a link in parent order is the same part the zone map would have ruled out. And the early stop of section 5.4 is asked at the first run that starts past the first third of the children rather than at the first part boundary past it, which is the same question asked at most a few rows later. The packed form is read as before.

## Results

The same timers on server3 at SF1, three runs of each query in one process, summed over the pushes in each run. The box was under load from other work, so the absolute numbers are several times what an idle machine gives, but both builds ran on the same box one after the other.

| query | push before | push after |
|---|---|---|
| q02 | 37.3 ms | 15.5 ms |
| q03 | 74.0 ms | 30.7 ms |
| q04 | 166.4 ms | 34.3 ms |
| q05 | 128.1 ms | 45.6 ms |
| q07 | 138.7 ms | 49.4 ms |
| q10 | 194.9 ms | 14.5 ms |

All 22 answers with the setting on match main with it off. The new test builds a monotone link with parents that have no children and parents whose runs cross words and parts, and checks the kept rows and the skipped parts against the child at a time definition for empty, sparse and dense sets.

## What is left

With the push smaller, the key map lookups ahead of it are now most of what the reduction costs, one lookup per build row on one thread. The build side of these joins often has hundreds of thousands of rows, so that step is the next one to take apart, either by splitting it across the workers that gathered the build side or by carrying the parent row through from the scan so there is nothing to look up. Until then `graph_sections` stays off by default.
