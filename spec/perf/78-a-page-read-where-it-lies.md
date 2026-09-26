# 78. A page read where it lies

## The problem

Every query in this directory has been counted in `instructions:u` since note 68, which is the right counter for comparing two builds of the engine and the wrong one for asking where the CPU goes. Counted with `perf stat -e task-clock,user_time,system_time` on server3 at one thread over SF1, three copies of each query in one process, the kernel's share of a rudb run was large on every query we looked at:

| query | task | user | sys |
|---|---|---|---|
| q01 | 1132 ms | 890 ms | 258 ms |
| q06 | 295 ms | 161 ms | 145 ms |
| q09 | 2403 ms | 1731 ms | 727 ms |
| q12 | 927 ms | 584 ms | 397 ms |
| q18 | 825 ms | 561 ms | 284 ms |

On q06 half the time was in the kernel. The file was in the page cache the whole time, so none of that was waiting on a disk. The profile of q06 said where it went: the copy out of the page cache in `pread`, the kernel zeroing fresh pages for the buffer the copy went into, the faults that mapped those pages, and freeing them again afterwards.

That is the read path of the native reader doing exactly what it was written to do. A column page, or one part of it when the scan does not want the page, is read with a positional read into a `vec![0; length]`. A page is hundreds of kilobytes, which is above the size where the allocator goes to the kernel, so each buffer is fresh anonymous memory, and the kernel zeroes it a page at a time as the read touches it and then copies the page cache's bytes over the zeroes. Then the decoder reads the buffer once and it is freed. The bytes were already in memory before any of that started, in the page cache, and all of this work was to put a second copy of them somewhere else for one pass.

## The change

`rudb_io::Mapped` maps a file read only. It is unix only, it maps the file as long as it was when it was opened, and `get(offset, length)` hands back the bytes at that range as a slice, or `None` when the range runs past the mapping. On any other platform, or for an empty file, it is `None` and the caller reads the way it did before.

The native `Catalog` maps its file once when it opens it, next to the file handle it already shares with every reader it hands out, and every `Reader` holds the same mapping. A held page is either bytes it read or a range of the mapping, and a part read without its page is a slice of the mapping when the mapping covers it. The checksum of a part is still checked the first time a reader reads it, the same as before.

The mapping is safe for the native format because of three things the format already does. A committed page is never written again. A rewrite of the whole file goes to a new file that is renamed over the old one, so a reader that has the old one mapped keeps the old one's bytes. Nothing truncates a native file. The one thing in the file that is written in place is the pair of header slots, and those are read with a positional read and never through the mapping. A file cut short by something outside rudb while it is mapped would fault the reader rather than return a short read, which is the same class of failure as the file being changed under a reader at all.

The ring that bounds how many pages a reader keeps still counts a mapped page at its full length. That keeps the eviction order the same as before, and whether a mapped page should count for less is a separate question from this one.

## Results

All 22 answers match the binary before this change at one and at eight threads.

Three copies of each query in one process at one thread over SF1 on server3, the before binary and this one run back to back:

| query | task before | task after | kernel instructions before | after | user instructions before | after |
|---|---|---|---|---|---|---|
| q01 | 1600 ms | 1159 ms | 153 M | 44 M | 3243 M | 3236 M |
| q03 | 692 ms | 611 ms | 106 M | 47 M | 1468 M | 1461 M |
| q05 | 1173 ms | 972 ms | 110 M | 50 M | 1820 M | 1813 M |
| q06 | 373 ms | 237 ms | 61 M | 21 M | 521 M | 515 M |
| q09 | 2160 ms | 1799 ms | 247 M | 145 M | 3633 M | 3623 M |
| q12 | 977 ms | 735 ms | 151 M | 82 M | 1339 M | 1332 M |
| q13 | 1589 ms | 1175 ms | 145 M | 84 M | 2895 M | 2891 M |
| q18 | 789 ms | 635 ms | 112 M | 81 M | 1243 M | 1241 M |
| q21 | 1964 ms | 1671 ms | 130 M | 93 M | 3511 M | 3506 M |

The box was at a load of 25 to 45 the whole time, so the task times are rough and only the direction is worth reading off them. The kernel instruction counts are the part that is not rough, and they went down on every query, by 64 percent on q06 and q01. User instructions went down slightly, from not zeroing and copying into the buffer. Page faults went the same way as the kernel's share: q01 from 10,769 to 1,617 and q06 from 9,021 to 1,121.

## What is left

The kernel still has 21 M to 145 M instructions of each query. What is left is mostly the anonymous memory the decoders and operators allocate for their own output, which is the same zero and fault cost for vectors that are not copies of anything. That is a question about reusing a thread's buffers from one chunk to the next rather than about reading, and it is the next thing to look at from this side.

`read_index` and the dictionary and statistics pages still use a positional read. They are read once per reader, so they are not in the numbers above, but they could take the same path.
