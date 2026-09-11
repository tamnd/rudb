//! Turning one column chunk's pages into vectors.
//!
//! This is where the four pieces meet. [`crate::page`] says where each page begins and ends,
//! `rudb-compress` turns its body into bytes, [`crate::hybrid`] reads the definition levels and the
//! dictionary indices out of it, and [`crate::plain`] reads the values. What is left is the part
//! that is genuinely Parquet rather than any of those: values are stored only for the rows that
//! have one, so a page of a thousand rows with four hundred nulls holds six hundred values, and
//! putting them back where they belong is what the definition levels are for.
//!
//! A flat schema makes that simple. The maximum definition level is one for an optional column and
//! zero for a required one, so a level of one means present and anything else means null, and there
//! is no repetition level at all. The footer reader already rejects a nested schema, so the case
//! where a level of one means something else cannot reach here.
//!
//! The output is a run of vectors of at most [`VECTOR_SIZE`] rather than one vector of the whole
//! chunk, because that is the unit every operator above this is written against. The split is by
//! row index and nothing else, so the same split falls in the same place in every column of the row
//! group and the pieces line up into chunks without anybody coordinating.
//!
//! Dictionary encoded pages are materialized rather than passed through as a dictionary vector.
//! `Vector::dictionary` exists and this is where it will be used, but doing it now means deciding
//! what happens when a chunk's pages disagree about their dictionary and when a null needs a code,
//! and both of those are decisions worth making alongside the predicates that will read them. This
//! reader is the one that makes ClickBench run, not the one that makes it fast.

use rudb_common::{Error, LogicalType, PhysicalType, Result};
use rudb_compress::Codec;
use rudb_vector::{Buffer, Data, StringColumn, VECTOR_SIZE, Validity, Vector};

use crate::hybrid::{Decoder, bit_width};
use crate::metadata::{ColumnChunk, Encoding, SchemaColumn};
use crate::page::{self, Kindof};
use crate::plain::{self, Values};

/// Decodes one column chunk into vectors of at most [`VECTOR_SIZE`] rows.
///
/// `bytes` is the chunk as it lies in the file, starting at [`ColumnChunk::start`], which is the
/// dictionary page when there is one and the first data page when there is not. `rows` is how many
/// rows the row group has, which is how the reader knows when to stop: a writer is allowed to end a
/// chunk with pages nobody needs and the row count is the authority on how many rows there are.
///
/// # Errors
///
/// If a page header does not parse, a page is longer than the chunk holding it, the codec is one
/// this build cannot decompress, an encoding is one this reader does not have yet, or the pages
/// hold fewer rows than the row group claims.
pub(crate) fn decode(
    column: &SchemaColumn,
    chunk: &ColumnChunk,
    bytes: &[u8],
    rows: usize,
) -> Result<Vec<Vector>> {
    let max_level = u32::from(column.optional);
    let (dictionary_bytes, dictionary_count, mut at) = dictionary_page(chunk, bytes)?;
    let dictionary = match &dictionary_bytes {
        Some(page) => Some(plain::decode(chunk.physical, column.width, page, dictionary_count)?),
        None => None,
    };
    let mut builder = Builder::of(&column.ty)?;
    let mut valid: Vec<bool> = Vec::with_capacity(rows);
    let mut levels: Vec<u32> = Vec::new();
    let mut codes: Vec<u32> = Vec::new();
    while valid.len() < rows && at < bytes.len() {
        let header = page::header(&bytes[at..])?;
        let body = body(bytes, at, &header)?;
        at += header.header_size + header.compressed_size;
        let (count, encoding, page) = match header.kind {
            Kindof::Data { values, encoding, definition } => {
                let page = chunk.compression.decompress(body, header.uncompressed_size)?;
                let taken = v1_levels(&page, definition, max_level, values, &mut levels)?;
                (values, encoding, Page { bytes: page, values_at: taken })
            }
            Kindof::DataV2 {
                values,
                nulls,
                encoding,
                definition_bytes,
                repetition_bytes,
                compressed,
            } => {
                let page = v2_page(
                    chunk.compression,
                    body,
                    &header,
                    repetition_bytes,
                    definition_bytes,
                    compressed,
                )?;
                v2_levels(
                    &page.bytes,
                    repetition_bytes,
                    definition_bytes,
                    max_level,
                    values,
                    nulls,
                    &mut levels,
                )?;
                (values, encoding, page)
            }
            Kindof::Dictionary { .. } => {
                return Err(Error::io("a parquet chunk with a dictionary page after a data page"));
            }
            Kindof::Other => continue,
        };
        let count = count.min(rows - valid.len());
        let present = record(&levels, max_level, count, &mut valid);
        let values = &page.bytes[page.values_at..];
        if encoding.is_dictionary() {
            let dictionary = dictionary.as_ref().ok_or_else(|| {
                Error::io(format!("a {} page in a chunk with no dictionary page", encoding.name()))
            })?;
            indices(values, present, &mut codes)?;
            let highest = codes.iter().copied().max().unwrap_or(0) as usize;
            if !codes.is_empty() && highest >= dictionary.len() {
                return Err(Error::io(format!(
                    "a parquet page using dictionary entry {highest} of a dictionary with {} of them",
                    dictionary.len()
                )));
            }
            append(&mut builder, dictionary, Some(&codes), &levels, max_level, count)?;
        } else {
            if encoding != Encoding::Plain {
                return Err(Error::not_implemented(format!(
                    "the parquet {} encoding, on column {}",
                    encoding.name(),
                    column.name
                )));
            }
            let decoded = plain::decode(chunk.physical, column.width, values, present)?;
            append(&mut builder, &decoded, None, &levels, max_level, count)?;
        }
    }
    if valid.len() < rows {
        return Err(Error::io(format!(
            "a row group of {rows} rows whose {} column holds {} of them",
            column.name,
            valid.len()
        )));
    }
    split(builder.finish(), &valid, &column.ty, rows)
}

/// One page, decompressed, and where its values start inside it.
///
/// The levels live in front of the values in both page versions, so the values are a suffix of the
/// page rather than a buffer of their own, and nothing is copied to separate them.
#[derive(Debug)]
struct Page {
    bytes: Vec<u8>,
    values_at: usize,
}

/// Reads the dictionary page, when the chunk has one, and says where the data pages start.
///
/// The dictionary page is always the first page of the chunk, because [`ColumnChunk::start`] is its
/// offset when it exists. The returned offset is where the walk over data pages begins, which is
/// after the dictionary page when there was one and zero when there was not.
fn dictionary_page(chunk: &ColumnChunk, bytes: &[u8]) -> Result<(Option<Vec<u8>>, usize, usize)> {
    if chunk.dictionary_page_offset.is_none() {
        return Ok((None, 0, 0));
    }
    let header = page::header(bytes)?;
    let body = body(bytes, 0, &header)?;
    let at = header.header_size + header.compressed_size;
    let Kindof::Dictionary { values, encoding } = header.kind else {
        return Err(Error::io("a chunk whose dictionary page offset points at a data page"));
    };
    if !matches!(encoding, Encoding::Plain | Encoding::PlainDictionary) {
        return Err(Error::not_implemented(format!(
            "a parquet dictionary page encoded with {}",
            encoding.name()
        )));
    }
    let page = chunk.compression.decompress(body, header.uncompressed_size)?;
    Ok((Some(page), values, at))
}

/// The page body that follows a header, checked against the bytes that are actually there.
fn body<'a>(bytes: &'a [u8], at: usize, header: &page::Header) -> Result<&'a [u8]> {
    let start = at + header.header_size;
    let end = start
        .checked_add(header.compressed_size)
        .ok_or_else(|| Error::io("a parquet page whose length wraps"))?;
    bytes.get(start..end).ok_or_else(|| {
        Error::io(format!(
            "a parquet page of {} bytes at {start} in a chunk of {}",
            header.compressed_size,
            bytes.len()
        ))
    })
}

/// Reads the definition levels of a v1 data page, and says where the values start.
///
/// A v1 page puts its levels inside its own compressed body, each section prefixed with a four byte
/// little endian length. A required column has no levels at all and the values start at byte zero.
fn v1_levels(
    page: &[u8],
    encoding: Encoding,
    max_level: u32,
    values: usize,
    levels: &mut Vec<u32>,
) -> Result<usize> {
    if max_level == 0 {
        levels.clear();
        return Ok(0);
    }
    if encoding != Encoding::Rle {
        return Err(Error::not_implemented(format!(
            "parquet definition levels encoded with {}, which only writers from before 2013 emit",
            encoding.name()
        )));
    }
    let header = page
        .get(..4)
        .ok_or_else(|| Error::io("a parquet page that ends inside its level length"))?;
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let end =
        4usize.checked_add(len).ok_or_else(|| Error::io("a parquet level length that wraps"))?;
    let section = page.get(4..end).ok_or_else(|| {
        Error::io(format!("a parquet level section of {len} bytes past the end of its page"))
    })?;
    Decoder::new(section, bit_width(max_level)).read(levels, values)?;
    Ok(end)
}

/// Decompresses a v2 data page, leaving its levels where they are.
///
/// A v2 page compresses only its values, so the levels in front of them are already bytes and the
/// decompression covers the rest. They are put back together into one buffer here so that the rest
/// of the reader sees the same shape as a v1 page.
fn v2_page(
    codec: Codec,
    body: &[u8],
    header: &page::Header,
    repetition_bytes: usize,
    definition_bytes: usize,
    compressed: bool,
) -> Result<Page> {
    let levels = repetition_bytes
        .checked_add(definition_bytes)
        .ok_or_else(|| Error::io("a parquet page whose level lengths wrap"))?;
    if levels > body.len() || levels > header.uncompressed_size {
        return Err(Error::io(format!(
            "a parquet v2 page of {} bytes claiming {levels} bytes of levels",
            body.len()
        )));
    }
    let mut bytes = body[..levels].to_vec();
    if compressed {
        bytes.extend_from_slice(
            &codec.decompress(&body[levels..], header.uncompressed_size - levels)?,
        );
    } else {
        bytes.extend_from_slice(&body[levels..]);
    }
    Ok(Page { bytes, values_at: levels })
}

/// Reads the definition levels of a v2 data page.
///
/// The levels are always `RLE` in a v2 page, the format says so, and their length is in the header
/// rather than in front of them. A page whose column has no nulls still writes them, so the null
/// count in the header is a cross check rather than a substitute.
fn v2_levels(
    page: &[u8],
    repetition_bytes: usize,
    definition_bytes: usize,
    max_level: u32,
    values: usize,
    nulls: usize,
    levels: &mut Vec<u32>,
) -> Result<()> {
    if max_level == 0 || definition_bytes == 0 {
        levels.clear();
        if nulls != 0 {
            return Err(Error::io(format!(
                "a parquet v2 page claiming {nulls} nulls with no definition levels"
            )));
        }
        return Ok(());
    }
    let section = page
        .get(repetition_bytes..repetition_bytes + definition_bytes)
        .ok_or_else(|| Error::io("a parquet v2 page shorter than the levels it claims"))?;
    Decoder::new(section, bit_width(max_level)).read(levels, values)
}

/// Reads the dictionary indices of a data page.
///
/// The bit width is one byte in front of the stream rather than anything the metadata says, because
/// it depends on how many distinct values the chunk turned out to have and the writer only knows
/// that once it has written them.
fn indices(values: &[u8], present: usize, codes: &mut Vec<u32>) -> Result<()> {
    let (&width, rest) = values
        .split_first()
        .ok_or_else(|| Error::io("a parquet dictionary page with no bit width in front of it"))?;
    Decoder::new(rest, width).read(codes, present)
}

/// Records which rows of a page have a value, and says how many do.
///
/// A required column has no levels, so every row has a value and the count is the row count. This is
/// the one place the two cases are told apart and everything below it takes the levels as given.
fn record(levels: &[u32], max_level: u32, count: usize, valid: &mut Vec<bool>) -> usize {
    if max_level == 0 {
        valid.resize(valid.len() + count, true);
        return count;
    }
    let mut present = 0;
    for &level in levels.iter().take(count) {
        let here = level >= max_level;
        present += usize::from(here);
        valid.push(here);
    }
    present
}

/// The column being built, one variant per physical layout a Parquet column can land in.
#[derive(Debug)]
enum Builder {
    Bool(Vec<bool>),
    Int8(Vec<i8>),
    Int16(Vec<i16>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    Int128(Vec<i128>),
    UInt8(Vec<u8>),
    UInt16(Vec<u16>),
    UInt32(Vec<u32>),
    UInt64(Vec<u64>),
    UInt128(Vec<u128>),
    Float32(Vec<f32>),
    Float64(Vec<f64>),
    Text(StringColumn),
}

impl Builder {
    /// An empty builder for a column of `ty`.
    ///
    /// # Errors
    ///
    /// If the type is one that has no flat layout, which is the nested types. The footer reader
    /// rejects a nested schema before this is reached, so a type arriving here that has no layout
    /// is a reader bug rather than a file.
    fn of(ty: &LogicalType) -> Result<Self> {
        Ok(match ty.physical() {
            PhysicalType::Bool => Self::Bool(Vec::new()),
            PhysicalType::Int8 => Self::Int8(Vec::new()),
            PhysicalType::Int16 => Self::Int16(Vec::new()),
            PhysicalType::Int32 => Self::Int32(Vec::new()),
            PhysicalType::Int64 => Self::Int64(Vec::new()),
            PhysicalType::Int128 => Self::Int128(Vec::new()),
            PhysicalType::UInt8 => Self::UInt8(Vec::new()),
            PhysicalType::UInt16 => Self::UInt16(Vec::new()),
            PhysicalType::UInt32 => Self::UInt32(Vec::new()),
            PhysicalType::UInt64 => Self::UInt64(Vec::new()),
            PhysicalType::UInt128 => Self::UInt128(Vec::new()),
            PhysicalType::Float32 => Self::Float32(Vec::new()),
            PhysicalType::Float64 => Self::Float64(Vec::new()),
            PhysicalType::Varlen => Self::Text(StringColumn::new()),
            other => {
                return Err(Error::not_implemented(format!(
                    "a parquet column laid out as {other:?}"
                )));
            }
        })
    }

    /// The values, as a vector's data.
    fn finish(self) -> Data {
        match self {
            Self::Bool(values) => Data::Bool(Buffer::from_vec(values)),
            Self::Int8(values) => Data::Int8(Buffer::from_vec(values)),
            Self::Int16(values) => Data::Int16(Buffer::from_vec(values)),
            Self::Int32(values) => Data::Int32(Buffer::from_vec(values)),
            Self::Int64(values) => Data::Int64(Buffer::from_vec(values)),
            Self::Int128(values) => Data::Int128(Buffer::from_vec(values)),
            Self::UInt8(values) => Data::UInt8(Buffer::from_vec(values)),
            Self::UInt16(values) => Data::UInt16(Buffer::from_vec(values)),
            Self::UInt32(values) => Data::UInt32(Buffer::from_vec(values)),
            Self::UInt64(values) => Data::UInt64(Buffer::from_vec(values)),
            Self::UInt128(values) => Data::UInt128(Buffer::from_vec(values)),
            Self::Float32(values) => Data::Float32(Buffer::from_vec(values)),
            Self::Float64(values) => Data::Float64(Buffer::from_vec(values)),
            Self::Text(values) => Data::Varlen(values),
        }
    }
}

/// Appends one page's worth of rows to the column.
///
/// `codes` is the dictionary indices when the page was dictionary encoded and nothing when the
/// values are the page's own. Either way there is one entry per row that has a value, and the
/// levels say which rows those are.
///
/// The match is on the pair of layouts and it is deliberately a list rather than a fallback. A
/// physical type that reaches a builder it does not belong in is a file the footer reader described
/// wrongly, and quietly widening it would turn that into a wrong answer.
fn append(
    builder: &mut Builder,
    values: &Values<'_>,
    codes: Option<&[u32]>,
    levels: &[u32],
    max_level: u32,
    rows: usize,
) -> Result<()> {
    let levels = if max_level == 0 { None } else { Some(levels) };
    let put = Put { codes, levels, max_level, rows };
    match (&mut *builder, values) {
        (Builder::Bool(out), Values::Bool(src)) => put.run(out, src, |&v| v),
        (Builder::Int8(out), Values::Int32(src)) => put.run(out, src, |&v| v as i8),
        (Builder::Int16(out), Values::Int32(src)) => put.run(out, src, |&v| v as i16),
        (Builder::Int32(out), Values::Int32(src)) => put.run(out, src, |&v| v),
        (Builder::Int64(out), Values::Int32(src)) => put.run(out, src, |&v| i64::from(v)),
        (Builder::Int64(out), Values::Int64(src)) => put.run(out, src, |&v| v),
        (Builder::UInt8(out), Values::Int32(src)) => put.run(out, src, |&v| v as u8),
        (Builder::UInt16(out), Values::Int32(src)) => put.run(out, src, |&v| v as u16),
        (Builder::UInt32(out), Values::Int32(src)) => put.run(out, src, |&v| v as u32),
        (Builder::UInt64(out), Values::Int64(src)) => put.run(out, src, |&v| v as u64),
        (Builder::Float32(out), Values::Float(src)) => put.run(out, src, |&v| v),
        (Builder::Float64(out), Values::Double(src)) => put.run(out, src, |&v| v),
        (Builder::Int16(out), Values::Bytes(src)) => put.run(out, src, |&v| twos(v) as i16),
        (Builder::Int32(out), Values::Bytes(src)) => put.run(out, src, |&v| twos(v) as i32),
        (Builder::Int64(out), Values::Bytes(src)) => put.run(out, src, |&v| twos(v) as i64),
        (Builder::Int128(out), Values::Bytes(src)) => put.run(out, src, twos_ref),
        (Builder::Text(out), Values::Bytes(src)) => return put.text(out, src),
        (builder, values) => {
            return Err(Error::internal(format!(
                "a parquet page of {values:?} values cannot fill a {builder:?} column"
            )));
        }
    }
    Ok(())
}

/// A big endian two's complement integer, which is how Parquet writes a decimal in bytes.
fn twos(bytes: &[u8]) -> i128 {
    let mut value: i128 = if bytes.first().is_some_and(|&head| head & 0x80 != 0) { -1 } else { 0 };
    for &byte in bytes {
        value = (value << 8) | i128::from(byte);
    }
    value
}

/// [`twos`] behind the reference a `Vec<&[u8]>` iterates into.
fn twos_ref(bytes: &&[u8]) -> i128 {
    twos(bytes)
}

/// Where a page's values go, which is everything the two loops below need to know.
#[derive(Debug, Clone, Copy)]
struct Put<'a> {
    codes: Option<&'a [u32]>,
    levels: Option<&'a [u32]>,
    max_level: u32,
    rows: usize,
}

impl Put<'_> {
    /// Appends `rows` values, filling the null positions with the type's zero.
    ///
    /// The zero is never read, because the validity says the row is null, and it is there because a
    /// flat vector holds a value at every position whether or not that position means anything.
    fn run<S, T: Copy + Default>(self, out: &mut Vec<T>, src: &[S], map: impl Fn(&S) -> T) {
        out.reserve(self.rows);
        let mut next = 0;
        for at in 0..self.rows {
            if self.levels.is_some_and(|levels| levels[at] < self.max_level) {
                out.push(T::default());
                continue;
            }
            match self.pick(src, next) {
                Some(value) => out.push(map(value)),
                None => out.push(T::default()),
            }
            next += 1;
        }
    }

    /// The same, for the string column, which is not a `Vec` and can fail on bytes that are not text.
    fn text(self, out: &mut StringColumn, src: &[&[u8]]) -> Result<()> {
        let mut next = 0;
        for at in 0..self.rows {
            if self.levels.is_some_and(|levels| levels[at] < self.max_level) {
                out.push("");
                continue;
            }
            let bytes = self.pick(src, next).copied().unwrap_or(b"");
            let text = std::str::from_utf8(bytes).map_err(|_| {
                Error::not_implemented(
                    "a parquet byte array column whose bytes are not UTF-8, which rudb's string column cannot hold yet",
                )
            })?;
            out.push(text);
            next += 1;
        }
        Ok(())
    }

    /// The value for the `next`th row that has one, through the dictionary when there is one.
    ///
    /// Nothing when the page is short, which the caller turns into the type's zero. Both ways of
    /// being short are checked before this: a page claiming more values than it holds by
    /// [`plain::decode`], and a dictionary index past the end of its dictionary by [`decode`]. The
    /// zero is what is left if either check is ever wrong, and it is a wrong value rather than a
    /// panic on a file nobody trusted in the first place.
    fn pick<S>(self, src: &[S], next: usize) -> Option<&S> {
        match self.codes {
            Some(codes) => src.get(*codes.get(next)? as usize),
            None => src.get(next),
        }
    }
}

/// Cuts a whole chunk's worth of values into vectors of at most [`VECTOR_SIZE`] rows.
fn split(data: Data, valid: &[bool], ty: &LogicalType, rows: usize) -> Result<Vec<Vector>> {
    let mut out = Vec::with_capacity(rows.div_ceil(VECTOR_SIZE));
    let whole = Vector::flat(ty.clone(), data)?
        .with_validity(Validity::from_run(&valid[..rows]).normalize(rows));
    let mut at = 0;
    while at < rows {
        let len = VECTOR_SIZE.min(rows - at);
        let indices: Vec<u32> = (at..at + len).map(|row| row as u32).collect();
        out.push(whole.gather(&indices)?);
        at += len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;
    use rudb_vector::Validity;

    use super::{Builder, Put, record, split, twos};
    use crate::plain::Values;

    #[test]
    fn a_required_column_has_no_levels_and_every_row_is_present() {
        let mut valid = Vec::new();
        assert_eq!(record(&[], 0, 3, &mut valid), 3);
        assert_eq!(valid, vec![true, true, true]);
    }

    #[test]
    fn a_level_below_the_maximum_is_a_null() {
        let mut valid = Vec::new();
        assert_eq!(record(&[1, 0, 1, 1], 1, 4, &mut valid), 3);
        assert_eq!(valid, vec![true, false, true, true]);
    }

    #[test]
    fn the_values_of_a_page_land_where_the_levels_say_they_do() {
        // Two values for three rows, because the middle row is null and a null is not stored.
        let mut out = Vec::new();
        let put = Put { codes: None, levels: Some(&[1, 0, 1]), max_level: 1, rows: 3 };
        put.run(&mut out, &[10_i32, 20], |&v| v);
        assert_eq!(out, vec![10, 0, 20]);
    }

    #[test]
    fn a_page_that_is_short_fills_the_rest_with_the_zero_rather_than_panicking() {
        let mut out = Vec::new();
        let put = Put { codes: Some(&[0, 9]), levels: None, max_level: 0, rows: 2 };
        put.run(&mut out, &[5_i32], |&v| v);
        assert_eq!(out, vec![5, 0]);
        assert_eq!(Values::Int32(vec![5]).len(), 1);
    }

    #[test]
    fn a_dictionary_page_is_read_through_its_codes() {
        let mut out = Vec::new();
        let put = Put { codes: Some(&[2, 0, 1]), levels: None, max_level: 0, rows: 3 };
        put.run(&mut out, &[7_i32, 8, 9], |&v| v);
        assert_eq!(out, vec![9, 7, 8]);
    }

    #[test]
    fn a_decimal_in_bytes_is_big_endian_and_signed() {
        assert_eq!(twos(&[0x00, 0x01]), 1);
        assert_eq!(twos(&[0xff, 0xff]), -1);
        assert_eq!(twos(&[0x80, 0x00]), -32768);
        assert_eq!(twos(&[0x01, 0x00, 0x00]), 65536);
    }

    #[test]
    fn a_chunk_longer_than_a_vector_comes_back_as_several() {
        let mut builder = Builder::of(&LogicalType::Integer).expect("has a layout");
        let Builder::Int32(out) = &mut builder else { panic!("an integer column") };
        out.extend(0..2500_i32);
        let valid = vec![true; 2500];
        let pieces = split(builder.finish(), &valid, &LogicalType::Integer, 2500).expect("splits");
        assert_eq!(
            pieces.iter().map(rudb_vector::Vector::len).collect::<Vec<_>>(),
            vec![1024, 1024, 452]
        );
        assert_eq!(pieces[2].value_at(451), rudb_common::Value::Integer(2499));
    }

    #[test]
    fn a_column_with_no_nulls_comes_back_saying_so() {
        let mut builder = Builder::of(&LogicalType::Integer).expect("has a layout");
        let Builder::Int32(out) = &mut builder else { panic!("an integer column") };
        out.extend([1, 2, 3]);
        let pieces =
            split(builder.finish(), &[true, true, true], &LogicalType::Integer, 3).expect("splits");
        assert_eq!(pieces[0].validity(), &Validity::AllValid);
    }
}
