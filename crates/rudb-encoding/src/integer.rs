//! The single column integer encodings and the cascade over them.
//!
//! `spec/06-compression.md` section 6.2 lists the encoding set and section 6.3 says the ratios are
//! in the cascade rather than in any one encoding. This module is both: the six candidate shapes
//! for an integer column, each of which encodes its own output by calling back into the chooser, so
//! that RLE over a dictionary over a bit packed code array is a thing that happens by construction
//! rather than a case somebody wrote out.
//!
//! Everything here works on `i64`. A narrower column is widened on the way in and nothing is lost
//! by it, because every encoding's size comes from the range of the values rather than from the
//! declared width of the type: a `SMALLINT` column of values 100 to 130 packs to 5 bits whether it
//! arrived as `i16` or as `i64`. The one place the widening would cost something is a raw copy, and
//! there is no raw copy, because a bit packed unit at width 64 is exactly that and the chooser
//! reaches it on its own when nothing else fits.
//!
//! ## The unit
//!
//! Bit packing is per 1024 values, per [`crate::bitpack`]. Everything else is per chunk, where a
//! chunk is however many values the caller passes in and is meant to be a row group. The two
//! granularities are the point rather than an accident. A frame of reference base that is chosen
//! per 1024 values tracks a column that drifts, which is what a timestamp column and an
//! autoincrementing key both do, and one base per row group would pay the whole range of the row
//! group on every value. A dictionary, on the other hand, is worth more the larger the unit it
//! covers, which is the argument section 6.5 takes all the way to a dictionary per table.
//!
//! ## The serialized form
//!
//! A chunk is a tag byte, a value count, and a body whose shape depends on the tag. Bodies that
//! contain another array of integers contain a whole chunk, tag and all, which is what makes the
//! decoder a fold and what makes the cascade free: nothing in `Rle` knows what its run lengths are
//! encoded as. The header is fixed width little endian rather than a varint, because 5 bytes per
//! chunk against a chunk that holds a row group is not worth the branch on the decode path.
//!
//! ## What the chooser does, and what it will have to do instead
//!
//! It encodes every candidate and keeps the smallest. That is the honest baseline for M1, which is
//! a measurement of what the format can do rather than of how fast a writer can decide, and it is
//! not what a write path can afford. Section 6.3 describes the real thing: evaluate the candidates
//! on a systematic sample, not the first N rows, because column data is frequently clustered and
//! the first 1024 rows of a sorted column look constant. Building that first would mean the numbers
//! this milestone produces are the sampler's numbers rather than the format's, and there would be
//! no way to tell how much the sampler is leaving behind.

use rudb_common::{Error, Result};

use crate::bitpack::{self, VALUES};

/// How deep a cascade is allowed to go.
///
/// Three levels is what section 6.3 says captures most of what a general compressor would find:
/// dictionary, then bit packed codes, then nothing left worth doing. The limit exists because the
/// chooser is exhaustive and a cascade that could nest forever would be exponential, and because a
/// fourth level has never once been the smallest candidate in anything measured so far.
const MAX_DEPTH: u8 = 3;

/// What a chunk is encoded as. The discriminant is the tag byte in the serialized form and is part
/// of the format, so the numbers are written down rather than left to the compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One value repeated. The whole chunk is the tag, the count and the value.
    Constant = 0,
    /// Frame of reference then bit packed, per 1024 values. Covers plain bit packing at base zero
    /// and a raw copy at width 64.
    Packed = 1,
    /// Differences between neighbours, zigzagged so a decreasing column is as cheap as an
    /// increasing one, then encoded as a chunk in its own right.
    Delta = 2,
    /// Run values and run lengths, each encoded as a chunk in its own right.
    Rle = 3,
    /// A dictionary of the distinct values and an array of codes into it, both encoded as chunks in
    /// their own right.
    Dict = 4,
    /// One dominant value with an exception list of positions and values.
    Sparse = 5,
}

impl Kind {
    fn tag(self) -> u8 {
        self as u8
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Constant),
            1 => Ok(Self::Packed),
            2 => Ok(Self::Delta),
            3 => Ok(Self::Rle),
            4 => Ok(Self::Dict),
            5 => Ok(Self::Sparse),
            other => Err(Error::internal(format!("unknown encoding tag {other}"))),
        }
    }

    /// The name that goes in a report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Constant => "CONSTANT",
            Self::Packed => "FOR+BITPACK",
            Self::Delta => "DELTA",
            Self::Rle => "RLE",
            Self::Dict => "DICT",
            Self::Sparse => "SPARSE",
        }
    }
}

/// Encodes a chunk of integers, choosing the cascade that comes out smallest.
///
/// # Errors
///
/// If the chunk is longer than `u32::MAX`, or if an encoding produces something its own decoder
/// would not accept, which is an internal inconsistency rather than a caller error.
pub fn encode(values: &[i64]) -> Result<Vec<u8>> {
    encode_at(values, 0)
}

/// Decodes a chunk written by [`encode`].
///
/// # Errors
///
/// If the bytes are truncated, carry an unknown tag, or describe a chunk whose parts do not agree
/// with each other.
pub fn decode(bytes: &[u8]) -> Result<Vec<i64>> {
    let mut reader = Reader::new(bytes);
    let values = decode_chunk(&mut reader)?;
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after decoding a chunk",
            reader.remaining()
        )));
    }
    Ok(values)
}

/// The size in bytes of every candidate, for a report that wants to say what the cascade was
/// chosen over rather than only what it chose. A candidate that does not apply is absent.
///
/// # Errors
///
/// As [`encode`].
pub fn candidate_sizes(values: &[i64]) -> Result<Vec<(Kind, usize)>> {
    let mut sizes = Vec::new();
    for kind in candidates(values, 0) {
        if let Some(bytes) = encode_as(kind, values, 0)? {
            sizes.push((kind, bytes.len()));
        }
    }
    Ok(sizes)
}

/// The cascade a chunk was encoded as, as a line of text like `DICT(PACKED, PACKED)`.
///
/// # Errors
///
/// As [`decode`].
pub fn describe(bytes: &[u8]) -> Result<String> {
    let mut reader = Reader::new(bytes);
    describe_chunk(&mut reader)
}

fn encode_at(values: &[i64], depth: u8) -> Result<Vec<u8>> {
    let mut best: Option<Vec<u8>> = None;
    for kind in candidates(values, depth) {
        let Some(bytes) = encode_as(kind, values, depth)? else {
            continue;
        };
        if best.as_ref().is_none_or(|current| bytes.len() < current.len()) {
            best = Some(bytes);
        }
    }
    // `Packed` applies to every input including the empty one, so the chooser always has at least
    // one candidate and this cannot be reached without a bug in `candidates`.
    best.ok_or_else(|| Error::internal("no encoding applied to the chunk"))
}

/// Which candidates are worth encoding for this input.
///
/// The filters here are not the cost model. They are the cases where the encoding cannot be
/// expressed at all, or is provably larger than `Packed` on the same data, so that the exhaustive
/// chooser does not spend a dictionary build on a column of 100,000 distinct values to discover
/// what its distinct count already said.
fn candidates(values: &[i64], depth: u8) -> Vec<Kind> {
    let mut kinds = vec![Kind::Packed];
    if depth >= MAX_DEPTH || values.is_empty() {
        return kinds;
    }
    if values.iter().all(|value| *value == values[0]) {
        // Nothing else can beat 13 bytes, so this is the whole answer rather than a candidate.
        return vec![Kind::Constant];
    }
    if values.len() >= 2 && deltas(values).is_some() {
        kinds.push(Kind::Delta);
    }
    if run_count(values) * 4 <= values.len() * 3 {
        kinds.push(Kind::Rle);
    }
    let distinct = distinct_values(values);
    if distinct.len() * 2 <= values.len() {
        kinds.push(Kind::Dict);
    }
    // Written as a match rather than as a chained `if let` because the minimum supported Rust
    // version is 1.85 and let chains landed in 1.88.
    match dominant_value(values) {
        Some((_, count)) if count * 10 >= values.len() * 8 => kinds.push(Kind::Sparse),
        _ => {}
    }
    kinds
}

/// `None` when the encoding does not apply to this input, which the caller treats as a candidate
/// that did not run rather than as a failure.
fn encode_as(kind: Kind, values: &[i64], depth: u8) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    put_u8(&mut out, kind.tag());
    put_u32(&mut out, u32::try_from(values.len()).map_err(|_| too_long(values.len()))?);
    match kind {
        Kind::Constant => {
            let Some(first) = values.first() else {
                return Ok(None);
            };
            if values.iter().any(|value| value != first) {
                return Ok(None);
            }
            put_i64(&mut out, *first);
        }
        Kind::Packed => encode_packed(values, &mut out)?,
        Kind::Delta => {
            let Some(deltas) = deltas(values) else {
                return Ok(None);
            };
            put_i64(&mut out, values[0]);
            out.extend_from_slice(&encode_at(&deltas, depth + 1)?);
        }
        Kind::Rle => {
            let (run_values, run_lengths) = runs(values);
            if run_values.is_empty() {
                return Ok(None);
            }
            out.extend_from_slice(&encode_at(&run_values, depth + 1)?);
            out.extend_from_slice(&encode_at(&run_lengths, depth + 1)?);
        }
        Kind::Dict => {
            let dictionary = distinct_values(values);
            if dictionary.is_empty() {
                return Ok(None);
            }
            let codes = codes_over(values, &dictionary);
            out.extend_from_slice(&encode_at(&dictionary, depth + 1)?);
            out.extend_from_slice(&encode_at(&codes, depth + 1)?);
        }
        Kind::Sparse => {
            let Some((value, _)) = dominant_value(values) else {
                return Ok(None);
            };
            let mut positions = Vec::new();
            let mut exceptions = Vec::new();
            for (index, other) in values.iter().enumerate() {
                if *other != value {
                    positions.push(index as i64);
                    exceptions.push(*other);
                }
            }
            put_i64(&mut out, value);
            put_u32(
                &mut out,
                u32::try_from(positions.len()).map_err(|_| too_long(positions.len()))?,
            );
            out.extend_from_slice(&encode_at(&positions, depth + 1)?);
            out.extend_from_slice(&encode_at(&exceptions, depth + 1)?);
        }
    }
    Ok(Some(out))
}

/// Frame of reference and bit packing, one base and one width per 1024 values.
///
/// A base per unit rather than per chunk is most of what makes this work on real columns. A
/// timestamp column over a day drifts across a range that needs 47 bits, and the same column inside
/// any one unit spans a few seconds and needs 12. One base per row group would pay the 47 on every
/// value.
///
/// A unit shorter than 1024 values, which is the last one of any chunk whose length is not a
/// multiple of the unit and is the only one of every short array in a cascade, goes through
/// [`bitpack::pack_tail`] instead. The transposed layout has no partial form and would charge a
/// five entry dictionary for 1024 entries.
fn encode_packed(values: &[i64], out: &mut Vec<u8>) -> Result<()> {
    for unit in values.chunks(VALUES) {
        let base = unit.iter().copied().min().unwrap_or(0);
        let offsets: Vec<u64> = unit.iter().map(|value| offset_from(*value, base)).collect();
        let width = bitpack::required_width(&offsets);
        put_i64(out, base);
        put_u8(out, u8::try_from(width).map_err(|_| Error::internal("impossible width"))?);
        if unit.len() == VALUES {
            let mut packed = vec![0u64; bitpack::packed_len::<u64>(width)];
            bitpack::pack(&offsets, width, &mut packed)?;
            for word in packed {
                put_u64(out, word);
            }
        } else {
            bitpack::pack_tail(&offsets, width, out)?;
        }
    }
    Ok(())
}

fn decode_chunk(reader: &mut Reader<'_>) -> Result<Vec<i64>> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    match kind {
        Kind::Constant => Ok(vec![reader.i64()?; count]),
        Kind::Packed => {
            let mut values = Vec::with_capacity(count);
            while values.len() < count {
                let base = reader.i64()?;
                let width = reader.u8()? as usize;
                let wanted = (count - values.len()).min(VALUES);
                if wanted == VALUES {
                    let mut packed = vec![0u64; bitpack::packed_len::<u64>(width)];
                    for word in &mut packed {
                        *word = reader.u64()?;
                    }
                    let mut unit = vec![0u64; VALUES];
                    bitpack::unpack(&packed, width, &mut unit)?;
                    values.extend(unit.iter().map(|offset| value_from(*offset, base)));
                } else {
                    let bytes = reader.bytes(bitpack::tail_len(wanted, width))?;
                    let unit = bitpack::unpack_tail(bytes, width, wanted)?;
                    values.extend(unit.iter().map(|offset| value_from(*offset, base)));
                }
            }
            Ok(values)
        }
        Kind::Delta => {
            let first = reader.i64()?;
            let deltas = decode_chunk(reader)?;
            let mut values = Vec::with_capacity(count);
            values.push(first);
            let mut current = first;
            for delta in deltas {
                current = current.wrapping_add(unzigzag(delta as u64));
                values.push(current);
            }
            check_count(values.len(), count)?;
            Ok(values)
        }
        Kind::Rle => {
            let run_values = decode_chunk(reader)?;
            let run_lengths = decode_chunk(reader)?;
            if run_values.len() != run_lengths.len() {
                return Err(Error::internal("an RLE chunk has more runs than run lengths"));
            }
            let mut values = Vec::with_capacity(count);
            for (value, length) in run_values.into_iter().zip(run_lengths) {
                let length = usize::try_from(length)
                    .map_err(|_| Error::internal("a negative RLE run length"))?;
                values.extend(std::iter::repeat_n(value, length));
            }
            check_count(values.len(), count)?;
            Ok(values)
        }
        Kind::Dict => {
            let dictionary = decode_chunk(reader)?;
            let codes = decode_chunk(reader)?;
            let mut values = Vec::with_capacity(count);
            for code in codes {
                let index =
                    usize::try_from(code).ok().and_then(|index| dictionary.get(index)).ok_or_else(
                        || Error::internal(format!("code {code} is not in the dictionary")),
                    )?;
                values.push(*index);
            }
            check_count(values.len(), count)?;
            Ok(values)
        }
        Kind::Sparse => {
            let value = reader.i64()?;
            let exception_count = reader.u32()? as usize;
            let positions = decode_chunk(reader)?;
            let exceptions = decode_chunk(reader)?;
            if positions.len() != exception_count || exceptions.len() != exception_count {
                return Err(Error::internal("a sparse chunk disagrees about its exception count"));
            }
            let mut values = vec![value; count];
            for (position, exception) in positions.into_iter().zip(exceptions) {
                let position = usize::try_from(position)
                    .ok()
                    .filter(|position| *position < count)
                    .ok_or_else(|| {
                        Error::internal(format!("exception at {position} is outside the chunk"))
                    })?;
                values[position] = exception;
            }
            Ok(values)
        }
    }
}

fn describe_chunk(reader: &mut Reader<'_>) -> Result<String> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    Ok(match kind {
        Kind::Constant => {
            reader.i64()?;
            "CONSTANT".to_string()
        }
        Kind::Packed => {
            let mut widths = Vec::new();
            let mut seen = 0;
            while seen < count {
                reader.i64()?;
                let width = reader.u8()? as usize;
                let wanted = (count - seen).min(VALUES);
                if wanted == VALUES {
                    for _ in 0..bitpack::packed_len::<u64>(width) {
                        reader.u64()?;
                    }
                } else {
                    reader.bytes(bitpack::tail_len(wanted, width))?;
                }
                widths.push(width);
                seen += wanted;
            }
            let low = widths.iter().copied().min().unwrap_or(0);
            let high = widths.iter().copied().max().unwrap_or(0);
            // Square brackets rather than round ones, so that a reader and a test can both take a
            // parenthesis to mean one more level of cascade and nothing else.
            if low == high {
                format!("FOR+BITPACK[{low}]")
            } else {
                format!("FOR+BITPACK[{low}..{high}]")
            }
        }
        Kind::Delta => {
            reader.i64()?;
            format!("DELTA({})", describe_chunk(reader)?)
        }
        Kind::Rle => {
            let values = describe_chunk(reader)?;
            let lengths = describe_chunk(reader)?;
            format!("RLE({values}, {lengths})")
        }
        Kind::Dict => {
            let dictionary = describe_chunk(reader)?;
            let codes = describe_chunk(reader)?;
            format!("DICT({dictionary}, {codes})")
        }
        Kind::Sparse => {
            reader.i64()?;
            reader.u32()?;
            let positions = describe_chunk(reader)?;
            let exceptions = describe_chunk(reader)?;
            format!("SPARSE({positions}, {exceptions})")
        }
    })
}

/// The distance from the frame of reference base, which is always representable in a `u64` because
/// both ends came from an `i64` and the width of the difference is at most 65 bits minus the sign.
fn offset_from(value: i64, base: i64) -> u64 {
    (i128::from(value) - i128::from(base)) as u64
}

fn value_from(offset: u64, base: i64) -> i64 {
    (i128::from(base) + i128::from(offset)) as i64
}

/// Zigzag, so that a column that counts down packs as narrowly as one that counts up. Without it a
/// delta of -1 is 64 bits of ones.
fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// The zigzagged differences, or `None` if any difference is too wide to be one.
///
/// A column holding both `i64::MIN` and `i64::MAX` has a difference that does not fit in an `i64`,
/// and rather than widening every delta array to 128 bits for a case that does not occur in data,
/// the encoding declines to apply. `Packed` covers it.
fn deltas(values: &[i64]) -> Option<Vec<i64>> {
    let mut deltas = Vec::with_capacity(values.len().saturating_sub(1));
    for pair in values.windows(2) {
        let difference = i128::from(pair[1]) - i128::from(pair[0]);
        let difference = i64::try_from(difference).ok()?;
        deltas.push(zigzag(difference) as i64);
    }
    Some(deltas)
}

fn run_count(values: &[i64]) -> usize {
    let mut runs = 0;
    let mut previous = None;
    for value in values {
        if previous != Some(value) {
            runs += 1;
            previous = Some(value);
        }
    }
    runs
}

fn runs(values: &[i64]) -> (Vec<i64>, Vec<i64>) {
    let mut run_values: Vec<i64> = Vec::new();
    let mut run_lengths: Vec<i64> = Vec::new();
    for value in values {
        if run_values.last() == Some(value) {
            *run_lengths.last_mut().expect("a run length exists beside every run value") += 1;
        } else {
            run_values.push(*value);
            run_lengths.push(1);
        }
    }
    (run_values, run_lengths)
}

/// The distinct values in sorted order.
///
/// Sorted rather than in order of first appearance, because an ordered dictionary is what lets a
/// range predicate become a code range instead of a code set, per section 6.7, and because the
/// codes of a clustered column then run in order and delta encode.
fn distinct_values(values: &[i64]) -> Vec<i64> {
    let mut distinct = values.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    distinct
}

fn codes_over(values: &[i64], dictionary: &[i64]) -> Vec<i64> {
    values
        .iter()
        .map(|value| {
            dictionary
                .binary_search(value)
                .expect("the dictionary is the distinct values of this chunk") as i64
        })
        .collect()
}

/// The most frequent value and how often it occurs, found without a hash map because the caller
/// only asks when the column is already suspected of being nearly constant.
fn dominant_value(values: &[i64]) -> Option<(i64, usize)> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mut best: Option<(i64, usize)> = None;
    let mut index = 0;
    while index < sorted.len() {
        let value = sorted[index];
        let mut end = index;
        while end < sorted.len() && sorted[end] == value {
            end += 1;
        }
        let count = end - index;
        if best.is_none_or(|(_, seen)| count > seen) {
            best = Some((value, count));
        }
        index = end;
    }
    best
}

fn check_count(actual: usize, expected: usize) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::internal(format!(
            "a chunk says it holds {expected} values and decoded to {actual}"
        )))
    }
}

fn too_long(len: usize) -> Error {
    Error::internal(format!("a chunk of {len} values is longer than the format allows"))
}

fn put_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// A cursor over a chunk. Every read is checked, because these bytes come off a disk that has been
/// there longer than the process has.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self.at + N;
        if end > self.bytes.len() {
            return Err(Error::internal(format!(
                "a chunk ended after {} bytes with {N} more wanted",
                self.bytes.len()
            )));
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.at..end]);
        self.at = end;
        Ok(out)
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.at + len;
        if end > self.bytes.len() {
            return Err(Error::internal(format!(
                "a chunk ended after {} bytes with {len} more wanted",
                self.bytes.len()
            )));
        }
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take::<1>()?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take::<4>()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take::<8>()?))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take::<8>()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(values: &[i64]) -> Vec<u8> {
        let bytes = encode(values).unwrap();
        assert_eq!(decode(&bytes).unwrap(), values, "{}", describe(&bytes).unwrap());
        bytes
    }

    fn kind_of(bytes: &[u8]) -> Kind {
        Kind::from_tag(bytes[0]).unwrap()
    }

    /// The same xorshift the bit packing tests use, for the same reason.
    struct Random(u64);

    impl Random {
        fn new() -> Self {
            Self(0x9e37_79b9_7f4a_7c15)
        }

        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn an_empty_chunk_round_trips() {
        let bytes = round_trip(&[]);
        assert_eq!(bytes.len(), 5);
    }

    #[test]
    fn a_constant_column_costs_thirteen_bytes_however_long_it_is() {
        let bytes = round_trip(&vec![42; 1_000_000]);
        assert_eq!(kind_of(&bytes), Kind::Constant);
        assert_eq!(bytes.len(), 13);
    }

    #[test]
    fn a_narrow_range_is_packed_at_the_width_of_the_range_and_not_of_the_type() {
        // 100_000 values between 1000 and 1063 is 6 bits each, plus 9 bytes of header per 1024.
        let mut random = Random::new();
        let values: Vec<i64> = (0..100_000).map(|_| 1000 + (random.next() % 64) as i64).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Packed);
        let packed = 100_000 * 6 / 8;
        assert!(bytes.len() < packed + 2000, "{} bytes for {packed} of payload", bytes.len());
        assert!(bytes.len() > packed, "{} bytes cannot hold {packed}", bytes.len());
    }

    #[test]
    fn a_counter_becomes_deltas_and_then_a_constant() {
        // The classic case and the reason DELTA exists. A million consecutive integers is a
        // difference of 1 a million times, which is a constant chunk under the delta.
        let values: Vec<i64> = (0..1_000_000).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Delta);
        assert_eq!(describe(&bytes).unwrap(), "DELTA(CONSTANT)");
        assert!(bytes.len() < 40, "{} bytes for a counter", bytes.len());
    }

    #[test]
    fn a_column_that_counts_down_is_as_cheap_as_one_that_counts_up() {
        // What zigzag is for. Without it every delta is -1, which is 64 bits of ones.
        let up: Vec<i64> = (0..100_000).collect();
        let down: Vec<i64> = (0..100_000).rev().collect();
        assert_eq!(round_trip(&up).len(), round_trip(&down).len());
    }

    #[test]
    fn long_runs_become_rle() {
        let mut values = Vec::new();
        for run in 0..1000 {
            values.extend(std::iter::repeat_n(run % 7, 200));
        }
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Rle);
        assert!(bytes.len() < 2000, "{} bytes for 1000 runs", bytes.len());
    }

    #[test]
    fn a_low_cardinality_column_becomes_a_dictionary() {
        // Values that are far apart so that packing them directly is 30 bits each, and only 40 of
        // them so that the codes are 6 bits each. The dictionary has to win by a factor of five.
        let mut random = Random::new();
        let dictionary: Vec<i64> = (0..40).map(|index| 1_000_000_000 + index * 7919).collect();
        let values: Vec<i64> =
            (0..100_000).map(|_| dictionary[(random.next() % 40) as usize]).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Dict);
        assert!(bytes.len() < 100_000, "{} bytes", bytes.len());
    }

    #[test]
    fn a_nearly_constant_column_becomes_sparse() {
        let mut values = vec![0i64; 100_000];
        for index in 0..300 {
            values[index * 331] = 1 << 40;
        }
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Sparse);
        assert!(bytes.len() < 3000, "{} bytes for 300 exceptions", bytes.len());
    }

    #[test]
    fn the_cascade_goes_more_than_one_level_deep() {
        // The whole point of section 6.3. A dictionary over a clustered column produces codes that
        // run in long stretches, and the run lengths of those are themselves compressible.
        let mut values = Vec::new();
        for index in 0..2000i64 {
            values.extend(std::iter::repeat_n(1_000_000 + (index % 5) * 104_729, 100));
        }
        let bytes = round_trip(&values);
        let shape = describe(&bytes).unwrap();
        assert!(shape.contains('('), "{shape} is not a cascade");
        assert!(bytes.len() < 4000, "{} bytes: {shape}", bytes.len());
    }

    #[test]
    fn random_data_is_packed_at_full_width_and_costs_what_it_costs() {
        // The case where nothing works, which has to come out at eight bytes a value plus change
        // rather than at eight bytes a value plus a dictionary of every value in the column.
        let mut random = Random::new();
        let values: Vec<i64> = (0..10_000).map(|_| random.next() as i64).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Packed);
        assert!(bytes.len() < 10_000 * 8 + 1000, "{} bytes", bytes.len());
    }

    #[test]
    fn the_extremes_of_the_type_survive() {
        // Every offset and every delta in here overflows something if the arithmetic is done in 64
        // bits, which is why it is done in 128.
        let values = vec![i64::MIN, i64::MAX, 0, -1, i64::MIN, i64::MAX];
        round_trip(&values);
        round_trip(&[i64::MIN; 3]);
        round_trip(&[i64::MIN, i64::MIN + 1]);
    }

    #[test]
    fn a_chunk_that_is_not_a_multiple_of_the_unit_round_trips() {
        for len in [1, 2, 1023, 1024, 1025, 2047, 2049] {
            let values: Vec<i64> = (0..len).map(|index| (index * 31 % 97) as i64).collect();
            round_trip(&values);
        }
    }

    #[test]
    fn a_partial_unit_costs_its_own_values_and_not_a_whole_unit() {
        // Three values that need 40 bits each. In the transposed layout a unit is 1024 values
        // whether it holds them or not, so this would be 5 KB, and every nested array in a cascade
        // is this short. It is 15 bytes of payload and 14 of header.
        let values = vec![1i64 << 39, (1 << 39) + 7, 1 << 38];
        let bytes = encode_as(Kind::Packed, &values, 0).unwrap().unwrap();
        assert_eq!(bytes.len(), 5 + 9 + 15);
        assert_eq!(decode(&bytes).unwrap(), values);
    }

    #[test]
    fn the_frame_of_reference_is_per_unit_and_not_per_chunk() {
        // A column that drifts, which is what a timestamp column and a clustered key both do. Each
        // unit here spans 1023 and packs at 10 bits, and a base per chunk would pay the 22 bits the
        // whole chunk spans on every value in it.
        let values: Vec<i64> =
            (0..4096i64).map(|index| (index / 1024) * 1_000_000 + (index % 1024)).collect();
        let bytes = encode_as(Kind::Packed, &values, 0).unwrap().unwrap();
        assert_eq!(describe(&bytes).unwrap(), "FOR+BITPACK[10]");
        assert_eq!(decode(&bytes).unwrap(), values);
    }

    #[test]
    fn every_candidate_that_applies_decodes_to_the_input() {
        // The chooser only ever hands back the smallest, so without this the other five are only
        // tested when they happen to win. Any of them being wrong is a wrong answer that appears
        // when a column's distribution shifts.
        let mut values = vec![5i64; 3000];
        for (index, value) in values.iter_mut().enumerate() {
            if index % 500 == 0 {
                *value = index as i64;
            }
        }
        let applicable = candidates(&values, 0);
        assert!(applicable.len() >= 4, "{applicable:?}");
        for kind in applicable {
            let bytes = encode_as(kind, &values, 0).unwrap().unwrap();
            assert_eq!(decode(&bytes).unwrap(), values, "{}", kind.name());
        }
    }

    #[test]
    fn the_chooser_picks_the_smallest_candidate_rather_than_the_first_that_applies() {
        let mut values = vec![5i64; 3000];
        values[1500] = 9;
        let chosen = encode(&values).unwrap();
        for (_, size) in candidate_sizes(&values).unwrap() {
            assert!(chosen.len() <= size);
        }
    }

    #[test]
    fn a_truncated_chunk_is_an_error_and_not_a_panic() {
        let bytes = encode(&[1, 2, 3, 4, 5]).unwrap();
        for len in 0..bytes.len() {
            let error = decode(&bytes[..len]).unwrap_err();
            assert!(error.message().contains("chunk"), "{error}");
        }
    }

    #[test]
    fn trailing_bytes_are_an_error() {
        let mut bytes = encode(&[1, 2, 3]).unwrap();
        bytes.push(0);
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("left over"), "{error}");
    }

    #[test]
    fn an_unknown_tag_is_an_error() {
        let error = decode(&[99, 0, 0, 0, 0]).unwrap_err();
        assert!(error.message().contains("unknown encoding tag"), "{error}");
    }

    #[test]
    fn a_dictionary_code_outside_the_dictionary_is_an_error() {
        // A corrupted or malicious chunk must not index out of bounds, and this is the one place in
        // the decoder where a number that came off the disk is used as an index. Built by hand
        // rather than by corrupting a real chunk, because a byte offset into an encoding that the
        // chooser is free to change is a test that breaks for the wrong reason.
        let mut bytes = vec![Kind::Dict.tag()];
        put_u32(&mut bytes, 1);
        bytes.extend_from_slice(&encode(&[10]).unwrap());
        bytes.extend_from_slice(&encode(&[5]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("not in the dictionary"), "{error}");
    }

    #[test]
    fn a_negative_run_length_is_an_error() {
        // The other number off the disk that the decoder would otherwise trust, and the one that
        // would turn into an allocation of nine quintillion values.
        let mut bytes = vec![Kind::Rle.tag()];
        put_u32(&mut bytes, 4);
        bytes.extend_from_slice(&encode(&[7]).unwrap());
        bytes.extend_from_slice(&encode(&[-4]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("negative"), "{error}");
    }

    #[test]
    fn the_cascade_depth_is_bounded() {
        // Without the limit a chooser that finds a dictionary of a dictionary of a dictionary would
        // recurse until the values ran out, and the encode time of a wide column would be a
        // surprise rather than a number.
        let values: Vec<i64> = (0..50_000).map(|index| (index / 100) % 250).collect();
        let bytes = round_trip(&values);
        let shape = describe(&bytes).unwrap();
        let depth = shape.matches('(').count();
        assert!(depth <= MAX_DEPTH as usize, "{shape} is {depth} deep");
    }

    #[test]
    fn candidate_sizes_reports_what_the_chooser_looked_at() {
        let values: Vec<i64> = (0..5000).map(|index| index % 17).collect();
        let sizes = candidate_sizes(&values).unwrap();
        assert!(sizes.iter().any(|(kind, _)| *kind == Kind::Dict));
        assert!(sizes.iter().any(|(kind, _)| *kind == Kind::Packed));
        assert!(sizes.iter().all(|(_, size)| *size > 0));
    }
}
