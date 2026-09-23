# What a query costs before it reads

## Why this document exists

Document 31 ended with four of forty three queries clearing both axes at ten million rows, and named three more that were over a hundred times ahead on processor and missed only on memory: q5 at 12.9 MiB against a limit of 10.8, q16 at 13.5 against 12.6, and q36 at 13.9 against 11.9. Their operators hold almost nothing. What they paid was the floor, the resident size of a process that has opened the table and run a statement, which document 31 put at 12.8 MiB and left alone.

A floor is not a constant. It is made of things, and each of them is there for a reason that can be checked. This document takes it apart, finds two of those reasons wrong, and fixes them.

## Where the floor is

A resident size is file pages and anonymous pages. `/proc/<pid>/smaps` of q5 on the ten million row file, taken after the answer and before exit, splits the 13.3 MB like this:

| | resident |
| --- | ---: |
| the `rudb` binary's own pages | 7.0 MB |
| mimalloc's heap | 4.1 MB |
| the C library, libm, the loader | 2.0 MB |

Neither of the two large rows is data. The binary is 12.8 MB on disk and a statement brings in over half of it. The heap at the peak of q5 is the table's directory, because the answer to `COUNT(DISTINCT UserID)` is one number in that directory and the query reads nothing else.

## The directory

A table directory is everything the reader knows about a table without reading a page: its fields, and for every stripe and every column the page's place in the file, its optional membership, sieve and part range pages, and the column's two ends, null count and sum. After those come the frequency synopses, one per column. Over `hits` at ten million rows it is 960,350 bytes: 522,716 of stripes, 425,856 of synopses and the rest trailing blocks.

Opening the table read all of it into one buffer, checked it, decoded it, and only then let the buffer go, so the peak held both. A massif trace of the system allocator build had the decoded form at about 2.6 MB beside the 0.96 MB it came from. Of that, 958 KB was synopses, 19,953 entries at forty eight bytes each, and 524 KB was three `Vec<Option<Page>>` per stripe of thirty two bytes a slot, holding 7171 pages across 16,380 slots.

The principle the directory breaks is the one every other structure in this format keeps, that a query pays for the columns it touches. The directory made every query pay for all hundred and five, at open, before it was known what the query wanted. Three changes, none of which touch the file format:

The directory is checked and decoded out of a 64 KiB window rather than a buffer of its own size. The xxHash64 it is checked against is carried across reads in the four lane state the algorithm already has, and decoding follows the checksum through the same window. The file is read twice, and the second read comes out of the page cache the first filled.

The synopses are left in the file. At open each one is read through and checked, as before, so a torn one still refuses the table, and what is kept is where it is: sixteen bytes a column instead of up to twenty four kilobytes. A query that asks about one column's frequencies reads that column's synopsis back, a few kilobytes, and the reader already remembers the decoded values per column.

The optional pages of a stripe are kept sparse, a sorted list of the pages that are there, each packed to twenty four bytes with its column in what was padding.

The directory's share of q5's peak was 3.1 MB in the massif trace. The whole heap at that peak is now 1.5 MB, and mimalloc's resident pages go from 4.1 MB to 2.0 MB.

## The relocations

The other half of the floor is the binary, and most of what it brings in is not code. A position independent executable carries its pointer tables, vtables and static slices, as relocations the loader applies at every start. This one had 39,600 of them: a 950 KB `.rela.dyn` that the loader reads from end to end, and a 1.2 MB `.data.rel.ro` that it writes into, which makes every page of it resident and private whether any query ever reads it. Before any database is opened, the process has paid 2.1 MB for the address it was loaded at.

Linked at a fixed address, the linker resolves those relocations once. The tables become ordinary read only pages of the file, and a query brings in only the ones it touches. The read only segment in front of the code goes from 1.5 MB resident to 0.6, and the relocated one from 1.2 MB to 0.3. `rudb-cli`'s build script now passes `-no-pie` to its binaries on Linux.

What that gives up is that the shell's own code is at the same address every time it runs. Its libraries, heap and stack are still placed at random. The library crates and the C API are untouched, and a program that embeds rudb links however it chooses.

## Measured

The same ten million row file, on the same host, each binary run three times interleaved with the one before it, with the smallest peak of the three kept. The host carried a load near twenty three from other work, so the processor numbers are not worth a column.

| | main | directory | fixed address | DuckDB | needed |
| --- | ---: | ---: | ---: | ---: | ---: |
| `SELECT COUNT(*)` | 11.5 MiB | 11.0 MiB | 9.4 MiB | | |
| q5 | 11.9 MiB | 11.1 MiB | 9.4 MiB | 107.9 MiB | 10.8 MiB |
| q16 | 12.6 MiB | 11.9 MiB | 10.1 MiB | 126.1 MiB | 12.6 MiB |
| q36 | 13.0 MiB | 12.0 MiB | 10.4 MiB | 118.7 MiB | 11.9 MiB |

The directory changes are worth 0.5 to 1.0 MiB of resident size and not the two megabytes and more they took off the heap. mimalloc keeps pages it has been given, and part of the directory's old peak was pages the rest of the query would have touched anyway. The fixed address is worth a further 1.6 to 1.8 MiB on every query, because it is paid before the query starts.

The whole suite, run once with the new binary against the numbers document 31 recorded for DuckDB:

| | processor | peak | DuckDB peak | memory ratio |
| --- | ---: | ---: | ---: | ---: |
| q5 | 123x | 9.4 MiB | 107.9 MiB | 11.5x |
| q6 | 41x | 9.4 MiB | 222.3 MiB | 23.7x |
| q13 | 34x | 15.1 MiB | 273.1 MiB | 18.1x |
| q16 | 114x | 10.1 MiB | 126.1 MiB | 12.5x |
| q34 | 53x | 22.8 MiB | 989.9 MiB | 43.5x |
| q35 | 99x | 23.0 MiB | 1013.7 MiB | 44.1x |
| q36 | 107x | 10.5 MiB | 118.7 MiB | 11.3x |

Seven of forty three clear both axes, where document 31 had four. Every peak under 26 MiB fell, by 2.3 to 3.8 MiB. Above that a single pass moved peaks in both directions by more than the floor, q25 down 10.1 MiB and q38 up 6.2, which is the host and the scheduling of the query's own threads rather than anything at open. Every answer is the one the previous binary gave, apart from which ten groups q18 lists, having no `ORDER BY`, and which rows tie at the limit in q22, q23, q32, q33, q40 and q41.

## What is left of the floor

The new floor is 9.4 MiB, and 4.5 MB of it is code. The text segment is 7.25 MB and a statement brings in 4.5 MB of it. That is not because a statement runs 4.5 MB of instructions. A page fault on a file mapping maps the sixteen pages around the one that faulted, if they are cached, which they are, so a function that is called once costs 64 KiB of resident size wherever it sits. Code a query uses is spread across the whole segment because the linker orders functions by crate and module, not by use. Ordering the functions a statement runs into one contiguous stretch, from a profile of the suite, is the lever for this part, and it is a change to how the binary is linked rather than to what it does.

The directory still costs 611 KB of stripe zones at open, one 112 byte `Range` for every column of every stripe, and the reader builds three slot tables of the same shape for the pages, sieves and part ranges it reads lazily. At ten million rows those are about a megabyte. At a hundred million, with ten times the stripes, they are about ten, and the stripe section of the directory itself is about five. That scaling is the next structural item and it needs the format to change: a directory stored column by column, with an offset per column, so that opening a table reads its fields and its stripe lengths and a query reads the ends and pages of the columns it names. Every access to a stripe's per column fields goes through the reader, so the change is to what the reader holds and not to what the operators ask it.

## What this does not claim

It does not claim the target is met. Seven of forty three queries clear both axes, in the native quadrant, at one scale. The Parquet quadrant is not measured here.

It does not claim the floor is the same on another system. The loader's relocation cost and the fault around width are Linux's, and the fixed address change is only made there.

It does not claim a quiet host. The memory column was taken interleaved to be comparable, and the processor column was not used for anything.
