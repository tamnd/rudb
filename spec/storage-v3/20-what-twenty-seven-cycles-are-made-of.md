# What twenty seven cycles a value are made of

## Why this document exists

Document 18 treats the scan as the floor under the suite, and derives the project's ceiling from it: the scans are 19.3% of suite CPU, so zeroing every operator above them leaves about five times better and not ten. A floor is a strong claim and this document tests it, because if the scan is merely slow rather than near the limit of the hardware then the ceiling moves and so does everything document 18 concluded.

The test says the floor is closer to real than to arbitrary, but not for the reason document 18 assumed, and the measurement that got there was misread once on the way. That misreading is recorded here rather than removed, because it is the fourth of its kind in this series and the first one caught inside a single session.

## The measurement that looked like slack

A `COUNT(*)` over one column with a predicate, repeated, taking the minimum as the estimator least damaged by a host at load 16 to 23 on eight cores:

| column | width | scan CPU, best of six | per row |
| --- | ---: | ---: | ---: |
| `UserID` | 8 bytes | 0.068 s | 6.8 ns |
| `ResolutionWidth` | 2 bytes | 0.088 s | 8.8 ns |

Then the same measurement with columns added to one query, so that the marginal cost of a column can be read directly rather than inferred:

| columns scanned | scan CPU, best of four | per row | marginal |
| --- | ---: | ---: | ---: |
| one, `u16` | 0.089 s | 8.88 ns | |
| two, adding an `i64` | 0.174 s | 17.39 ns | 8.51 ns |
| three, adding an `i32` | 0.275 s | 27.46 ns | 10.07 ns |

A scan costs about nine nanoseconds per row per column and it costs that whether the column is two bytes wide or eight. Nine nanoseconds is roughly twenty seven cycles, and a bit unpacking loop that uses the vector registers this machine has should retire a value in less than one. The obvious reading is that the scan has more than an order of magnitude in hand, which would mean document 18's floor is not a floor and the ceiling it implies is not a ceiling.

That reading is wrong, and the flatness is the clue that it is wrong rather than evidence for it. A decoder that ran at memory speed would show the two byte column finishing in a quarter of the time of the eight byte one. One that charges the same for both is not moving bytes at all. It is doing something per value whose cost has nothing to do with how wide the value is, and twenty seven cycles is only excessive if decoding is what they are being spent on.

## What the twenty seven cycles are

Forty repeats of the single column query under `perf`, sampled, symbols demangled, everything at or above one percent:

| symbol | share |
| --- | ---: |
| `rudb_encoding::integer::decode_chunk` | 22.1% |
| `rudb_kernels::compare::dispatch` | 6.8% |
| `rudb_native::narrowed` | 5.5% |
| `smp_call_function_many_cond`, in the kernel | 4.9% |
| `rudb_vector::chunk::Chunk::select` | 4.7% |
| `rudb_kernels::select::picked` | 3.5% |
| `rudb_encoding::bitpack::unpack_tail` | 3.3% |
| `mi_malloc` and `_mi_malloc_generic` | 3.2% |
| `__memset_avx2_unaligned_erms` | 1.4% |
| `__memmove_avx_unaligned_erms` | 1.4% |

Decoding is a quarter of the scan and not the whole of it. That single number retires the argument above: a decoder made infinitely fast takes the scan to about three quarters of what it costs now, which is 1.3 times and not ten. The rest is a predicate, a selection, a materialisation and an allocator, and each of them is doing work that something has to do.

So document 18's conclusion stands and its reasoning needs one repair. The scan is not near a hardware floor, and it is also not sitting on an order of magnitude. It is a layered cost where no layer is more than a quarter, which is the shape that resists a single fix and the shape that a ceiling argument survives.

## The reason a two byte column costs what an eight byte one costs

`rudb_native::narrowed` takes a `Vec<i64>` and returns the column's real type. Every integer column in this format is decoded into sixty four bit values first and converted into its declared width afterwards, into a second allocation, with the first one thrown away.

That is the whole explanation of the flat table at the top. A `USMALLINT` column is two bytes on disk and in the answer, and eight bytes for the width of one pass through memory in between, so the cost per row cannot depend on the width because every width is the same width while the work is happening. It also accounts for a good deal of what sits under the decoder in the profile, since `narrowed` is 5.5% on its own and the allocator, the `memset` and the `memmove` beneath it are another 6% serving buffers that a direct decode would not ask for.

Decoding into the declared width is a real and bounded piece of work with a number attached: it is worth something under a fifth of the scan, on a suite where the scan is a fifth of the whole, so about 4% of the suite. That is worth doing and it is not worth confusing with the kind of change the target needs. It is recorded here mainly because it explains a measurement that otherwise looks like an opportunity, and a measurement that looks like an opportunity and is not is the most expensive kind this series has produced.

## Where the allocator turns up

Document 17 found that half of rudb's minor page faults are avoidable by one mimalloc setting, and priced it as a configuration note beside the larger obligations. It appears in this profile exactly where that document predicted. `smp_call_function_many_cond` is the kernel broadcasting page table invalidations to the other cores, it is 4.9% of a scan that touches no file, and it is there because pages are being returned to the operating system and taken back. With `mi_malloc` at 3.2% and the `memset` of fresh pages at 1.4%, allocation and the page table churn behind it are approaching a tenth of the scan.

This is the first time a finding from that document has been confirmed by a different instrument. It does not change what document 17 recommended, and it moves the recommendation from a count of faults, which is easy to dismiss as bookkeeping, to a share of a profile.

## What this does to the target

Nothing good, and more precisely than before. A scan improved by every specific thing named here, the direct width decode and the allocator setting and a faster decoder, is worth perhaps two to three times rather than the ten to thirty the flat table appeared to promise. Folding that into documents 18 and 19 gives a suite around twenty three seconds against the 57.63 it costs now, which is 2.5 times better than rudb is today, and rudb is 2.09 times ahead of DuckDB at benchmark scale. The product is near five, which is where document 18 and document 19 independently arrived.

Three documents reaching five by three routes is the useful output. The target is ten, the gap is a factor of two on top of a program none of whose parts are implemented, and no measurement in this series has yet found the thing that would close it.

## What this document does not claim

The profile is a sample of one query shape on one column on a contended host, and the shares in it should be read as a ranking with rough magnitudes rather than as a budget. The per row and marginal figures are minima over repeats, chosen because a minimum is the least contaminated estimator available on this machine, which also means they understate the cost a busy run would pay.

It does not claim the layers it lists are irreducible. It claims that none of them is large enough on its own for the scan to hold an order of magnitude, which is the only thing the ceiling argument in document 18 needs, and which is the opposite of what the first table here appeared to say.
