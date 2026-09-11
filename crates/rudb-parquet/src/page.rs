//! Page headers: the Thrift structure in front of every page of a column chunk.
//!
//! A column chunk is a run of pages with no index in front of them. The footer says where the run
//! starts and how long it is, and finding the second page means decoding the first page's header,
//! which states both sizes, and stepping over that much. So a reader cannot seek to page five
//! without reading pages one to four's headers, and that is why a scan reads a whole chunk rather
//! than a page of one. The page index in `ColumnIndex` and `OffsetIndex` is what fixes that, and it
//! is E2 work because it only pays off alongside a predicate to prune with.
//!
//! Three kinds of page matter. A dictionary page holds the distinct values of the chunk and comes
//! first when it is there at all. A v1 data page holds its levels inside its own compressed body. A
//! v2 data page holds its levels in front, uncompressed, with their lengths in the header, which is
//! what lets a reader skip a page on a level predicate without decompressing it. An index page is a
//! thing the format defines and no writer emits, and it is stepped over here rather than rejected.
//!
//! The header states two sizes and neither is trusted. A page that claims to be longer than the
//! chunk that holds it is a corrupt file, and the check is at the point of use in
//! [`crate::column`] rather than here, because here there is nothing to check against.

use rudb_common::{Error, Result};

use crate::metadata::Encoding;
use crate::thrift::Reader;

/// What a page holds, with the fields that decoding it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kindof {
    /// A v1 data page: levels and values together in one compressed body.
    Data {
        /// How many values are in the page, counting the nulls.
        values: usize,
        /// How the values themselves are encoded.
        encoding: Encoding,
        /// How the definition levels are encoded, which is `RLE` on anything written this century.
        definition: Encoding,
    },
    /// A v2 data page: levels in front, uncompressed, then the compressed values.
    DataV2 {
        /// How many values are in the page, counting the nulls.
        values: usize,
        /// How many of them are null, which the writer counted so the reader does not have to.
        nulls: usize,
        /// How the values are encoded.
        encoding: Encoding,
        /// How many bytes the definition levels take, in front of the values.
        definition_bytes: usize,
        /// How many bytes the repetition levels take, in front of the definition levels.
        repetition_bytes: usize,
        /// Whether the values after the levels are compressed.
        compressed: bool,
    },
    /// The distinct values of the chunk.
    Dictionary {
        /// How many distinct values there are.
        values: usize,
        /// How they are encoded, which is `PLAIN` or the old spelling of it.
        encoding: Encoding,
    },
    /// A page kind this reader steps over, which today is the index page.
    Other,
}

/// One page header, and how long it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    /// What the page holds.
    pub(crate) kind: Kindof,
    /// How many bytes the page body takes in the file.
    pub(crate) compressed_size: usize,
    /// How many bytes the page body takes once decompressed.
    pub(crate) uncompressed_size: usize,
    /// How many bytes the header itself took, which is where the body begins.
    pub(crate) header_size: usize,
}

/// Decodes the page header at the front of `bytes`.
///
/// The header is a Thrift structure with no length in front of it, so decoding it is also how its
/// length is discovered, and [`Header::header_size`] is that length.
///
/// # Errors
///
/// If the bytes are not a `PageHeader`, or a size in it is negative, or the page type is not one
/// the format defines.
pub(crate) fn header(bytes: &[u8]) -> Result<Header> {
    let mut reader = Reader::new(bytes);
    let mut page_type = -1_i64;
    let mut uncompressed_size = 0_i64;
    let mut compressed_size = 0_i64;
    let mut data = None;
    let mut data_v2 = None;
    let mut dictionary = None;
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => page_type = reader.read_int()?,
            2 => uncompressed_size = reader.read_int()?,
            3 => compressed_size = reader.read_int()?,
            5 => data = Some(read_data(&mut reader)?),
            7 => dictionary = Some(read_dictionary(&mut reader)?),
            8 => data_v2 = Some(read_data_v2(&mut reader)?),
            _ => reader.skip(field.kind)?,
        }
    }
    let kind = match page_type {
        0 => data.ok_or_else(|| Error::io("a data page with no data page header"))?,
        2 => dictionary
            .ok_or_else(|| Error::io("a dictionary page with no dictionary page header"))?,
        3 => data_v2.ok_or_else(|| Error::io("a v2 data page with no v2 data page header"))?,
        1 => Kindof::Other,
        other => {
            return Err(Error::io(format!("a parquet page type of {other}, which is not one")));
        }
    };
    Ok(Header {
        kind,
        compressed_size: size(compressed_size, "compressed")?,
        uncompressed_size: size(uncompressed_size, "uncompressed")?,
        header_size: reader.position(),
    })
}

/// A size the header stated, checked for being a size.
fn size(value: i64, which: &str) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| Error::io(format!("a page of {value} {which} bytes, which is not a size")))
}

/// A count the header stated, checked for being a count.
fn count(value: i64, which: &str) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| Error::io(format!("a page of {value} {which}, which is not a count")))
}

/// Reads a `DataPageHeader`.
///
/// The repetition level encoding is read and dropped. A flat schema has no repetition levels at
/// all, the reader rejects a nested schema when it reads the footer, and a field that is always
/// absent does not need a place to be stored.
fn read_data(reader: &mut Reader<'_>) -> Result<Kindof> {
    let saved = reader.struct_begin();
    let mut values = 0;
    let mut encoding = Encoding::Plain;
    let mut definition = Encoding::Rle;
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => values = count(reader.read_int()?, "values")?,
            2 => encoding = Encoding::from_wire(reader.read_int()?)?,
            3 => definition = Encoding::from_wire(reader.read_int()?)?,
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok(Kindof::Data { values, encoding, definition })
}

/// Reads a `DataPageHeaderV2`.
///
/// `is_compressed` defaults to true when the field is absent, which is what the format says and the
/// opposite of what a Rust default would give.
fn read_data_v2(reader: &mut Reader<'_>) -> Result<Kindof> {
    let saved = reader.struct_begin();
    let mut values = 0;
    let mut nulls = 0;
    let mut encoding = Encoding::Plain;
    let mut definition_bytes = 0;
    let mut repetition_bytes = 0;
    let mut compressed = true;
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => values = count(reader.read_int()?, "values")?,
            2 => nulls = count(reader.read_int()?, "nulls")?,
            4 => encoding = Encoding::from_wire(reader.read_int()?)?,
            5 => definition_bytes = size(reader.read_int()?, "definition level")?,
            6 => repetition_bytes = size(reader.read_int()?, "repetition level")?,
            7 => compressed = reader.read_bool(field.kind, false)?,
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok(Kindof::DataV2 { values, nulls, encoding, definition_bytes, repetition_bytes, compressed })
}

/// Reads a `DictionaryPageHeader`.
fn read_dictionary(reader: &mut Reader<'_>) -> Result<Kindof> {
    let saved = reader.struct_begin();
    let mut values = 0;
    let mut encoding = Encoding::Plain;
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => values = count(reader.read_int()?, "values")?,
            2 => encoding = Encoding::from_wire(reader.read_int()?)?,
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok(Kindof::Dictionary { values, encoding })
}

#[cfg(test)]
mod tests {
    use super::{Kindof, header};
    use crate::metadata::Encoding;

    /// A minimal Thrift compact writer, enough to build the three page headers.
    #[derive(Debug, Default)]
    struct Writer {
        bytes: Vec<u8>,
        last_id: i16,
    }

    impl Writer {
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

        fn zigzag(&mut self, value: i64) {
            self.varint(((value << 1) ^ (value >> 63)) as u64);
        }

        fn field(&mut self, id: i16, wire: u8) {
            let delta = id - self.last_id;
            if delta > 0 && delta <= 15 {
                self.bytes.push(((delta as u8) << 4) | wire);
            } else {
                self.bytes.push(wire);
                self.zigzag(i64::from(id));
            }
            self.last_id = id;
        }

        fn int(&mut self, id: i16, value: i64) {
            self.field(id, 5);
            self.zigzag(value);
        }

        fn begin(&mut self, id: i16) -> i16 {
            self.field(id, 12);
            let saved = self.last_id;
            self.last_id = 0;
            saved
        }

        fn end(&mut self, saved: i16) {
            self.bytes.push(0);
            self.last_id = saved;
        }

        fn stop(&mut self) {
            self.bytes.push(0);
        }
    }

    #[test]
    fn a_v1_data_page_header_says_what_the_page_holds_and_how_long_it_is() {
        let mut writer = Writer::default();
        writer.int(1, 0);
        writer.int(2, 400);
        writer.int(3, 120);
        let saved = writer.begin(5);
        writer.int(1, 100);
        writer.int(2, 8);
        writer.int(3, 3);
        writer.int(4, 3);
        writer.end(saved);
        writer.stop();
        let header = header(&writer.bytes).expect("decodes");
        assert_eq!(header.compressed_size, 120);
        assert_eq!(header.uncompressed_size, 400);
        assert_eq!(header.header_size, writer.bytes.len());
        assert_eq!(
            header.kind,
            Kindof::Data {
                values: 100,
                encoding: Encoding::RleDictionary,
                definition: Encoding::Rle,
            }
        );
    }

    #[test]
    fn a_dictionary_page_header_is_a_count_and_an_encoding() {
        let mut writer = Writer::default();
        writer.int(1, 2);
        writer.int(2, 64);
        writer.int(3, 40);
        let saved = writer.begin(7);
        writer.int(1, 8);
        writer.int(2, 0);
        writer.end(saved);
        writer.stop();
        let header = header(&writer.bytes).expect("decodes");
        assert_eq!(header.kind, Kindof::Dictionary { values: 8, encoding: Encoding::Plain });
    }

    #[test]
    fn a_v2_data_page_defaults_to_compressed_when_the_field_is_absent() {
        let mut writer = Writer::default();
        writer.int(1, 3);
        writer.int(2, 400);
        writer.int(3, 120);
        let saved = writer.begin(8);
        writer.int(1, 100);
        writer.int(2, 7);
        writer.int(3, 100);
        writer.int(4, 0);
        writer.int(5, 13);
        writer.int(6, 0);
        writer.end(saved);
        writer.stop();
        let header = header(&writer.bytes).expect("decodes");
        assert_eq!(
            header.kind,
            Kindof::DataV2 {
                values: 100,
                nulls: 7,
                encoding: Encoding::Plain,
                definition_bytes: 13,
                repetition_bytes: 0,
                compressed: true,
            }
        );
    }

    #[test]
    fn an_index_page_is_stepped_over_rather_than_rejected() {
        let mut writer = Writer::default();
        writer.int(1, 1);
        writer.int(2, 16);
        writer.int(3, 16);
        writer.stop();
        let header = header(&writer.bytes).expect("decodes");
        assert_eq!(header.kind, Kindof::Other);
    }

    #[test]
    fn a_page_type_the_format_does_not_have_is_an_error() {
        let mut writer = Writer::default();
        writer.int(1, 9);
        writer.int(2, 16);
        writer.int(3, 16);
        writer.stop();
        let error = header(&writer.bytes).unwrap_err();
        assert!(error.to_string().contains("page type of 9"), "{error}");
    }

    #[test]
    fn a_data_page_with_no_data_page_header_is_an_error_rather_than_a_default() {
        let mut writer = Writer::default();
        writer.int(1, 0);
        writer.int(2, 16);
        writer.int(3, 16);
        writer.stop();
        let error = header(&writer.bytes).unwrap_err();
        assert!(error.to_string().contains("no data page header"), "{error}");
    }
}
