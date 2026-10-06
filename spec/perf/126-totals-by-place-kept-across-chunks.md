# 126. Totals by place kept across chunks

## What was slow

A chunk that adds its totals by place (note 115) left about 400 of q01's places with rows, and `fold_places` added each of them to what its group is owed and cleared its cells before the next chunk (note 117). On q01 at SF1 that is 724 chunks of about 8,000 rows each, and after note 125 the fold was still 8 percent of q01's instructions, about 26,000 a chunk. Nothing in a place's cells has to reach its group until something reads the groups or the map is about to mean something else.

## The change

The cells by place now go on from chunk to chunk. `PlaceSums` remembers the calls, the counting calls and the stride the cells were added up for, and how many kept rows they hold. `carries` says whether the next chunk can add into them. It can while the calls are the same and are fed the same way, the stride is the same, and the rows held stay under 2^30. When it cannot, the caller folds them in before the chunk. They are also folded in just before the map is held for other keys, reset or widened, since a place only means its group against the map it was added up under, and just before the groups settle, which comes before anything reads the accumulators or moves the groups.

A packed run's base can move from chunk to chunk. q01's `l_extendedprice` has a base of its own on every page, and refusing to carry when it moved meant the cells were folded nearly every chunk. Only 3 of the 724 chunks changed the map. The cells now hold codes counted from the bases of the first chunk they took. A later chunk's codes are lifted by how far its base moved before they are added, so a code of it plus the lift plus the first base is still its value. The pair kernel of note 125 adds the lift in lanes after it widens the codes, which is one more add per four rows of each side. Any other pass adds it to the codes it read out. A code is under 2^32 and a lift is held under 2^31, so over fewer than 2^30 rows no total can leave its `i64`.

`touched` still runs every chunk to find places with no group yet. It now checks the counts the cells have held since the last fold against the rows they hold, and it checks the map for each place it finds with rows rather than writing down a list. The fold walks the list of places that have ever had rows, since whenever those fell short of the rows held, `touched` looked at every place and put each one with rows on the list.

## Measured

Single thread at SF1 on server2, warm instructions and cycles as the difference between eleven runs and three in one process, against main at #2614. The answers to all 22 queries are the same bytes as before at one thread and at six.

| query | instructions before (M) | after (M) | cycles before (M) | after (M) |
| --- | --- | --- | --- | --- |
| q01 | 227 | 191 | 141 | 109 |
