//! Turning the body of a page into a vector.
//!
//! Two encodings between them cover almost every page any writer emits. Plain, which is the values
//! one after another in their wire form, and dictionary, which is indices into a dictionary page
//! that came first in the same chunk. The delta encodings and byte stream split are worth having
//! and are not here yet.
//!
//! # The wire type is not the layout
//!
//! Parquet has six physical types and rudb has fifteen layouts, so a column's wire type does not
//! decide how it is stored. A `UTINYINT` arrives as four byte integers with an annotation saying
//! eight bits unsigned, and it has to land as one byte a value or the vector's type and its storage
//! disagree, which `Vector::flat` refuses and is right to. So the wire width says how to read and
//! the logical type says where to put it, and the two are separate decisions here.
//!
//! # Nulls are not in the file
//!
//! A page holds only its non-null values. The definition levels say which positions those go in,
//! so reading is dense and the spread over the nulls is a second pass that the common case, a page
//! with no nulls at all, skips entirely.
//!
//! # Strings stay where they are
//!
//! A byte array column's page becomes the arena of its string column, and the views point into it.
//! Nothing is copied except the strings short enough to sit inside a view, which are copied because
//! that is what makes them readable without going near the arena. See `StringColumn::over`.

use std::sync::Arc;

use rudb_common::{Error, LogicalType, PhysicalType, Result};
use rudb_vector::{Buffer, Data, StringColumn, Validity, Vector};

use crate::chunk::Page;
use crate::hybrid::Hybrid;
use crate::metadata::{Encoding, Physical, SchemaColumn};
use crate::page::Body;

impl Page {
    /// The page's values, as a vector.
    ///
    /// `dictionary` is the vector the chunk's dictionary page decoded to, which a dictionary
    /// encoded page needs and a plain one ignores. It is passed in rather than held because one
    /// dictionary serves every data page of the chunk, and a page that decoded its own would be
    /// decoding the same values over and over.
    ///
    /// It arrives behind a handle rather than by value, and the handle is what goes into the page's
    /// vector, so the dictionary is decoded once per chunk and pointed at from then on. It used to
    /// be cloned into each page, which on a ClickBench scan was sixteen percent of the instructions
    /// the whole query ran: a chunk of fifty pages copied its dictionary fifty times into fifty
    /// handles that each had one holder.
    ///
    /// This takes the page by mutable reference and empties its body rather than consuming it,
    /// because the caller wants that buffer back to read the next page into. A byte array column
    /// is the one case where it does not come back: those strings are left in the body and pointed
    /// at, so the page becomes the column's arena and the caller gets an empty buffer.
    ///
    /// # Errors
    ///
    /// If the page is a dictionary or index page, if it is in an encoding this reader does not
    /// read yet, if a dictionary encoded page arrives without a dictionary, or if the values run
    /// off the end of the body. Also if the column is one of the types named as not read yet:
    /// `INT96`, fixed length byte arrays, and byte arrays that are not text.
    pub fn decode(
        &mut self,
        column: &SchemaColumn,
        dictionary: Option<&Arc<Vector>>,
    ) -> Result<Vector> {
        let encoding = match &self.header.body {
            Body::DataV1(page) => page.encoding,
            Body::DataV2(page) => page.encoding,
            Body::Dictionary(_) | Body::Index => {
                return Err(Error::io(
                    "values were asked of a page that holds no rows".to_string(),
                ));
            }
        };
        let total = usize::try_from(self.header.values())
            .map_err(|_| Error::io("a page with a negative value count".to_string()))?;
        let (levels, at) = self.definitions(column.optional)?;
        let (validity, valid) = presence(&levels, total);
        match encoding {
            Encoding::Plain => {
                let data = plain(column, &mut self.body, at, valid, &levels, total)?;
                Ok(Vector::flat(column.ty.clone(), data)?.with_validity(validity))
            }
            Encoding::PlainDictionary | Encoding::RleDictionary => {
                let dictionary = dictionary.ok_or_else(|| {
                    Error::io(
                        "a dictionary encoded page in a chunk with no dictionary page".to_string(),
                    )
                })?;
                let codes = codes(&self.body, at, valid, &levels, total, dictionary.len())?;
                Ok(Vector::dictionary_over(codes, Arc::clone(dictionary))?.with_validity(validity))
            }
            Encoding::DeltaBinaryPacked
            | Encoding::DeltaLengthByteArray
            | Encoding::DeltaByteArray
            | Encoding::ByteStreamSplit => {
                let data = delta(column, encoding, &mut self.body, at, valid, &levels, total)?;
                Ok(Vector::flat(column.ty.clone(), data)?.with_validity(validity))
            }
            other => {
                Err(Error::io(format!("a page in {other:?}, which this reader does not read yet")))
            }
        }
    }

    /// The values of a dictionary page, as a vector.
    ///
    /// Its values are always plain encoded and never null, which is why this is separate rather
    /// than a case inside [`Page::decode`]: a dictionary page has no levels, and asking it for
    /// any is an error there and would have to be an exception here.
    ///
    /// # Errors
    ///
    /// If the page is not a dictionary page, or for the same reasons [`Page::decode`] fails.
    pub fn decode_dictionary(&mut self, column: &SchemaColumn) -> Result<Vector> {
        let Body::Dictionary(page) = &self.header.body else {
            return Err(Error::io("a dictionary was asked of a data page".to_string()));
        };
        if !matches!(page.encoding, Encoding::Plain | Encoding::PlainDictionary) {
            return Err(Error::io(format!(
                "a dictionary page in {:?}, which is not an encoding a dictionary is written in",
                page.encoding
            )));
        }
        let count = usize::try_from(page.values)
            .map_err(|_| Error::io("a dictionary page with a negative count".to_string()))?;
        let data = plain(column, &mut self.body, 0, count, &[], count)?;
        Vector::flat(column.ty.clone(), data)
    }
}

/// The validity a run of definition levels describes, and how many values are actually in the page.
///
/// Both from one pass. They were two functions and each walked the levels, which for a nullable
/// column with nothing null in it is two passes over four bytes a row to find out that every one of
/// them says the same thing. The count is what the page needs to know how many values to read and
/// the validity is what the vector carries, so they are always wanted together.
fn presence(levels: &[u32], total: usize) -> (Validity, usize) {
    if levels.is_empty() {
        return (Validity::AllValid, total);
    }
    let valid = levels.iter().filter(|&&level| level == 1).count();
    if valid == levels.len() {
        return (Validity::AllValid, total);
    }
    (Validity::from_iter(total, |index| levels.get(index) == Some(&1)), valid)
}

/// Spreads dense values over the positions the levels say are not null.
///
/// The nulls get whatever the type's zero is, which nothing reads, because the validity beside the
/// data says they are not there. Writing a value rather than leaving a hole is what lets every
/// kernel above read the buffer straight through without checking validity first, which is the
/// whole reason a null has storage in this design at all.
fn spread<T: Copy + Default>(dense: Vec<T>, levels: &[u32], total: usize) -> Vec<T> {
    if levels.is_empty() || dense.len() == total {
        return dense;
    }
    let mut out = vec![T::default(); total];
    let mut next = 0;
    for (index, &level) in levels.iter().enumerate().take(total) {
        if level == 1 {
            if let Some(&value) = dense.get(next) {
                out[index] = value;
            }
            next += 1;
        }
    }
    out
}

/// Reads `count` plain encoded values out of `body` at `at`, laid out for the column's type.
fn plain(
    column: &SchemaColumn,
    body: &mut Vec<u8>,
    at: usize,
    count: usize,
    levels: &[u32],
    total: usize,
) -> Result<Data> {
    let target = column.ty.physical();
    match column.physical {
        Physical::Boolean => {
            let dense = booleans(tail(body, at)?, count)?;
            Ok(Data::Bool(Buffer::from_vec(spread(dense, levels, total))))
        }
        Physical::Int32 => {
            let bytes = fixed::<4>(tail(body, at)?, count)?;
            narrow(target, count, words::<4>(bytes).map(i32::from_le_bytes), levels, total)
        }
        Physical::Int64 => {
            let bytes = fixed::<8>(tail(body, at)?, count)?;
            narrow(target, count, words::<8>(bytes).map(i64::from_le_bytes), levels, total)
        }
        Physical::Float => {
            expect(target, PhysicalType::Float32, &column.ty)?;
            let bytes = fixed::<4>(tail(body, at)?, count)?;
            let dense: Vec<f32> = words::<4>(bytes).map(f32::from_le_bytes).collect();
            Ok(Data::Float32(Buffer::from_vec(spread(dense, levels, total))))
        }
        Physical::Double => {
            expect(target, PhysicalType::Float64, &column.ty)?;
            let bytes = fixed::<8>(tail(body, at)?, count)?;
            let dense: Vec<f64> = words::<8>(bytes).map(f64::from_le_bytes).collect();
            Ok(Data::Float64(Buffer::from_vec(spread(dense, levels, total))))
        }
        Physical::ByteArray => {
            text(column)?;
            strings(column, body, at, count, levels, total)
        }
        Physical::Int96 => Err(Error::io(
            "an INT96 column, which is the timestamp the format deprecated and this reader does \
             not read yet"
                .to_string(),
        )),
        Physical::FixedLenByteArray => Err(Error::io(
            "a FIXED_LEN_BYTE_ARRAY column, which this reader does not read yet".to_string(),
        )),
    }
}

/// Reads one of the delta encodings, or byte stream split, laid out for the column's type.
fn delta(
    column: &SchemaColumn,
    encoding: Encoding,
    body: &mut Vec<u8>,
    at: usize,
    count: usize,
    levels: &[u32],
    total: usize,
) -> Result<Data> {
    let target = column.ty.physical();
    match encoding {
        Encoding::DeltaBinaryPacked => {
            if !matches!(column.physical, Physical::Int32 | Physical::Int64) {
                return Err(Error::io(format!(
                    "a {:?} column delta encoded, which only integers are",
                    column.physical
                )));
            }
            let (dense, _) = crate::delta::binary_packed(tail(body, at)?, count)?;
            narrow(target, count, dense.into_iter(), levels, total)
        }
        Encoding::DeltaLengthByteArray => {
            text(column)?;
            let spans = crate::delta::length_byte_array(tail(body, at)?, count)?;
            // The spans came back relative to where the values start, and the column's arena is the
            // whole page, so every offset moves up by that much. Keeping the page whole rather than
            // slicing it is what lets the levels in front of the values stay where they are.
            let spans: Vec<(usize, usize)> =
                spans.into_iter().map(|(start, len)| (start + at, len)).collect();
            place(column, body, &spans, levels, total)
        }
        Encoding::DeltaByteArray => {
            text(column)?;
            let built = crate::delta::byte_array(tail(body, at)?, count)?;
            // The one encoding whose strings are not in the page, so this is the one column that
            // gets a fresh arena. Nothing else in this reader copies a string's bytes.
            let mut out = StringColumn::with_capacity(total);
            let mut next = 0;
            for index in 0..total {
                if !levels.is_empty() && levels.get(index) != Some(&1) {
                    out.push("");
                    continue;
                }
                let bytes = built.get(next).ok_or_else(|| {
                    Error::io(format!(
                        "a page with {} strings where {total} were wanted",
                        built.len()
                    ))
                })?;
                let value = std::str::from_utf8(bytes).map_err(|_| not_text(column))?;
                out.push(value);
                next += 1;
            }
            Ok(Data::Varlen(out))
        }
        Encoding::ByteStreamSplit => {
            let width = match column.physical {
                Physical::Float | Physical::Int32 => 4,
                Physical::Double | Physical::Int64 => 8,
                other => {
                    return Err(Error::io(format!(
                        "a {other:?} column byte stream split, which is not a fixed width type \
                         this reader splits"
                    )));
                }
            };
            // The transpose puts the bytes back in value order, and after that it is a plain page
            // that happens to live in a different buffer. Reading it through the plain path rather
            // than repeating the type switch is the point of doing the transpose first.
            let mut flat = crate::delta::stream_split(tail(body, at)?, width, count)?;
            plain(column, &mut flat, 0, count, levels, total)
        }
        other => Err(Error::io(format!("a page in {other:?}, which is not a delta encoding"))),
    }
}

/// Builds a string column over a page from spans that are already inside it.
///
/// The empty string stands in for a null, and it costs nothing: a view that short holds its bytes
/// inside itself and never touches the arena. The validity beside the column is what says it is not
/// a string at all.
fn place(
    column: &SchemaColumn,
    body: &mut Vec<u8>,
    spans: &[(usize, usize)],
    levels: &[u32],
    total: usize,
) -> Result<Data> {
    // The page becomes the column's arena, so this is where a body stops being recyclable. The
    // walker gets an empty buffer back and the next page of this column allocates its own.
    let mut out = StringColumn::over(Buffer::from_vec(std::mem::take(body)));
    let mut next = 0;
    for index in 0..total {
        if !levels.is_empty() && levels.get(index) != Some(&1) {
            out.push("");
            continue;
        }
        let &(start, len) = spans.get(next).ok_or_else(|| {
            Error::io(format!("a page with {} strings where {total} were wanted", spans.len()))
        })?;
        out.push_in_place(start, len).map_err(|_| not_text(column))?;
        next += 1;
    }
    Ok(Data::Varlen(out))
}

/// Checks that a byte array column is one this reader has somewhere to put.
///
/// `VARCHAR` and `BLOB`, which between them are every byte array a flat file holds. They are one
/// storage here, because rudb has one variable length column and it is a string column, so a blob
/// is read as its bytes and carries `BLOB` as its type the way it already does everywhere else in
/// the engine.
///
/// That is the whole of the limitation and it is worth stating plainly rather than leaving to be
/// found: the string column validates UTF-8 on the way in, so a blob whose bytes are not valid
/// UTF-8 is refused with an error that names the column. `spread` and the kernels above want a byte
/// column for this and it arrives with the storage layer. It is not a guess in the meantime, which
/// is the thing that would have been wrong: a reader that assumed the bytes were text would answer
/// the query rather than refuse it.
///
/// The annotation matters less than it looks like it should. The ClickBench file has twenty eight
/// byte array columns and not one of them is annotated, so all twenty eight are `BLOB` and reading
/// only `VARCHAR` meant reading none of them. All twenty eight million values in the first
/// partition are valid UTF-8, which is why this is the change that makes that file readable.
fn text(column: &SchemaColumn) -> Result<()> {
    expect(column.ty.physical(), PhysicalType::Varlen, &column.ty)?;
    if !matches!(column.ty, LogicalType::Varchar | LogicalType::Blob) {
        return Err(Error::io(format!(
            "a {} column, which this reader does not read yet",
            column.ty
        )));
    }
    Ok(())
}

/// Says which column a set of bytes that are not text came from.
///
/// The string column reports the offset it refused, which says nothing to anyone reading a query
/// that failed. The column name and the type it was annotated with are what a caller can act on,
/// since an unannotated byte array holding bytes is a file doing nothing wrong.
fn not_text(column: &SchemaColumn) -> Error {
    Error::not_implemented(format!(
        "the column {} holds bytes that are not valid UTF-8, and reading those needs the byte \
         column that arrives with the storage layer",
        column.name
    ))
}

/// The bytes of `body` from `at`, or an error naming how far short it fell.
fn tail(body: &[u8], at: usize) -> Result<&[u8]> {
    body.get(at..).ok_or_else(|| {
        Error::io(format!("a page whose values start at {at} in a body of {}", body.len()))
    })
}

/// Checks that a column's storage is the one the wire type produces.
fn expect(target: PhysicalType, wanted: PhysicalType, ty: &LogicalType) -> Result<()> {
    if target == wanted {
        return Ok(());
    }
    Err(Error::io(format!("a {ty} column stored in a parquet type it cannot come from")))
}

/// The bytes of `count` values of `WIDTH` bytes each, still in the page.
///
/// A slice rather than a vector of arrays, which is what this used to hand back. A plain page is
/// already the values laid out end to end, so copying it into a second buffer to then walk that one
/// is a copy of the whole column for nothing. The callers walk it in place with `chunks_exact`.
fn fixed<const WIDTH: usize>(bytes: &[u8], count: usize) -> Result<&[u8]> {
    let wanted = count.checked_mul(WIDTH).ok_or_else(|| {
        Error::io(format!("a page claiming {count} values of {WIDTH} bytes each"))
    })?;
    bytes.get(..wanted).ok_or_else(|| {
        Error::io(format!(
            "a page needing {wanted} bytes of values with {} left in it",
            bytes.len()
        ))
    })
}

/// The `WIDTH` byte little-endian words of a plain page, in order.
fn words<const WIDTH: usize>(bytes: &[u8]) -> impl Iterator<Item = [u8; WIDTH]> + '_ {
    bytes.chunks_exact(WIDTH).map(|chunk| chunk.try_into().expect("chunks_exact"))
}

/// Reads `count` plain encoded booleans, which are one bit each with the first in the low bit.
fn booleans(bytes: &[u8], count: usize) -> Result<Vec<bool>> {
    let wanted = count.div_ceil(8);
    if bytes.len() < wanted {
        return Err(Error::io(format!(
            "a page needing {wanted} bytes of booleans with {} left in it",
            bytes.len()
        )));
    }
    Ok((0..count).map(|index| bytes[index / 8] & (1 << (index % 8)) != 0).collect())
}

/// Reads `count` length prefixed byte arrays, leaving their bytes in the page.
fn strings(
    column: &SchemaColumn,
    body: &mut Vec<u8>,
    at: usize,
    count: usize,
    levels: &[u32],
    total: usize,
) -> Result<Data> {
    // Where each string is, found by walking the lengths, before the page is handed to the column.
    // Two passes over the same buffer, because the column owns the arena from the moment it is
    // built and the offsets have to be known by then.
    let mut spans = Vec::with_capacity(count);
    let mut cursor = at;
    for _ in 0..count {
        let head = body.get(cursor..cursor + 4).ok_or_else(|| {
            Error::io(format!("a byte array length at {cursor} in a page of {} bytes", body.len()))
        })?;
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let start = cursor + 4;
        let end = start
            .checked_add(len)
            .ok_or_else(|| Error::io("a byte array running past the end of memory".to_string()))?;
        if end > body.len() {
            return Err(Error::io(format!(
                "a byte array at {start} of {len} bytes in a page of {} bytes",
                body.len()
            )));
        }
        spans.push((start, len));
        cursor = end;
    }
    place(column, body, &spans, levels, total)
}

/// Narrows wire integers into whichever integer layout the column's type calls for.
///
/// Parquet has two integer widths and rudb has ten, so this is where a `USMALLINT` written as four
/// byte integers becomes two bytes a value. The conversion is checked per value rather than assumed
/// because a writer that annotated a column as eight bits unsigned and put 300 in it produced a
/// file that cannot be read as what it says it is, and reading it as 44 would be a wrong answer.
fn narrow<T>(
    target: PhysicalType,
    count: usize,
    wire: impl Iterator<Item = T>,
    levels: &[u32],
    total: usize,
) -> Result<Data>
where
    T: Copy + std::fmt::Display,
    i8: TryFrom<T>,
    i16: TryFrom<T>,
    i32: TryFrom<T>,
    i64: TryFrom<T>,
    i128: TryFrom<T>,
    u8: TryFrom<T>,
    u16: TryFrom<T>,
    u32: TryFrom<T>,
    u64: TryFrom<T>,
    u128: TryFrom<T>,
{
    macro_rules! narrowed {
        ($(($layout:ident, $variant:ident, $native:ty)),+ $(,)?) => {
            match target {
                $(PhysicalType::$layout => {
                    // Sized up front. A page is twenty thousand values and growing into it doubles
                    // fifteen times, which is fifteen copies of most of a column per page.
                    let mut dense = Vec::with_capacity(count);
                    for value in wire {
                        let narrowed = <$native>::try_from(value).map_err(|_| {
                            Error::io(format!(
                                "a value of {value} in a column that says it holds {}",
                                stringify!($native)
                            ))
                        })?;
                        dense.push(narrowed);
                    }
                    Ok(Data::$variant(Buffer::from_vec(spread(dense, levels, total))))
                })+
                other => Err(Error::io(format!(
                    "an integer column laid out as {other:?}, which this reader does not fill yet"
                ))),
            }
        };
    }
    narrowed!(
        (Int8, Int8, i8),
        (Int16, Int16, i16),
        (Int32, Int32, i32),
        (Int64, Int64, i64),
        (Int128, Int128, i128),
        (UInt8, UInt8, u8),
        (UInt16, UInt16, u16),
        (UInt32, UInt32, u32),
        (UInt64, UInt64, u64),
        (UInt128, UInt128, u128),
    )
}

/// Reads the dictionary indices of a data page and spreads them over its nulls.
///
/// The bit width is one byte in front of the stream rather than anywhere in the metadata, which is
/// the only place in a Parquet file where that is true and is easy to forget.
fn codes(
    body: &[u8],
    at: usize,
    count: usize,
    levels: &[u32],
    total: usize,
    distinct: usize,
) -> Result<Vec<u32>> {
    let &width = body.get(at).ok_or_else(|| {
        Error::io(format!(
            "a dictionary page whose bit width is at {at} in a body of {}",
            body.len()
        ))
    })?;
    let mut dense = Vec::with_capacity(count);
    Hybrid::new(&body[at + 1..], width)?.read(&mut dense, count)?;
    if let Some(&bad) = dense.iter().find(|&&code| code as usize >= distinct) {
        return Err(Error::io(format!(
            "a dictionary index of {bad} into a dictionary of {distinct} values"
        )));
    }
    Ok(spread(dense, levels, total))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rudb_common::{LogicalType, Value};
    use rudb_io::{File, Filesystem, OpenMode, RealFilesystem};
    use rudb_vector::{Form, Vector};

    use crate::metadata::Metadata;
    use crate::page::Body;
    use crate::{Pages, SchemaColumn};

    /// The file DuckDB wrote, the same one the footer and the page tests read.
    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/mixed.parquet")
    }

    fn open() -> Box<dyn File> {
        RealFilesystem::new().open(&fixture(), OpenMode::Read).expect("the fixture is committed")
    }

    /// Every value of one column of the file, read the way a scan would read it.
    ///
    /// One dictionary per chunk, decoded from the chunk's first page and handed to every data page
    /// after it, which is the arrangement the format is built around and the reason the dictionary
    /// is a parameter rather than something a page finds for itself.
    fn column(at: usize) -> (SchemaColumn, Vec<Vector>) {
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let schema = metadata.schema[at].clone();
        let mut out = Vec::new();
        for group in &metadata.row_groups {
            let chunk = &group.columns[at];
            let mut bytes = vec![0u8; chunk.compressed_size as usize];
            file.read_at(chunk.start(), &mut bytes).expect("the chunk is in the file");
            let mut dictionary = None;
            for page in Pages::new(&bytes, chunk.compression, chunk.values) {
                let mut page = page.expect("every page of the fixture walks");
                if matches!(page.header.body, Body::Dictionary(_)) {
                    dictionary = Some(Arc::new(
                        page.decode_dictionary(&schema).expect("the dictionary decodes"),
                    ));
                    continue;
                }
                out.push(page.decode(&schema, dictionary.as_ref()).expect("the values decode"));
            }
        }
        (schema, out)
    }

    /// Every value of a column, in order, nulls included.
    fn values(at: usize) -> Vec<Value> {
        column(at).1.iter().flat_map(Vector::iter).collect()
    }

    #[test]
    fn every_page_of_a_chunk_points_at_one_dictionary_rather_than_a_copy_of_it() {
        // The whole reason `decode` takes a handle. This is the assertion that stops a future
        // signature change from quietly putting the copy back, since a copy is not a wrong answer
        // and no other test in this file would notice one.
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let mut checked = 0;
        for (at, schema) in metadata.schema.iter().enumerate() {
            for group in &metadata.row_groups {
                let chunk = &group.columns[at];
                let mut bytes = vec![0u8; chunk.compressed_size as usize];
                file.read_at(chunk.start(), &mut bytes).expect("the chunk is in the file");
                let mut dictionary: Option<Arc<Vector>> = None;
                let mut pages = Vec::new();
                for page in Pages::new(&bytes, chunk.compression, chunk.values) {
                    let mut page = page.expect("every page of the fixture walks");
                    if matches!(page.header.body, Body::Dictionary(_)) {
                        let decoded =
                            page.decode_dictionary(schema).expect("the dictionary decodes");
                        dictionary = Some(Arc::new(decoded));
                        continue;
                    }
                    pages
                        .push(page.decode(schema, dictionary.as_ref()).expect("the values decode"));
                }
                let Some(dictionary) = dictionary else { continue };
                for page in &pages {
                    let (_, values) = page.dictionary_parts().expect("a dictionary encoded page");
                    assert!(
                        std::ptr::eq(values, dictionary.as_ref()),
                        "a page of column {} decoded its own copy of the dictionary",
                        schema.name
                    );
                }
                assert_eq!(Arc::strong_count(&dictionary), 1 + pages.len());
                checked += 1;
            }
        }
        assert!(checked > 0, "the fixture has a dictionary encoded column and none was walked");
    }

    #[test]
    fn the_integer_column_sums_to_what_duckdb_says_it_sums_to() {
        // `select count(*), sum(a), min(a), max(a) from mixed.parquet` in DuckDB on server2, which
        // is the only source of truth here that did not come out of this reader.
        let read: Vec<i64> = values(0)
            .into_iter()
            .map(|value| match value {
                Value::Integer(number) => i64::from(number),
                other => panic!("an integer column produced {other:?}"),
            })
            .collect();
        assert_eq!(read.len(), 4096);
        assert_eq!(read.iter().sum::<i64>(), 195_783);
        assert_eq!(read.iter().copied().min(), Some(0));
        assert_eq!(read.iter().copied().max(), Some(96));
    }

    #[test]
    fn the_big_integer_column_sums_to_what_duckdb_says_it_sums_to() {
        let read: Vec<i64> = values(1)
            .into_iter()
            .map(|value| match value {
                Value::BigInt(number) => number,
                other => panic!("a big integer column produced {other:?}"),
            })
            .collect();
        assert_eq!(read.len(), 4096);
        assert_eq!(i128::from(read.iter().sum::<i64>()), 2_002_560_000);
    }

    #[test]
    fn the_string_column_counts_and_bounds_match_duckdb() {
        // 3510 strings among 4096 rows, so 586 nulls, which is the 293 a row group the footer
        // counted. The bounds catch a dictionary read with the indices scrambled, which a count
        // on its own would not.
        let read = values(2);
        assert_eq!(read.len(), 4096);
        let text: Vec<String> = read
            .iter()
            .filter_map(|value| match value {
                Value::Varchar(text) => Some(text.clone()),
                Value::Null => None,
                other => panic!("a string column produced {other:?}"),
            })
            .collect();
        assert_eq!(text.len(), 3510);
        assert_eq!(text.iter().min().map(String::as_str), Some("tag0"));
        assert_eq!(text.iter().max().map(String::as_str), Some("tag4"));
    }

    #[test]
    fn the_double_column_sums_to_what_duckdb_says_it_sums_to() {
        let read: Vec<f64> = values(3)
            .into_iter()
            .map(|value| match value {
                Value::Double(number) => number,
                other => panic!("a double column produced {other:?}"),
            })
            .collect();
        assert_eq!(read.len(), 4096);
        assert!((read.iter().sum::<f64>() - 193_536.0).abs() < 1e-6);
    }

    #[test]
    fn the_boolean_column_has_the_true_count_duckdb_counted() {
        // Booleans are the one plain encoded type that is not a whole number of bytes, so a reader
        // with the bit order backwards still produces 4096 values and does not produce 2048 trues.
        let read: Vec<bool> = values(4)
            .into_iter()
            .map(|value| match value {
                Value::Boolean(flag) => flag,
                other => panic!("a boolean column produced {other:?}"),
            })
            .collect();
        assert_eq!(read.len(), 4096);
        assert_eq!(read.iter().filter(|&&flag| flag).count(), 2048);
    }

    #[test]
    fn a_date_and_a_timestamp_keep_the_bounds_duckdb_reports() {
        // Both are integers on the wire and neither is an integer to rudb, which is the annotation
        // path. The numbers are DuckDB's own bounds turned into its units: days for a date and
        // microseconds for a timestamp.
        let days: Vec<i32> = values(5)
            .into_iter()
            .map(|value| match value {
                Value::Date(day) => day,
                other => panic!("a date column produced {other:?}"),
            })
            .collect();
        assert_eq!(days.len(), 4096);
        assert_eq!(days.iter().copied().min(), Some(0));
        assert_eq!(days.iter().copied().max(), Some(999));
        let micros: Vec<i64> = values(6)
            .into_iter()
            .map(|value| match value {
                Value::Timestamp(at) => at,
                other => panic!("a timestamp column produced {other:?}"),
            })
            .collect();
        assert_eq!(micros.len(), 4096);
        assert_eq!(micros.iter().copied().min(), Some(1_373_882_400_000_000));
        assert_eq!(micros.iter().copied().max(), Some(1_373_883_299_000_000));
    }

    #[test]
    fn a_dictionary_encoded_column_stays_a_dictionary() {
        // The point of the whole arrangement. A string column that arrived as indices into five
        // distinct values does not become 2048 strings on the way in, because the vector layer has
        // a dictionary form and flattening it here would throw away the thing that makes a group
        // by on it fast.
        let (_, pages) = column(2);
        assert!(!pages.is_empty());
        for page in &pages {
            assert_eq!(page.form(), Form::Dictionary, "a string page came in flat");
            let (codes, distinct) = page.dictionary_parts().expect("a dictionary has parts");
            assert!(distinct.len() <= 8, "{} distinct values is not a dictionary", distinct.len());
            assert_eq!(codes.len(), page.len());
        }
    }

    #[test]
    fn the_strings_are_left_in_the_page_they_arrived_in() {
        // The other half of the point. The dictionary's values point into the page the dictionary
        // page decompressed to, and nothing copied them there.
        let (_, pages) = column(2);
        let (_, distinct) = pages[0].dictionary_parts().expect("a dictionary has parts");
        let text: Vec<Value> = distinct.iter().collect();
        assert!(text.iter().all(|value| matches!(value, Value::Varchar(_))));
        assert!(text.len() >= 2, "a dictionary of {} values", text.len());
    }

    #[test]
    fn every_column_comes_back_as_the_type_the_schema_said() {
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        for at in 0..metadata.schema.len() {
            let (schema, pages) = column(at);
            for page in &pages {
                assert_eq!(page.logical_type(), &schema.ty, "column {}", schema.name);
            }
        }
    }

    #[test]
    fn the_nulls_land_in_the_positions_the_levels_put_them() {
        // Reading only the values would pass with every null shifted to the end. Comparing against
        // the levels the same page decoded is what pins them in place.
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let schema = metadata.schema[2].clone();
        assert_eq!(schema.ty, LogicalType::Varchar);
        let chunk = &metadata.row_groups[0].columns[2];
        let mut bytes = vec![0u8; chunk.compressed_size as usize];
        file.read_at(chunk.start(), &mut bytes).expect("the chunk is in the file");
        let mut dictionary = None;
        let mut checked = 0;
        for page in Pages::new(&bytes, chunk.compression, chunk.values) {
            let mut page = page.expect("every page walks");
            if matches!(page.header.body, Body::Dictionary(_)) {
                dictionary = Some(Arc::new(
                    page.decode_dictionary(&schema).expect("the dictionary decodes"),
                ));
                continue;
            }
            let (levels, _) = page.definitions(true).expect("the levels decode");
            let vector = page.decode(&schema, dictionary.as_ref()).expect("the values decode");
            assert!(levels.is_empty() || levels.len() == vector.len());
            for at in 0..vector.len() {
                // No level for a position means the page said outright that it holds no nulls, so
                // the position is not null. That is a different thing to a level of zero and the
                // two have to keep landing on different answers here.
                let null = levels.get(at) == Some(&0);
                assert_eq!(
                    null,
                    matches!(vector.value_at(at), Value::Null),
                    "position {at} disagrees with its level"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 2048);
    }

    #[test]
    fn a_dictionary_encoded_page_with_no_dictionary_says_so() {
        let (schema, _) = column(2);
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let chunk = &metadata.row_groups[0].columns[2];
        let mut bytes = vec![0u8; chunk.compressed_size as usize];
        file.read_at(chunk.start(), &mut bytes).expect("the chunk is in the file");
        let mut data = Pages::new(&bytes, chunk.compression, chunk.values)
            .map(|page| page.expect("every page walks"))
            .find(|page| !matches!(page.header.body, Body::Dictionary(_)))
            .expect("the chunk has a data page");
        let error = data.decode(&schema, None).unwrap_err();
        assert!(error.message().contains("no dictionary page"), "{}", error.message());
    }

    #[test]
    fn asking_a_dictionary_page_for_rows_or_a_data_page_for_a_dictionary_is_an_error() {
        let (schema, _) = column(2);
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let chunk = &metadata.row_groups[0].columns[2];
        let mut bytes = vec![0u8; chunk.compressed_size as usize];
        file.read_at(chunk.start(), &mut bytes).expect("the chunk is in the file");
        let pages: Vec<_> = Pages::new(&bytes, chunk.compression, chunk.values)
            .map(|page| page.expect("every page walks"))
            .collect();
        let dictionary = pages
            .iter()
            .position(|page| matches!(page.header.body, Body::Dictionary(_)))
            .expect("the chunk has a dictionary page");
        let mut pages = pages;
        let mut data = pages.remove(dictionary + 1);
        let mut dictionary = pages.remove(dictionary);
        let error = dictionary.decode(&schema, None).unwrap_err();
        assert!(error.message().contains("holds no rows"), "{}", error.message());
        let error = data.decode_dictionary(&schema).unwrap_err();
        assert!(error.message().contains("asked of a data page"), "{}", error.message());
    }
}
