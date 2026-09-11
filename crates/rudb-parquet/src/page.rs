//! Page headers, which are the only structure in a Parquet file whose length nothing records.
//!
//! A column chunk is a run of pages with no index in front of it. Each page is a Thrift compact
//! `PageHeader` followed immediately by the page's bytes, and the header says how long the bytes
//! are but nothing says how long the header is. So walking a chunk means decoding a header,
//! asking the Thrift reader how far it got, stepping over the body by the length the header gave,
//! and doing it again. That is why [`Header::read`] hands back both the header and its length.
//!
//! # Two versions of the data page, and why both are here
//!
//! Version one puts the definition levels, the repetition levels and the values in one buffer and
//! compresses the whole thing, so a reader cannot look at the levels without decompressing the
//! values. Version two keeps the levels outside the compressed region and states their lengths,
//! which is what lets a reader decide from the levels alone that it does not want the values.
//!
//! Version two also states the null count and the row count per page, which version one does not,
//! so a page of a nullable column in version one has to have its definition levels counted to
//! find out how many values follow. That difference is not cosmetic and it is the reason the two
//! headers are separate types rather than one with optional fields.
//!
//! DuckDB writes version one. pyarrow writes version one by default and version two on request.
//! parquet-java writes version two more often. All four are in the corpus, so both are read.

use rudb_common::{Error, Result};

use crate::metadata::{Encoding, Stats, read_stats};
use crate::thrift::Reader;

/// What a page holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// Values, with their levels in the same compressed buffer.
    DataV1(DataV1),
    /// Values, with their levels outside the compressed region.
    DataV2(DataV2),
    /// The distinct values the indices in the data pages point into.
    Dictionary(Dictionary),
    /// The page type the format defined and no writer ever wrote.
    Index,
}

/// A version one data page header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataV1 {
    /// How many values, counting the nulls, which are values with a level and no bytes.
    pub values: i32,
    /// How the values are encoded.
    pub encoding: Encoding,
    /// How the definition levels are encoded, which is `Rle` from every writer this decade.
    pub definition_encoding: Encoding,
    /// How the repetition levels are encoded.
    pub repetition_encoding: Encoding,
    /// What the writer said about the page, where it said anything.
    pub stats: Option<Stats>,
}

/// A version two data page header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataV2 {
    /// How many values, counting the nulls.
    pub values: i32,
    /// How many of them are null, which version one leaves to be counted.
    pub nulls: i32,
    /// How many rows, which differs from the value count only for a repeated column.
    pub rows: i32,
    /// How the values are encoded.
    pub encoding: Encoding,
    /// How many bytes of definition levels sit in front of the values, outside the compression.
    pub definition_bytes: i32,
    /// How many bytes of repetition levels sit in front of those.
    pub repetition_bytes: i32,
    /// Whether the values after the levels are compressed. Absent means they are.
    pub compressed: bool,
    /// What the writer said about the page.
    pub stats: Option<Stats>,
}

/// A dictionary page header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dictionary {
    /// How many distinct values the page holds.
    pub values: i32,
    /// How they are encoded, which is `Plain` or the older `PlainDictionary` spelling of it.
    pub encoding: Encoding,
    /// Whether the writer sorted them, which a reader can use and must not assume.
    pub sorted: bool,
}

/// One page's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// How many bytes the page body is on disk.
    pub compressed_size: i32,
    /// How many bytes it is after decompression, which is what the codec is told to expect.
    pub uncompressed_size: i32,
    /// The CRC of the page body, where the writer emitted one.
    pub crc: Option<i32>,
    /// What kind of page this is and what that kind says.
    pub body: Body,
}

impl Header {
    /// Reads the header at the front of `bytes`, and says how many bytes it took.
    ///
    /// # Errors
    ///
    /// If the bytes are not a well formed header, if the page type is not one Parquet defines, or
    /// if a size is negative. A negative size is the interesting one: it is the corruption that
    /// turns into an enormous read rather than into a visible failure, since the reader that adds
    /// it to an offset lands somewhere else in the file and decodes whatever is there.
    pub fn read(bytes: &[u8]) -> Result<(Self, usize)> {
        let mut reader = Reader::new(bytes);
        let mut kind: Option<i64> = None;
        let mut uncompressed_size = None;
        let mut compressed_size = None;
        let mut crc = None;
        let mut v1 = None;
        let mut v2 = None;
        let mut dictionary = None;

        while let Some(field) = reader.field_begin()? {
            match field.id {
                1 => kind = Some(reader.read_int()?),
                2 => uncompressed_size = Some(reader.read_int()?),
                3 => compressed_size = Some(reader.read_int()?),
                4 => crc = Some(reader.read_int()?),
                5 => v1 = Some(read_v1(&mut reader)?),
                // `IndexPageHeader` is an empty structure in every version of the schema, so
                // there is nothing to read out of it and the page type is what says it was there.
                6 => skip_struct(&mut reader)?,
                7 => dictionary = Some(read_dictionary(&mut reader)?),
                8 => v2 = Some(read_v2(&mut reader)?),
                _ => reader.skip(field.kind)?,
            }
        }

        let kind = kind.ok_or_else(|| Error::io("a page header with no page type".to_string()))?;
        // The body is taken by the type field rather than by which structure turned up, because a
        // writer is free to emit a structure the type does not name and a reader that went by
        // presence would decode a version one page as a version two one.
        let body = match kind {
            0 => Body::DataV1(v1.ok_or_else(|| missing("a data page", "a DataPageHeader"))?),
            1 => Body::Index,
            2 => Body::Dictionary(
                dictionary.ok_or_else(|| missing("a dictionary page", "a DictionaryPageHeader"))?,
            ),
            3 => Body::DataV2(v2.ok_or_else(|| missing("a v2 data page", "a DataPageHeaderV2"))?),
            other => {
                return Err(Error::io(format!("a page type of {other}, which is not one")));
            }
        };

        Ok((
            Self {
                compressed_size: size(compressed_size, "compressed")?,
                uncompressed_size: size(uncompressed_size, "uncompressed")?,
                crc: crc.map(|value| value as i32),
                body,
            },
            reader.position(),
        ))
    }

    /// How many values the page holds, for the two kinds that hold any.
    pub fn values(&self) -> i32 {
        match &self.body {
            Body::DataV1(page) => page.values,
            Body::DataV2(page) => page.values,
            Body::Dictionary(page) => page.values,
            Body::Index => 0,
        }
    }
}

fn read_v1(reader: &mut Reader<'_>) -> Result<DataV1> {
    let saved = reader.struct_begin();
    let mut values = None;
    let mut encoding = None;
    let mut definition_encoding = None;
    let mut repetition_encoding = None;
    let mut stats = None;
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => values = Some(reader.read_int()?),
            2 => encoding = Some(Encoding::from_wire(reader.read_int()?)?),
            3 => definition_encoding = Some(Encoding::from_wire(reader.read_int()?)?),
            4 => repetition_encoding = Some(Encoding::from_wire(reader.read_int()?)?),
            5 => stats = Some(read_stats(reader)?),
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok(DataV1 {
        values: count(values, "a data page")?,
        encoding: encoding.ok_or_else(|| missing("a data page", "an encoding"))?,
        // A flat required column is written with levels nobody reads, and some writers leave the
        // level encoding out rather than saying `RLE` for a level that is always zero. Defaulting
        // is right here and only here, because the width of such a level is zero and an encoding
        // for zero bits of data is the same whichever name it goes by.
        definition_encoding: definition_encoding.unwrap_or(Encoding::Rle),
        repetition_encoding: repetition_encoding.unwrap_or(Encoding::Rle),
        stats,
    })
}

fn read_v2(reader: &mut Reader<'_>) -> Result<DataV2> {
    let saved = reader.struct_begin();
    let mut values = None;
    let mut nulls = None;
    let mut rows = None;
    let mut encoding = None;
    let mut definition_bytes = None;
    let mut repetition_bytes = None;
    let mut compressed = None;
    let mut stats = None;
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => values = Some(reader.read_int()?),
            2 => nulls = Some(reader.read_int()?),
            3 => rows = Some(reader.read_int()?),
            4 => encoding = Some(Encoding::from_wire(reader.read_int()?)?),
            5 => definition_bytes = Some(reader.read_int()?),
            6 => repetition_bytes = Some(reader.read_int()?),
            7 => compressed = Some(reader.read_bool(field.kind, false)?),
            8 => stats = Some(read_stats(reader)?),
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok(DataV2 {
        values: count(values, "a v2 data page")?,
        nulls: count(nulls, "a v2 data page's null count")?,
        rows: count(rows, "a v2 data page's row count")?,
        encoding: encoding.ok_or_else(|| missing("a v2 data page", "an encoding"))?,
        definition_bytes: count(definition_bytes, "a v2 data page's definition levels")?,
        repetition_bytes: count(repetition_bytes, "a v2 data page's repetition levels")?,
        // The schema's default is true, so an absent field means compressed. A reader that took
        // absent as false would hand a compressed page straight to the decoder.
        compressed: compressed.unwrap_or(true),
        stats,
    })
}

fn read_dictionary(reader: &mut Reader<'_>) -> Result<Dictionary> {
    let saved = reader.struct_begin();
    let mut values = None;
    let mut encoding = None;
    let mut sorted = None;
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => values = Some(reader.read_int()?),
            2 => encoding = Some(Encoding::from_wire(reader.read_int()?)?),
            3 => sorted = Some(reader.read_bool(field.kind, false)?),
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok(Dictionary {
        values: count(values, "a dictionary page")?,
        encoding: encoding.ok_or_else(|| missing("a dictionary page", "an encoding"))?,
        sorted: sorted.unwrap_or(false),
    })
}

/// Steps over a structure whose contents nothing needs.
fn skip_struct(reader: &mut Reader<'_>) -> Result<()> {
    let saved = reader.struct_begin();
    while let Some(field) = reader.field_begin()? {
        reader.skip(field.kind)?;
    }
    reader.struct_end(saved);
    Ok(())
}

/// A page size, which the header states as a signed integer and which cannot be negative.
fn size(value: Option<i64>, which: &str) -> Result<i32> {
    let value = value.ok_or_else(|| Error::io(format!("a page header with no {which} size")))?;
    i32::try_from(value)
        .ok()
        .filter(|size| *size >= 0)
        .ok_or_else(|| Error::io(format!("a page whose {which} size is {value}")))
}

/// A value count, which cannot be negative either.
fn count(value: Option<i64>, what: &str) -> Result<i32> {
    let value = value.ok_or_else(|| Error::io(format!("{what} with no count")))?;
    i32::try_from(value)
        .ok()
        .filter(|count| *count >= 0)
        .ok_or_else(|| Error::io(format!("{what} holding {value} values")))
}

fn missing(page: &str, what: &str) -> Error {
    Error::io(format!("{page} whose header has no {what}"))
}

#[cfg(test)]
mod tests {
    use super::{Body, Header};
    use crate::metadata::Encoding;
    use crate::thrift::Writer;

    /// A `PageHeader` around a nested body, which is the shape every one of these has.
    fn header(kind: i64, uncompressed: i64, compressed: i64, field: i16, body: Writer) -> Vec<u8> {
        let mut writer = Writer::default();
        writer.int(1, kind);
        writer.int(2, uncompressed);
        writer.int(3, compressed);
        writer.nested(field, body);
        writer.stop()
    }

    fn v1_body(values: i64, encoding: i64) -> Writer {
        let mut body = Writer::default();
        body.int(1, values);
        body.int(2, encoding);
        body.int(3, 3);
        body.int(4, 3);
        body
    }

    #[test]
    fn a_version_one_data_page_reads_and_says_how_long_its_header_was() {
        // The length is the point. Nothing in the file records it, so a reader that could not
        // produce it could not find the second page of a chunk.
        let bytes = header(0, 4096, 1200, 5, v1_body(2048, 8));
        let (page, len) = Header::read(&bytes).expect("a well formed header");
        assert_eq!(len, bytes.len(), "the header is the whole of these bytes");
        assert_eq!(page.compressed_size, 1200);
        assert_eq!(page.uncompressed_size, 4096);
        assert_eq!(page.values(), 2048);
        match page.body {
            Body::DataV1(body) => {
                assert_eq!(body.encoding, Encoding::RleDictionary);
                assert_eq!(body.definition_encoding, Encoding::Rle);
                assert!(body.stats.is_none());
            }
            other => panic!("read as {other:?}"),
        }
    }

    #[test]
    fn a_header_followed_by_a_page_stops_at_the_end_of_the_header() {
        // What walking a chunk actually does, and the case that catches a reader which decoded
        // the header out of a slice sized to the header instead of to the rest of the chunk.
        let mut bytes = header(0, 64, 64, 5, v1_body(8, 0));
        let header_len = bytes.len();
        bytes.extend_from_slice(&[0xab; 64]);
        let (_, len) = Header::read(&bytes).expect("a well formed header");
        assert_eq!(len, header_len);
    }

    #[test]
    fn a_version_two_data_page_keeps_its_level_lengths_and_its_null_count() {
        let mut body = Writer::default();
        body.int(1, 2048);
        body.int(2, 17);
        body.int(3, 2048);
        body.int(4, 0);
        body.int(5, 260);
        body.int(6, 0);
        let bytes = header(3, 8192, 3000, 8, body);
        let (page, _) = Header::read(&bytes).expect("a well formed header");
        match page.body {
            Body::DataV2(body) => {
                assert_eq!(body.values, 2048);
                assert_eq!(body.nulls, 17);
                assert_eq!(body.rows, 2048);
                assert_eq!(body.definition_bytes, 260);
                assert_eq!(body.repetition_bytes, 0);
                assert!(body.compressed, "absent means compressed, which is the schema's default");
            }
            other => panic!("read as {other:?}"),
        }
    }

    #[test]
    fn a_version_two_page_that_says_it_is_not_compressed_is_believed() {
        let mut body = Writer::default();
        body.int(1, 8);
        body.int(2, 0);
        body.int(3, 8);
        body.int(4, 0);
        body.int(5, 0);
        body.int(6, 0);
        body.boolean(7, false);
        let bytes = header(3, 64, 64, 8, body);
        let (page, _) = Header::read(&bytes).expect("a well formed header");
        match page.body {
            Body::DataV2(body) => assert!(!body.compressed),
            other => panic!("read as {other:?}"),
        }
    }

    #[test]
    fn a_dictionary_page_reads_its_count_and_defaults_to_unsorted() {
        let mut body = Writer::default();
        body.int(1, 512);
        body.int(2, 0);
        let bytes = header(2, 9000, 3400, 7, body);
        let (page, _) = Header::read(&bytes).expect("a well formed header");
        match page.body {
            Body::Dictionary(body) => {
                assert_eq!(body.values, 512);
                assert_eq!(body.encoding, Encoding::Plain);
                assert!(!body.sorted, "unsorted unless the writer says otherwise");
            }
            other => panic!("read as {other:?}"),
        }
    }

    #[test]
    fn the_page_type_decides_and_not_which_structure_turned_up() {
        // A writer is free to emit a structure the type does not name. Going by presence would
        // read this as a version one page, which has different rules about where the levels are.
        let mut writer = Writer::default();
        writer.int(1, 3);
        writer.int(2, 64);
        writer.int(3, 64);
        writer.nested(5, v1_body(8, 0));
        let mut v2 = Writer::default();
        v2.int(1, 8);
        v2.int(2, 0);
        v2.int(3, 8);
        v2.int(4, 0);
        v2.int(5, 0);
        v2.int(6, 0);
        writer.nested(8, v2);
        let (page, _) = Header::read(&writer.stop()).expect("a well formed header");
        assert!(matches!(page.body, Body::DataV2(_)), "read as {:?}", page.body);
    }

    #[test]
    fn a_field_this_reader_does_not_know_is_skipped_by_type() {
        // Which is the whole point of a pull decoder over a generated one. A file from a newer
        // writer has fields this version has never heard of and has to stay readable.
        let mut writer = Writer::default();
        writer.int(1, 0);
        writer.int(2, 64);
        writer.int(3, 64);
        writer.nested(5, v1_body(8, 0));
        writer.string(40, "a field from a version that does not exist yet");
        writer.list_of_ints(41, &[1, 2, 3]);
        let (page, _) = Header::read(&writer.stop()).expect("a well formed header");
        assert_eq!(page.values(), 8);
    }

    #[test]
    fn a_page_type_that_is_not_one_is_an_error() {
        let bytes = header(9, 64, 64, 5, v1_body(8, 0));
        let error = Header::read(&bytes).unwrap_err();
        assert!(error.message().contains("page type of 9"), "{}", error.message());
    }

    #[test]
    fn a_page_that_claims_a_negative_size_is_refused() {
        // The corruption worth naming. A negative size added to an offset lands somewhere else in
        // the file, and what is decoded there is bytes that are not this page.
        let bytes = header(0, 64, -1, 5, v1_body(8, 0));
        let error = Header::read(&bytes).unwrap_err();
        assert!(error.message().contains("compressed size is -1"), "{}", error.message());
    }

    #[test]
    fn a_data_page_with_no_data_page_header_is_an_error_rather_than_a_default() {
        let mut writer = Writer::default();
        writer.int(1, 0);
        writer.int(2, 64);
        writer.int(3, 64);
        let error = Header::read(&writer.stop()).unwrap_err();
        assert!(error.message().contains("DataPageHeader"), "{}", error.message());
    }

    #[test]
    fn a_header_with_no_page_type_is_an_error() {
        let mut writer = Writer::default();
        writer.int(2, 64);
        writer.int(3, 64);
        writer.nested(5, v1_body(8, 0));
        let error = Header::read(&writer.stop()).unwrap_err();
        assert!(error.message().contains("no page type"), "{}", error.message());
    }

    #[test]
    fn every_prefix_of_a_header_is_an_error_rather_than_a_panic() {
        let mut body = Writer::default();
        body.int(1, 2048);
        body.int(2, 8);
        body.int(3, 3);
        body.int(4, 3);
        let bytes = header(0, 4096, 1200, 5, body);
        for cut in 0..bytes.len() {
            assert!(
                Header::read(&bytes[..cut]).is_err(),
                "a header cut at {cut} of {} read anyway",
                bytes.len()
            );
        }
    }
}
