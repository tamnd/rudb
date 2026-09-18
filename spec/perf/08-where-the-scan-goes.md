# Where the scan goes

The last note stopped at the group by and said the key encoding was next. This one is about the other half of the suite, which turns out to be the bigger half, and about a number in the harness output that I had been reading past for weeks.

## The comparison is not the one I thought it was

`rudb-bench run clickbench --engines rudb,duckdb --rows 10m --runs 3` on the thirty two thread machine, current main:

| | duckdb | rudb |
|---|---|---|
| query time | 3.007s | 5.789s |
| hot cpu | 38.130s | 70.440s |
| peak RSS | 1.61 GiB | 1.50 GiB |
| load | 7.606s | none |
| on disk | 3.72 GiB | 1.83 GiB |
| storage | own format | the Parquet |

We are 1.93 times slower on wall clock and 1.85 times on CPU, and we use less memory than they do. That is the honest headline and it is nowhere near ten times either way.

But look at the load row. DuckDB spent 7.6 seconds writing the data into its own format and then answered every query out of that. We read the Parquet file inside every query, forty three times, decoding it again each time. The harness has always printed this and has always said what it means in the paragraph underneath. I had been reading it as a footnote. It is not a footnote, it is most of the finding.

`rudb :memory: nat.db` says `cannot open, because there is no storage format yet, see #103`. So the comparison is our Parquet decode against their memory mapped native blocks, and the operator breakdown says exactly what that costs:

| operator | cpu | share | per row |
|---|---|---|---|
| FileScan | 31.281s | 48.0% | 91.7ns |
| Aggregate | 23.382s | 35.9% | 128.0ns |
| Filter | 9.788s | 15.0% | 51.1ns |
| Project | 553.829ms | 0.8% | 2.6ns |
| TopN | 185.400ms | 0.3% | 5.9ns |

Half of everything we spend is turning Parquet into vectors, and the engine it is being compared against does not do that at all during the measurement. A native format is worth about two times on its own, which would put us level with DuckDB before any of the rest of this note. That is #103 and it is the single largest item on the board.

None of which makes the decode work wasted. A native format has to be written from something, the loader pays the same decode once, and every query that reads a Parquet file a user already has pays it every time. DuckDB reading the same Parquet directly is still 1.6 times faster than we are at it. So the decode has to get faster regardless, and what follows is where it goes.

## The profile

`valgrind --tool=callgrind` on `SELECT count(*) FROM hits WHERE URL LIKE '%google%'`, one million rows, one thread, which is the smallest query that touches the whole scan path.

| what | Ir | share |
|---|---|---|
| snappy decompress | 527,734,964 | 31.6% |
| the LIKE pattern | 413,992,494 | 24.8% |
| UTF-8 validation | 213,299,135 | 12.8% |
| memcpy inside the above | 113,797,775 | 6.8% |

Four things are eighty percent of a scan. Three of them are fixable without changing any layout at all and two of them are fixed as of today.

## The searcher was built per row

`memmem::find` is not a search. It builds a searcher out of the needle, by scanning it for its two rarest bytes and working out the stride between them, and then it searches. The building is a few hundred instructions and it does not depend on the row. The `LIKE` loop was calling `find` per row, so it was rebuilding a searcher for `google` ten million times per query.

Building it where the pattern is compiled, which is once per plan, took a quarter off the query. Twenty two percent of everything, filter and scan and aggregate together.

The same change also stopped asking the question per row when the column is dictionary encoded. A substring search is the most expensive thing any of these kernels does to one value and a dictionary is a promise that the value comes back, so the answer is computed once per entry and gathered. That turns a string predicate from linear in rows into linear in distinct values, and per #288 every string column in the published file is dictionary encoded bar the widest few.

The guard is worth writing down because getting it wrong would have made this a loss. A dictionary can have more entries than the rows pointing at it, which is what the first chunk of a row group looks like when the dictionary serves the whole group. Answering per entry there searches values nobody asked about. So the per entry pass only runs when the dictionary is shorter than the chunk.

## The decompressor called libc seven million times

`__memcpy_avx_unaligned_erms`, 7,022,549 calls, to read one column of one million rows. One per copy element, and seventy two percent of the copy elements on a real page are four to seven bytes.

All of them came from `copy_within`. It is the overlapping move, it lowers to `llvm.memmove`, and the compiler leaves that as a call even where the length is a constant sixteen. The literal path beside it uses `copy_from_slice`, which is the non overlapping one, and that inlines completely: three calls in the whole query rather than seven million.

Two of the three copy paths never overlap and now say so, one through a sixteen byte array and one through `split_at_mut`. Calls went to 46,380 and the query lost seven percent of its instructions. The repeating pattern path, where a run of sixty four identical bytes arrives as an offset of one, does overlap and is untouched.

## What is left, in order

Decompression is still twenty nine percent of that query and it is the largest single thing in the engine. The profile now says the cost is validation rather than copying: per element there is a length check, an offset check, a room check and a tag test, and together they are about half the instructions the loop spends. Every fast snappy decoder splits into a body that has proved once that there is room for sixteen more bytes on both sides and a checked tail for the last few. We have not done that.

UTF-8 validation is sixteen percent. `StringColumn::push_in_place` calls `str::from_utf8` per string, and the function is not even inlined: it shows up in the profile as its own symbol at 213 instructions per URL. The doc comment on that method already says the right answer, that a reader whose format guarantees UTF-8 wants to validate the page once rather than once per string.

An important thing settled while looking at this. I had written in note 07 that we should stop validating altogether, on the grounds that it would be more DuckDB compatible. That is wrong and the experiment says so. Writing a BLOB holding `\xFF\xFE` to Parquet and reading it back with `binary_as_string=True` gets `Invalid Input Error: value is not valid UTF8` out of DuckDB. They validate, they throw, and so must we. What is available is doing it faster, not skipping it.

The hybrid RLE and bit packing reader is fourteen percent of a `SELECT *`, which is the next one after those two.

## The thing the scaling sweep said

The same query at ten million rows, both engines, threads swept:

| threads | rudb wall | rudb cpu | duckdb wall | duckdb cpu |
|---|---|---|---|---|
| 1 | 1.09 | 1.09 | 0.67 | 0.72 |
| 2 | 0.59 | 1.17 | 0.36 | 0.76 |
| 4 | 0.33 | 1.29 | 0.23 | 0.86 |
| 8 | 0.19 | 1.37 | 0.18 | 1.08 |
| 16 | 0.16 | 1.89 | 0.18 | 1.38 |
| 32 | 0.15 | 2.62 | 0.20 | 1.46 |

We scale 7.3 times across thirty two threads. DuckDB scales 3.7 and then goes backwards at thirty two. At the top both engines are within a few hundredths of each other and neither is bound by instructions any more, because ten million rows of `URL` is more memory traffic than either has work to hide behind it.

This is why the LIKE change did not show at all in a thirty two thread wall clock: 0.17 to 0.16, inside the noise, while the instruction count fell by twenty two percent. Measuring a CPU change at the thread count where the query is memory bound measures the memory.

So the development loop for anything that removes instructions is: callgrind at one thread for the size of the change, one thread wall clock for whether it is real, and the full thread count only to check it did not make scaling worse. Doing it the other way round is how the last two days produced two contradictory benchmark tables.

The corollary is less comfortable. Our parallelism is already good, better than theirs on this query, and there is no ten times hiding in the thread count. The ten times has to come from doing less work per row, and the largest single piece of work per row is decoding a file format that the engine we are measured against is not reading during the measurement.

## The validation split, which did not work

Note 08 said above that the next thing in the decompressor was splitting the element loop into a body that has proved once that there is room for sixteen more bytes on both sides and a checked tail for the last few. That is what every fast Snappy does and it is what the line level annotation pointed at. I wrote it and it is worth nothing at all.

The shape: one `element` function taking a `const ROOM: bool`, called from two loops, one of which has proved `input.len() - src >= 64` and `out.len() - pos >= 64` and passes `true`. Sixty four because a copy is at most sixty four bytes and a literal whose length rides in its tag is at most sixty, so an element that far from either end cannot run off. Under `ROOM` the length check, the room check and both slice bounds checks are gone.

Instructions on the usual query, one million rows, one thread:

| | main | the split |
|---|---|---|
| `decompress_into` | 501,354,558 | 455,752,986 |
| `long_or_overlapping_copy` | inlined | 44,964,228 |
| the two together | 501,354,558 | 500,717,214 |
| the whole query | 1,149,421,140 | 1,150,309,476 |

Six hundred thousand instructions out of five hundred million, which is a tenth of a percent, and the query as a whole is flat. The forty five million that left `decompress_into` did not go anywhere better, it went into the copy helper that got pushed out of line in the same change. LLVM was already folding the checks I went to remove.

Three things went wrong on the way there and all three are worth remembering, because each of them was a bigger effect than the change itself.

The first version passed the two cursors as `&mut usize`. That is 35 percent slower than the loop it replaced. The compiler will not keep a cursor in a register when it is behind a mutable reference that an inlined callee also writes, so both were reloaded from the stack every element. Passing them in and out by value fixed it.

The second version marked the emit helpers `#[inline]`. With two callers to weigh, the compiler put the whole of `emit_copy` out of line, including the four instruction branch that nine copies in ten take, and charged a call for it: 291 million instructions, a 16 percent regression on the query. `#[inline(always)]` on the hot branch with the rare ones behind `#[inline(never)]` fixed that.

The third version then put the literal's exact width branch out of line too, by symmetry, and that cost 30 million instructions back. A literal is long about as often as it is short, because a page of `URL` is three million literals and most of them are a whole URL. The symmetry was wrong: copies are 90 percent one shape and literals are not.

So the decompressor is not spending its time on validation. At about 50 instructions an element and 1538 MB/s it is within reach of what reference Snappy does, and there is no micro change left in it that is worth the code. The way to stop paying for it is to stop doing it forty three times, which is #103.

That is the last of the scan micro work. Everything after this is the format.
