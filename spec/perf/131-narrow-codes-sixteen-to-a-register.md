# 131. Narrow packed codes compared sixteen to a register

## What was slow

A filter on a packed column compares its codes with the range in lanes of 32 bits, eight codes to a register, see 108. That is the right lane for a code of up to 25 bits. But the columns TPC-H filters on most are narrower than that. A date is twelve bits, because seven years of days is about 2,500 values, and `l_discount` is four. In q06 the three filter columns were 30 percent of the query in the steady state, and two of the three were spending a 32 bit lane on a code of twelve bits or less.

## The change

A code that fits in the two bytes it starts on now goes into a lane of 16 bits, sixteen codes to a register. That is every width up to nine, and ten, twelve and sixteen, whose codes start on even bits, a nibble and a byte. One shuffle puts each code's two bytes into its lane. AVX2 has no shift of 16 bit lanes by a different count each, so a multiply shifts each code up against the top of its lane, which drops the bits of the code after it, and one shift down by the same count for every lane drops the bits of the code before it. The subtract, unsigned minimum and compare are the same as before on 16 bit lanes. Two registers of answers pack into one of bytes, and one `movemask` gives 32 rows. A block of 64 codes is four groups and two `movemask`s, where it was eight groups and eight.

Codes of 11, 13, 14, 15 and 17 to 25 bits stay on the 32 bit lanes. `l_quantity` is 13 bits and is one of them.

## Measured

At one thread at SF1 on server2, against main at #2634. The server's load average was between 20 and 30. The answers to all 22 queries are the same bytes as before, at one thread and at six.

Twenty runs of q06 in one process, six runs of each binary interleaved:

| binary | instructions (M) | cycles (M) |
| --- | --- | --- |
| main | 1,616 | 1,431 |
| this change | 1,386 | 1,291 |

In the steady state a run of q06 went from 66 M to 54 M instructions. The other queries that filter on a date moved by 3 to 7 percent of their instructions: q12 from 114 M to 109 M, q14 from 67 M to 63 M, q15 from 69 M to 64 M, q20 from 138 M to 134 M and q01 from 191 M to 186 M.
