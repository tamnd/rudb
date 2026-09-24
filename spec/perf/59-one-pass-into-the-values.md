# One pass into the values

The shared walk of the runs reads a packed column out into a run of `i64` before it folds it, and the reading was two passes and two vectors where it needed one of each.

`Packed::unpack` leaves codes in a slice of `u64`, which is the right thing for a caller that wants codes. A caller that wants the values then has to walk every row a second time to add the column's base to them, and has to give the first pass somewhere to land, so what a chunk cost was a vector of codes allocated, that vector zeroed before a single code was written into it, the unpack, a second vector allocated, and a pass that loads and stores every row again. In the q01 cycle profile the zeroing alone was 0.84 percent of the query, showing as `memset` under `coded_runs`, and q01 does it three times a chunk over close to three thousand chunks.

`Packed::unpack_mapped` does it once. It unpacks into 64 codes of stack that the next block writes over, puts each one through the caller's function on the way out, and appends the result to a vector it reserved room in once. There is no vector of codes, there is no second pass, and nothing is zeroed anywhere, because a vector grown by appending into reserved capacity never writes a value it does not mean.

Unpacking a block at a time into the stack is the shape `codes_into` tried for random rows and lost with, which is worth knowing before reaching for it again. That loss came from asking which block each row falls in, once per row, and a sequential read never asks.

On server2 at SF1, one thread, three rounds, median, with the `SELECT 1` baseline subtracted, q01 went from 1619.4 M instructions to 1605.5 M and the suite from 16.09 G to 16.08 G. All 22 answers are unchanged and nothing moved the other way past 1.001x. Cycles moved from a median of 859.8 M to 846.0 M over five rounds, which is the right direction and too close to the noise on a shared box to claim as more than that.

The other two `memset` callers in the profile are untouched and both are larger than this one. The scan's page buffer is 1.60 percent of q01 and the projection's output vectors are 0.73, and #1801 having made the system allocator the shell's default means whatever is done about either wants a fresh profile first rather than the numbers in this paragraph.
