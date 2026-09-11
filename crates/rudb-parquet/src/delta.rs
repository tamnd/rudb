//! The delta encodings and byte stream split.
//!
//! Four encodings that pyarrow, parquet-java and parquet-cpp all emit and DuckDB does not, which
//! is why they are here in their own change and why the fixtures for them are written rather than
//! committed. A reader that only reads what DuckDB writes is not a Parquet reader.
//!
//! # Delta binary packed
//!
//! Integers as differences from the one before, in blocks. Each block states the smallest
//! difference in it and then packs every difference as its distance above that minimum, which is
//! what turns a run of increasing values into a stream of near zeroes and a sorted column into
//! almost nothing. The block is cut into miniblocks with their own bit widths, so a block that is
//! mostly flat with one jump in it does not pay for the jump across the whole block.
//!
//! Two things about it are easy to get wrong. The minimum is signed and is usually negative for a
//! decreasing column, so it is added rather than subtracted and the addition wraps. And the value
//! count in the header counts the first value, which is in the header rather than in any block, so
//! the blocks hold one fewer than the header says.
//!
//! # Delta length byte array and delta byte array
//!
//! The first writes the lengths with the encoding above and then every string's bytes end to end,
//! which means the bytes are contiguous in the page and the strings can stay there.
//!
//! The second adds a prefix length per value, so each string continues the one before it. Those
//! strings do not exist anywhere in the page as whole strings, so they are built, which is the one
//! place in this reader where a string column's bytes are not the page's bytes.
//!
//! # Byte stream split
//!
//! A float's four bytes go into four separate streams, all the first bytes then all the second
//! bytes and so on. Nothing is compressed by it directly and that is the point: the exponent bytes
//! of a column of similar magnitudes are nearly constant once they sit together, so the codec that
//! runs afterwards has something to find. Reading it is a transpose.

use rudb_common::{Error, Result};

use crate::hybrid::unpack;

/// Reads a delta binary packed stream of `count` values.
///
/// Returns the values and how many bytes of `bytes` they took, which the byte array encodings need
/// because their payload starts where their lengths end.
///
/// # Errors
///
/// If a header or a block runs off the end of the page, or if the stream holds fewer values than
/// were asked for.
pub(crate) fn binary_packed(bytes: &[u8], count: usize) -> Result<(Vec<i64>, usize)> {
    let mut at = 0;
    let block = varint(bytes, &mut at)? as usize;
    let miniblocks = varint(bytes, &mut at)? as usize;
    let stated = varint(bytes, &mut at)? as usize;
    let first = zigzag(varint(bytes, &mut at)?);
    if block == 0 || miniblocks == 0 || block % miniblocks != 0 {
        return Err(Error::io(format!(
            "a delta header of {block} values in {miniblocks} miniblocks, which do not divide"
        )));
    }
    let per_miniblock = block / miniblocks;
    if per_miniblock % 8 != 0 {
        // The packing works in groups of eight, so a miniblock that is not a multiple of eight
        // values has no defined length in bytes. Every writer emits 32, and the specification
        // requires the multiple.
        return Err(Error::io(format!(
            "a delta miniblock of {per_miniblock} values, which is not a multiple of eight"
        )));
    }
    if stated < count {
        return Err(Error::io(format!(
            "a delta stream of {stated} values where {count} were wanted"
        )));
    }
    let mut out = Vec::with_capacity(count);
    if count > 0 {
        out.push(first);
    }
    let mut value = first;
    // The first value came out of the header, so the blocks hold one fewer than the count.
    let mut left = stated.saturating_sub(1);
    while left > 0 && out.len() < count {
        let min = zigzag(varint(bytes, &mut at)?);
        let widths = bytes.get(at..at + miniblocks).ok_or_else(|| {
            Error::io(format!(
                "a delta block wanting {miniblocks} bit widths at {at} in {} bytes",
                bytes.len()
            ))
        })?;
        let widths = widths.to_vec();
        at += miniblocks;
        for &width in &widths {
            if left == 0 {
                break;
            }
            if width > 64 {
                return Err(Error::io(format!(
                    "a delta miniblock of {width} bits, which is wider than the values"
                )));
            }
            let size = per_miniblock * usize::from(width) / 8;
            let body = bytes.get(at..at + size).ok_or_else(|| {
                Error::io(format!(
                    "a delta miniblock of {size} bytes at {at} in {} bytes",
                    bytes.len()
                ))
            })?;
            // A miniblock always holds its full complement even at the end of the stream, and the
            // values past the count are padding. Taking only what is left is what stops the
            // padding turning into rows.
            let take = per_miniblock.min(left);
            let mut deltas = Vec::with_capacity(take);
            unpack(body, width, 0, take, |delta| deltas.push(delta));
            for delta in deltas {
                // Wrapping because the minimum is signed and the values are allowed to be anywhere
                // in the range. A column counting down has a negative minimum, and a column that
                // spans the whole range of the type has a difference that does not fit in it.
                value = value.wrapping_add(min).wrapping_add(delta as i64);
                if out.len() < count {
                    out.push(value);
                }
            }
            at += size;
            left -= take;
        }
    }
    if out.len() < count {
        return Err(Error::io(format!(
            "a delta stream that ran out after {} of {count} values",
            out.len()
        )));
    }
    Ok((out, at))
}

/// Reads a delta length byte array stream, returning where each string is inside `bytes`.
///
/// The spans are offsets into `bytes` rather than copies, because the strings are contiguous in the
/// page and can stay in it.
///
/// # Errors
///
/// If the lengths do not decode, if a length is negative, or if the strings run off the end.
pub(crate) fn length_byte_array(bytes: &[u8], count: usize) -> Result<Vec<(usize, usize)>> {
    let (lengths, mut at) = binary_packed(bytes, count)?;
    let mut spans = Vec::with_capacity(count);
    for length in lengths {
        let len = usize::try_from(length)
            .map_err(|_| Error::io(format!("a byte array of {length} bytes")))?;
        let end = at
            .checked_add(len)
            .ok_or_else(|| Error::io("a byte array past the end of memory".to_string()))?;
        if end > bytes.len() {
            return Err(Error::io(format!(
                "a byte array at {at} of {len} bytes in a page of {} bytes",
                bytes.len()
            )));
        }
        spans.push((at, len));
        at = end;
    }
    Ok(spans)
}

/// Reads a delta byte array stream, building each string from its prefix and its suffix.
///
/// The one encoding whose strings are not in the page. A value that shares nine characters with the
/// one before it is stored as those nine characters not at all, so there is nowhere in the file the
/// whole thing exists and it has to be put together somewhere.
///
/// # Errors
///
/// If either length stream does not decode, if a prefix is longer than the value before it, or if
/// the suffixes run off the end.
pub(crate) fn byte_array(bytes: &[u8], count: usize) -> Result<Vec<Vec<u8>>> {
    let (prefixes, used) = binary_packed(bytes, count)?;
    let rest = &bytes[used..];
    let spans = length_byte_array(rest, count)?;
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(count);
    for (at, (prefix, &(start, len))) in prefixes.iter().zip(spans.iter()).enumerate() {
        let prefix = usize::try_from(*prefix)
            .map_err(|_| Error::io(format!("a shared prefix of {prefix} bytes")))?;
        let before = if at == 0 { &[][..] } else { &out[at - 1] };
        if prefix > before.len() {
            return Err(Error::io(format!(
                "a value sharing {prefix} bytes with one that is {} bytes long",
                before.len()
            )));
        }
        let mut value = Vec::with_capacity(prefix + len);
        value.extend_from_slice(&before[..prefix]);
        value.extend_from_slice(&rest[start..start + len]);
        out.push(value);
    }
    Ok(out)
}

/// Reads a byte stream split column of `count` values of `width` bytes each.
///
/// # Errors
///
/// If the streams are not all there.
pub(crate) fn stream_split(bytes: &[u8], width: usize, count: usize) -> Result<Vec<u8>> {
    let wanted = width
        .checked_mul(count)
        .ok_or_else(|| Error::io("a byte stream split past the end of memory".to_string()))?;
    if bytes.len() < wanted {
        return Err(Error::io(format!(
            "a byte stream split wanting {wanted} bytes with {} in the page",
            bytes.len()
        )));
    }
    let mut out = vec![0u8; wanted];
    for stream in 0..width {
        let from = &bytes[stream * count..(stream + 1) * count];
        for (value, &byte) in from.iter().enumerate() {
            out[value * width + stream] = byte;
        }
    }
    Ok(out)
}

/// A little endian base 128 varint, advancing `at` past it.
fn varint(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*at).ok_or_else(|| {
            Error::io("a delta header that ran off the end of the page".to_string())
        })?;
        *at += 1;
        if shift >= 64 {
            return Err(Error::io("a delta header longer than 64 bits".to_string()));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

/// Undoes the zigzag that makes a small negative number a small unsigned one.
fn zigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rudb_common::Value;
    use rudb_io::{File, Filesystem, OpenMode, RealFilesystem};
    use rudb_vector::Vector;

    use super::{binary_packed, stream_split, zigzag};
    use crate::metadata::Metadata;
    use crate::page::Body;
    use crate::{Pages, SchemaColumn};

    /// A delta binary packed writer, for the cases pyarrow does not produce.
    ///
    /// It sits here rather than in a fixture because the shapes worth testing are the awkward ones,
    /// a stream of one value, a block whose miniblocks are not all used, a column that only
    /// decreases, and committing a file for each is more bytes and less clarity than the twenty
    /// lines that write them.
    #[derive(Debug)]
    struct Writer {
        bytes: Vec<u8>,
    }

    impl Writer {
        /// Writes `values` with a block of `block` values in `miniblocks` miniblocks.
        fn write(values: &[i64], block: usize, miniblocks: usize) -> Vec<u8> {
            let mut out = Writer { bytes: Vec::new() };
            out.varint(block as u64);
            out.varint(miniblocks as u64);
            out.varint(values.len() as u64);
            out.varint(Self::zigzag(values.first().copied().unwrap_or(0)));
            let per = block / miniblocks;
            let deltas: Vec<i64> =
                values.windows(2).map(|pair| pair[1].wrapping_sub(pair[0])).collect();
            for chunk in deltas.chunks(block) {
                let min = chunk.iter().copied().min().unwrap_or(0);
                out.varint(Self::zigzag(min));
                let groups: Vec<Vec<u64>> = chunk
                    .chunks(per)
                    .map(|part| {
                        part.iter().map(|&d| d.wrapping_sub(min) as u64).collect::<Vec<u64>>()
                    })
                    .collect();
                let widths: Vec<u8> = (0..miniblocks)
                    .map(|at| match groups.get(at) {
                        Some(part) => {
                            let max = part.iter().copied().max().unwrap_or(0);
                            if max == 0 { 0 } else { (64 - max.leading_zeros()) as u8 }
                        }
                        None => 0,
                    })
                    .collect();
                out.bytes.extend_from_slice(&widths);
                for (at, &width) in widths.iter().enumerate() {
                    let Some(part) = groups.get(at) else { continue };
                    out.pack(part, width, per);
                }
            }
            out.bytes
        }

        /// Packs `per` values of `width` bits, padding the group out with zeroes.
        fn pack(&mut self, values: &[u64], width: u8, per: usize) {
            if width == 0 {
                return;
            }
            let mut bits = vec![0u8; per * usize::from(width) / 8];
            for (at, &value) in values.iter().enumerate() {
                for bit in 0..usize::from(width) {
                    if value >> bit & 1 == 1 {
                        let put = at * usize::from(width) + bit;
                        bits[put / 8] |= 1 << (put % 8);
                    }
                }
            }
            self.bytes.extend_from_slice(&bits);
        }

        fn varint(&mut self, mut value: u64) {
            loop {
                let byte = (value & 0x7f) as u8;
                value >>= 7;
                if value == 0 {
                    self.bytes.push(byte);
                    return;
                }
                self.bytes.push(byte | 0x80);
            }
        }

        fn zigzag(value: i64) -> u64 {
            ((value << 1) ^ (value >> 63)) as u64
        }
    }

    fn roundtrip(values: &[i64], block: usize, miniblocks: usize) {
        let bytes = Writer::write(values, block, miniblocks);
        let (read, used) = binary_packed(&bytes, values.len()).expect("the stream decodes");
        assert_eq!(read, values, "block {block} in {miniblocks}");
        assert_eq!(used, bytes.len(), "the reader stopped in the wrong place");
    }

    #[test]
    fn a_run_that_only_climbs_comes_back_as_it_went_in() {
        roundtrip(&(0..1000).map(|i| i * 7).collect::<Vec<i64>>(), 128, 4);
    }

    #[test]
    fn a_run_that_only_falls_comes_back_as_it_went_in() {
        // The case a reader that subtracted the block minimum instead of adding it gets wrong, and
        // it gets it wrong quietly, because the values it produces are the right shape.
        roundtrip(&(0..1000).map(|i| 5_000_000 - i * 13).collect::<Vec<i64>>(), 128, 4);
    }

    #[test]
    fn a_flat_run_packs_to_nothing_and_reads_back_whole() {
        // Every delta is the block minimum, so every miniblock is zero bits wide and occupies no
        // bytes at all. A reader that computed a width of one here reads the next block's bytes.
        roundtrip(&vec![42i64; 500], 128, 4);
    }

    #[test]
    fn a_single_value_lives_entirely_in_the_header() {
        roundtrip(&[-7i64], 128, 4);
    }

    #[test]
    fn a_run_that_spans_the_whole_range_of_the_type_wraps_rather_than_panics() {
        // The differences here do not fit in an i64, which the format allows and handles by saying
        // the arithmetic wraps. A reader doing checked arithmetic refuses a file that is fine.
        roundtrip(&[i64::MIN, i64::MAX, i64::MIN, 0, i64::MAX], 128, 4);
    }

    #[test]
    fn the_block_and_miniblock_sizes_writers_actually_use_all_work() {
        let values: Vec<i64> = (0..600).map(|i| i * i - 300).collect();
        for (block, miniblocks) in [(128, 4), (256, 8), (1024, 32), (128, 1)] {
            roundtrip(&values, block, miniblocks);
        }
    }

    /// A header and nothing after it, for the shapes the writer above will not produce.
    fn header(block: usize, miniblocks: usize, count: usize) -> Vec<u8> {
        let mut out = Writer { bytes: Vec::new() };
        out.varint(block as u64);
        out.varint(miniblocks as u64);
        out.varint(count as u64);
        out.varint(Writer::zigzag(0));
        out.bytes
    }

    #[test]
    fn a_header_whose_blocks_do_not_divide_is_refused() {
        let error = binary_packed(&header(128, 3, 3), 3).unwrap_err();
        assert!(error.message().contains("do not divide"), "{}", error.message());
    }

    #[test]
    fn a_header_with_no_blocks_in_it_is_refused_rather_than_dividing_by_zero() {
        for (block, miniblocks) in [(0, 4), (128, 0)] {
            let error = binary_packed(&header(block, miniblocks, 3), 3).unwrap_err();
            assert!(error.message().contains("do not divide"), "{}", error.message());
        }
    }

    #[test]
    fn a_miniblock_that_is_not_a_multiple_of_eight_values_is_refused() {
        // Two values per miniblock has no length in bytes, because the packing works in groups of
        // eight. Every writer emits 32.
        let error = binary_packed(&header(128, 64, 3), 3).unwrap_err();
        assert!(error.message().contains("multiple of eight"), "{}", error.message());
    }

    #[test]
    fn every_prefix_of_a_stream_is_an_error_rather_than_a_panic() {
        let bytes = Writer::write(&(0..300).map(|i| i * 5).collect::<Vec<i64>>(), 128, 4);
        for cut in 0..bytes.len() {
            let _ = binary_packed(&bytes[..cut], 300);
        }
    }

    #[test]
    fn a_stream_asked_for_more_than_it_holds_says_so() {
        let bytes = Writer::write(&[1, 2, 3], 128, 4);
        let error = binary_packed(&bytes, 9).unwrap_err();
        assert!(error.message().contains("where 9 were wanted"), "{}", error.message());
    }

    #[test]
    fn zigzag_undoes_itself_around_zero_and_at_the_edges() {
        for value in [0i64, -1, 1, -2, 2, i64::MIN, i64::MAX, -1_000_000, 1_000_000] {
            assert_eq!(zigzag(Writer::zigzag(value)), value, "{value}");
        }
    }

    #[test]
    fn a_byte_stream_split_is_a_transpose_and_nothing_else() {
        // Four values of four bytes, written as all the first bytes, then all the second, and so
        // on. Writing the expected answer out rather than computing it is the point: a transpose
        // computed the same way in the test and the code agrees with itself either way round.
        let bytes: Vec<u8> = vec![
            0, 4, 8, 12, // every value's first byte
            1, 5, 9, 13, // every value's second
            2, 6, 10, 14, //
            3, 7, 11, 15,
        ];
        let flat = stream_split(&bytes, 4, 4).expect("the split reads");
        assert_eq!(flat, (0..16).collect::<Vec<u8>>());
    }

    #[test]
    fn a_byte_stream_split_that_is_short_is_an_error() {
        let error = stream_split(&[0, 1, 2], 4, 4).unwrap_err();
        assert!(error.message().contains("wanting 16 bytes"), "{}", error.message());
    }

    /// The file pyarrow wrote, in the four encodings DuckDB does not write.
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
    }

    fn open(name: &str) -> Box<dyn File> {
        RealFilesystem::new()
            .open(&fixture(name), OpenMode::Read)
            .expect("the fixture is committed")
    }

    /// Every value of one column of a file, read the way a scan would read it.
    fn column(name: &str, at: usize) -> (SchemaColumn, Vec<Value>) {
        let file = open(name);
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let schema = metadata.schema[at].clone();
        let mut out = Vec::new();
        for group in &metadata.row_groups {
            let chunk = &group.columns[at];
            let mut bytes = vec![0u8; chunk.compressed_size as usize];
            file.read_at(chunk.start(), &mut bytes).expect("the chunk is in the file");
            let mut dictionary = None;
            for page in Pages::new(&bytes, chunk.compression, chunk.values) {
                let page = page.expect("every page of the fixture walks");
                if matches!(page.header.body, Body::Dictionary(_)) {
                    dictionary =
                        Some(page.into_dictionary(&schema).expect("the dictionary decodes"));
                    continue;
                }
                let vector =
                    page.into_vector(&schema, dictionary.as_ref()).expect("the values decode");
                out.extend(Vector::iter(&vector));
            }
        }
        (schema, out)
    }

    #[test]
    fn a_delta_packed_integer_column_from_pyarrow_matches_duckdb() {
        // The numbers are DuckDB reading the same file on server2. Two readers that disagree about
        // an encoding neither of their writers produced is exactly the failure the other-writers
        // corpus is for.
        let read: Vec<i64> = column("delta.parquet", 0)
            .1
            .into_iter()
            .map(|value| match value {
                Value::Integer(number) => i64::from(number),
                other => panic!("an integer column produced {other:?}"),
            })
            .collect();
        assert_eq!(read.len(), 4096);
        assert_eq!(read.iter().sum::<i64>(), 23_111_680);
        assert_eq!(read.iter().copied().min(), Some(-500));
        assert_eq!(read.iter().copied().max(), Some(11785));
    }

    #[test]
    fn a_delta_packed_column_that_only_falls_matches_duckdb() {
        // Written descending on purpose, so the block minimum is negative throughout.
        let read: Vec<i64> = column("delta.parquet", 1)
            .1
            .into_iter()
            .map(|value| match value {
                Value::BigInt(number) => number,
                other => panic!("a big integer column produced {other:?}"),
            })
            .collect();
        assert_eq!(read.len(), 4096);
        assert_eq!(i128::from(read.iter().sum::<i64>()), 4_095_999_941_294_080);
    }

    #[test]
    fn a_delta_byte_array_column_from_pyarrow_matches_duckdb() {
        // Every value shares a prefix with the one before it, which is the case this encoding is
        // for and the one where the strings do not exist in the page as whole strings.
        let read = column("delta.parquet", 2).1;
        assert_eq!(read.len(), 4096);
        let text: Vec<String> = read
            .iter()
            .filter_map(|value| match value {
                Value::Varchar(text) => Some(text.clone()),
                Value::Null => None,
                other => panic!("a string column produced {other:?}"),
            })
            .collect();
        assert_eq!(text.len(), 3640);
        assert_eq!(text.iter().min().map(String::as_str), Some("prefix_00000"));
        assert_eq!(text.iter().max().map(String::as_str), Some("prefix_01023"));
    }

    #[test]
    fn a_byte_stream_split_column_from_pyarrow_matches_duckdb() {
        let read: Vec<f64> = column("delta.parquet", 3)
            .1
            .into_iter()
            .map(|value| match value {
                Value::Double(number) => number,
                other => panic!("a double column produced {other:?}"),
            })
            .collect();
        assert_eq!(read.len(), 4096);
        assert!((read.iter().sum::<f64>() - 4_193_280.0).abs() < 1e-6);
    }

    #[test]
    fn a_delta_length_byte_array_column_from_pyarrow_matches_duckdb() {
        // Same values as the delta byte array column, written the other way, so the two answers
        // have to agree with each other as well as with DuckDB.
        let read = column("lengths.parquet", 0).1;
        assert_eq!(read.len(), 4096);
        let text: Vec<String> = read
            .iter()
            .filter_map(|value| match value {
                Value::Varchar(text) => Some(text.clone()),
                Value::Null => None,
                other => panic!("a string column produced {other:?}"),
            })
            .collect();
        assert_eq!(text.len(), 3640);
        assert_eq!(text.iter().min().map(String::as_str), Some("prefix_00000"));
        assert_eq!(text.iter().max().map(String::as_str), Some("prefix_01023"));
        assert_eq!(read, column("delta.parquet", 2).1, "the two encodings disagree");
    }

    #[test]
    fn the_nulls_in_a_delta_column_land_where_pyarrow_put_them() {
        // pyarrow wrote a null at every ninth row. Checking the positions rather than the count is
        // what catches a reader that decoded the values densely and never spread them.
        let read = column("delta.parquet", 2).1;
        for (at, value) in read.iter().enumerate() {
            assert_eq!(at % 9 == 0, matches!(value, Value::Null), "row {at} is {value:?}");
        }
    }
}
