//! The Thrift compact protocol, as much of it as Parquet metadata uses.
//!
//! Parquet's footer is a Thrift structure written in the compact protocol, so reading a Parquet
//! file starts with reading Thrift. The workspace has no external dependencies, so this is written
//! rather than pulled in, and writing it is a smaller job than it sounds: the compact protocol is
//! varints, zigzag varints, a one byte field header that carries the field id as a delta, and four
//! container headers. That is the whole wire format. What makes a Thrift library large is the code
//! generator and the transport layer, and a reader that already knows which structure it is looking
//! at needs neither.
//!
//! This is a pull decoder rather than a deserializer. There is no derive and no intermediate tree,
//! because a tree would mean allocating a node per field of a footer that on `hits` has a hundred
//! and five columns in every row group, and every one of those nodes would be read once and dropped.
//! A caller reads a field header, matches on the field id, and asks for the value it knows is there.
//! An unknown field id is skipped by type, which is what makes a reader written against one version
//! of `parquet.thrift` keep working against a file written by a newer one.
//!
//! Everything here borrows from one slice. The footer is read into memory in one go, because it is
//! the one part of a Parquet file whose length is known before it is parsed and whose parts are not
//! worth a read each.

use rudb_common::{Error, Result};

/// What a field or a container element holds.
///
/// The numbers are the wire values, so the conversion from the low nibble of a field header is a
/// table lookup and not arithmetic. `BooleanTrue` and `BooleanFalse` are two types rather than one
/// because the compact protocol puts a boolean field's value in the type nibble, which saves the
/// byte that would otherwise hold it. Inside a list they mean something slightly different, which
/// [`Reader::read_bool`] handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A boolean field whose value is true, or the element type of a list of booleans.
    BooleanTrue,
    /// A boolean field whose value is false.
    BooleanFalse,
    /// One signed byte.
    Byte,
    /// A zigzag varint that the schema calls 16 bit.
    I16,
    /// A zigzag varint that the schema calls 32 bit.
    I32,
    /// A zigzag varint that the schema calls 64 bit.
    I64,
    /// Eight bytes, little endian.
    Double,
    /// A varint length and then that many bytes, which is both `binary` and `string`.
    Binary,
    /// A list header and then its elements.
    List,
    /// A set, written exactly like a list.
    Set,
    /// A map header and then its pairs.
    Map,
    /// A nested structure, ended by a stop byte.
    Struct,
}

impl Kind {
    /// The type in the low nibble of a field header or of a list header.
    fn from_wire(wire: u8) -> Result<Self> {
        Ok(match wire {
            1 => Self::BooleanTrue,
            2 => Self::BooleanFalse,
            3 => Self::Byte,
            4 => Self::I16,
            5 => Self::I32,
            6 => Self::I64,
            7 => Self::Double,
            8 => Self::Binary,
            9 => Self::List,
            10 => Self::Set,
            11 => Self::Map,
            12 => Self::Struct,
            other => return Err(Error::io(format!("a thrift type of {other}, which is not one"))),
        })
    }
}

/// One field header: which field, and what it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Field {
    /// The field id from `parquet.thrift`.
    pub(crate) id: i16,
    /// What the value is.
    pub(crate) kind: Kind,
}

/// A cursor over a Thrift compact structure.
///
/// The last field id is state because the protocol writes ids as deltas from it, which is where the
/// compactness comes from on a structure whose fields are numbered from one and written in order.
/// A nested structure has its own numbering, so [`Reader::struct_begin`] saves the id and
/// [`Reader::struct_end`] puts it back, and a caller that parses a nested structure between those
/// two calls does not have to know that this is happening.
#[derive(Debug)]
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    last_id: i16,
}

impl<'a> Reader<'a> {
    /// A reader over a whole structure, positioned at its first field header.
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0, last_id: 0 }
    }

    /// How many bytes have been consumed.
    ///
    /// A page header is the one structure in a Parquet file whose length nothing records. It is
    /// written immediately in front of the page it describes, so the only way to find where the
    /// page starts is to decode the header and ask how far that got.
    pub(crate) fn position(&self) -> usize {
        self.at
    }

    /// The next field of the current structure, or nothing when the stop byte is reached.
    ///
    /// # Errors
    ///
    /// If the bytes run out, or if the type nibble is not a type.
    pub(crate) fn field_begin(&mut self) -> Result<Option<Field>> {
        let header = self.byte()?;
        if header == 0 {
            return Ok(None);
        }
        let kind = Kind::from_wire(header & 0x0f)?;
        let delta = i16::from(header >> 4);
        let id = if delta == 0 { self.zigzag()? as i16 } else { self.last_id + delta };
        self.last_id = id;
        Ok(Some(Field { id, kind }))
    }

    /// Enters a nested structure, returning the field numbering to restore afterwards.
    pub(crate) fn struct_begin(&mut self) -> i16 {
        let saved = self.last_id;
        self.last_id = 0;
        saved
    }

    /// Leaves a nested structure, restoring the numbering [`Reader::struct_begin`] returned.
    pub(crate) fn struct_end(&mut self, saved: i16) {
        self.last_id = saved;
    }

    /// Reads a boolean field's value.
    ///
    /// The value is in the field header for a field, which is why the header is the argument, and
    /// a list of booleans writes one byte an element instead. A list element arrives here as
    /// [`Kind::BooleanTrue`], because that is what the element type nibble of such a list says, and
    /// the byte still has to be read.
    ///
    /// # Errors
    ///
    /// If a list element's byte is missing.
    pub(crate) fn read_bool(&mut self, kind: Kind, in_list: bool) -> Result<bool> {
        if in_list {
            return Ok(self.byte()? != 0);
        }
        Ok(kind == Kind::BooleanTrue)
    }

    /// Reads one signed byte.
    ///
    /// # Errors
    ///
    /// If the bytes run out.
    pub(crate) fn read_byte(&mut self) -> Result<i8> {
        Ok(self.byte()? as i8)
    }

    /// Reads a zigzag varint.
    ///
    /// The three integer widths are one function because they are one wire format. A file that
    /// writes a value too large for the field's declared width is a file whose writer is wrong, and
    /// the caller narrows where it matters rather than every read paying for a range check.
    ///
    /// # Errors
    ///
    /// If the bytes run out or the varint does not end within ten bytes.
    pub(crate) fn read_int(&mut self) -> Result<i64> {
        self.zigzag()
    }

    /// Reads a length prefixed run of bytes, which is `binary` and `string` both.
    ///
    /// The result borrows from the footer rather than copying, because most of these are column
    /// names that the caller turns into one `String` each and statistics that the caller looks at
    /// and mostly throws away.
    ///
    /// # Errors
    ///
    /// If the bytes run out or the length is negative.
    pub(crate) fn read_binary(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()?;
        let len = usize::try_from(len)
            .map_err(|_| Error::io(format!("a thrift binary of {len} bytes")))?;
        self.take(len)
    }

    /// Reads a length prefixed string, as far as one is valid UTF-8.
    ///
    /// # Errors
    ///
    /// If the bytes run out or the bytes are not UTF-8. Parquet writes column names as `string`,
    /// and a name that is not UTF-8 is a file this reader cannot describe rather than one it should
    /// guess at.
    pub(crate) fn read_string(&mut self) -> Result<&'a str> {
        let bytes = self.read_binary()?;
        std::str::from_utf8(bytes).map_err(|_| Error::io("a thrift string that is not UTF-8"))
    }

    /// Reads a list or set header: how many elements and of what.
    ///
    /// # Errors
    ///
    /// If the bytes run out or the element type is not a type.
    pub(crate) fn list_begin(&mut self) -> Result<(usize, Kind)> {
        let header = self.byte()?;
        let kind = Kind::from_wire(header & 0x0f)?;
        let short = usize::from(header >> 4);
        let len = if short == 15 {
            let len = self.varint()?;
            usize::try_from(len).map_err(|_| Error::io(format!("a thrift list of {len}")))?
        } else {
            short
        };
        Ok((len, kind))
    }

    /// Reads a map header: how many pairs, and of what key and value.
    ///
    /// An empty map is one zero byte and no type nibbles at all, which is why the types come back
    /// as [`Kind::Byte`] in that case and the caller is expected not to ask for any pairs.
    ///
    /// # Errors
    ///
    /// If the bytes run out or a type nibble is not a type.
    pub(crate) fn map_begin(&mut self) -> Result<(usize, Kind, Kind)> {
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| Error::io(format!("a thrift map of {len}")))?;
        if len == 0 {
            return Ok((0, Kind::Byte, Kind::Byte));
        }
        let types = self.byte()?;
        Ok((len, Kind::from_wire(types >> 4)?, Kind::from_wire(types & 0x0f)?))
    }

    /// Steps over a value of the given type without interpreting it.
    ///
    /// This is what makes the reader survive a newer writer. A field id this version does not know
    /// is a field whose bytes still have to be walked, and walking them needs the type and nothing
    /// else, which is the property the compact protocol was designed to have.
    ///
    /// # Errors
    ///
    /// If the bytes run out or a nested type is not a type.
    pub(crate) fn skip(&mut self, kind: Kind) -> Result<()> {
        match kind {
            Kind::BooleanTrue | Kind::BooleanFalse => {}
            Kind::Byte => {
                self.byte()?;
            }
            Kind::I16 | Kind::I32 | Kind::I64 => {
                self.zigzag()?;
            }
            Kind::Double => {
                self.take(8)?;
            }
            Kind::Binary => {
                self.read_binary()?;
            }
            Kind::List | Kind::Set => {
                let (len, element) = self.list_begin()?;
                for _ in 0..len {
                    self.skip_element(element)?;
                }
            }
            Kind::Map => {
                let (len, key, value) = self.map_begin()?;
                for _ in 0..len {
                    self.skip_element(key)?;
                    self.skip_element(value)?;
                }
            }
            Kind::Struct => {
                let saved = self.struct_begin();
                while let Some(field) = self.field_begin()? {
                    self.skip(field.kind)?;
                }
                self.struct_end(saved);
            }
        }
        Ok(())
    }

    /// Steps over one element of a container.
    ///
    /// The difference from [`Reader::skip`] is the boolean, which inside a container occupies a
    /// byte rather than living in a type nibble.
    fn skip_element(&mut self, kind: Kind) -> Result<()> {
        if matches!(kind, Kind::BooleanTrue | Kind::BooleanFalse) {
            self.byte()?;
            return Ok(());
        }
        self.skip(kind)
    }

    /// One byte, or an error saying the structure ended in the middle of itself.
    fn byte(&mut self) -> Result<u8> {
        let byte = *self
            .bytes
            .get(self.at)
            .ok_or_else(|| Error::io("thrift metadata that ends in the middle of a value"))?;
        self.at += 1;
        Ok(byte)
    }

    /// A run of bytes, or an error if there are not that many left.
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end =
            self.at.checked_add(len).ok_or_else(|| Error::io("a thrift length that wraps"))?;
        let bytes = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| Error::io(format!("a thrift value of {len} bytes past the end")))?;
        self.at = end;
        Ok(bytes)
    }

    /// An unsigned varint, seven bits a byte, low group first.
    fn varint(&mut self) -> Result<u64> {
        let mut value = 0_u64;
        let mut shift = 0;
        loop {
            let byte = self.byte()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift > 63 {
                return Err(Error::io("a thrift varint longer than ten bytes"));
            }
        }
    }

    /// A zigzag varint, which is how the compact protocol writes a signed integer.
    ///
    /// Zigzag maps a small negative number to a small unsigned one, so that minus one is one byte
    /// rather than ten. Parquet leans on it: an offset is positive and a field that is absent is
    /// absent rather than minus one, but the levels and the statistics are full of small signed
    /// values.
    fn zigzag(&mut self) -> Result<i64> {
        let raw = self.varint()?;
        Ok(((raw >> 1) as i64) ^ -((raw & 1) as i64))
    }
}

/// A Thrift compact writer, for tests.
///
/// Writing one is cheaper than committing a fixture for every shape, and it is the only
/// Thrift writer in the workspace, because rudb writes Parquet at 2m and not here. It sits
/// here rather than inside `tests` because the page header tests need it too, and two
/// encoders that have to agree is one more than the number worth having.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Writer {
    bytes: Vec<u8>,
    last_id: i16,
}

#[cfg(test)]
impl Writer {
    pub(crate) fn varint(&mut self, mut value: u64) {
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

    pub(crate) fn zigzag(&mut self, value: i64) {
        self.varint(((value << 1) ^ (value >> 63)) as u64);
    }

    pub(crate) fn field(&mut self, id: i16, wire: u8) {
        let delta = id - self.last_id;
        if delta > 0 && delta <= 15 {
            self.bytes.push(((delta as u8) << 4) | wire);
        } else {
            self.bytes.push(wire);
            self.zigzag(i64::from(id));
        }
        self.last_id = id;
    }

    pub(crate) fn int(&mut self, id: i16, value: i64) {
        self.field(id, 6);
        self.zigzag(value);
    }

    pub(crate) fn string(&mut self, id: i16, value: &str) {
        self.field(id, 8);
        self.varint(value.len() as u64);
        self.bytes.extend_from_slice(value.as_bytes());
    }

    pub(crate) fn boolean(&mut self, id: i16, value: bool) {
        self.field(id, if value { 1 } else { 2 });
    }

    pub(crate) fn list_of_ints(&mut self, id: i16, values: &[i64]) {
        self.field(id, 9);
        if values.len() < 15 {
            self.bytes.push(((values.len() as u8) << 4) | 5);
        } else {
            self.bytes.push(0xf5);
            self.varint(values.len() as u64);
        }
        for &value in values {
            self.zigzag(value);
        }
    }

    pub(crate) fn nested(&mut self, id: i16, inner: Writer) {
        self.field(id, 12);
        self.bytes.extend_from_slice(&inner.bytes);
        self.bytes.push(0);
    }

    pub(crate) fn stop(mut self) -> Vec<u8> {
        self.bytes.push(0);
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::{Kind, Reader, Writer};

    #[test]
    fn a_field_id_is_a_delta_until_it_is_too_far() {
        let mut writer = Writer::default();
        writer.int(1, 7);
        writer.int(2, 8);
        writer.int(40, 9);
        let bytes = writer.stop();
        let mut reader = Reader::new(&bytes);
        let mut seen = Vec::new();
        while let Some(field) = reader.field_begin().expect("reads a field header") {
            seen.push((field.id, reader.read_int().expect("reads an int")));
        }
        assert_eq!(seen, vec![(1, 7), (2, 8), (40, 9)]);
    }

    #[test]
    fn every_value_comes_back_as_it_went_in() {
        let mut writer = Writer::default();
        writer.boolean(1, true);
        writer.boolean(2, false);
        writer.int(3, -1);
        writer.int(4, i64::MIN);
        writer.string(5, "SearchPhrase");
        writer.list_of_ints(6, &[0, 2, 3, 8]);
        let bytes = writer.stop();
        let mut reader = Reader::new(&bytes);
        let mut answers = Vec::new();
        while let Some(field) = reader.field_begin().expect("reads a field header") {
            answers.push(match field.id {
                1 | 2 => format!("{}", reader.read_bool(field.kind, false).expect("a boolean")),
                3 | 4 => format!("{}", reader.read_int().expect("an int")),
                5 => reader.read_string().expect("a string").to_string(),
                _ => {
                    let (len, kind) = reader.list_begin().expect("a list header");
                    assert_eq!(kind, Kind::I32);
                    let mut values = Vec::new();
                    for _ in 0..len {
                        values.push(reader.read_int().expect("an element"));
                    }
                    format!("{values:?}")
                }
            });
        }
        assert_eq!(
            answers,
            vec![
                "true".to_string(),
                "false".to_string(),
                "-1".to_string(),
                i64::MIN.to_string(),
                "SearchPhrase".to_string(),
                "[0, 2, 3, 8]".to_string(),
            ]
        );
    }

    #[test]
    fn a_long_list_writes_its_length_separately() {
        let values: Vec<i64> = (0..100).collect();
        let mut writer = Writer::default();
        writer.list_of_ints(1, &values);
        let bytes = writer.stop();
        let mut reader = Reader::new(&bytes);
        reader.field_begin().expect("reads a field header").expect("has a field");
        let (len, _) = reader.list_begin().expect("a list header");
        assert_eq!(len, 100);
        let mut back = Vec::new();
        for _ in 0..len {
            back.push(reader.read_int().expect("an element"));
        }
        assert_eq!(back, values);
    }

    #[test]
    fn a_nested_structure_numbers_its_own_fields() {
        let mut inner = Writer::default();
        inner.int(1, 11);
        inner.string(2, "inner");
        let mut writer = Writer::default();
        writer.int(3, 33);
        writer.nested(4, inner);
        writer.int(5, 55);
        let bytes = writer.stop();
        let mut reader = Reader::new(&bytes);
        let mut outer = Vec::new();
        let mut within = Vec::new();
        while let Some(field) = reader.field_begin().expect("reads a field header") {
            match field.id {
                4 => {
                    let saved = reader.struct_begin();
                    while let Some(field) = reader.field_begin().expect("reads a nested header") {
                        within.push(match field.id {
                            1 => reader.read_int().expect("an int").to_string(),
                            _ => reader.read_string().expect("a string").to_string(),
                        });
                    }
                    reader.struct_end(saved);
                }
                id => outer.push((id, reader.read_int().expect("an int"))),
            }
        }
        assert_eq!(outer, vec![(3, 33), (5, 55)]);
        assert_eq!(within, vec!["11".to_string(), "inner".to_string()]);
    }

    #[test]
    fn a_field_nobody_asked_about_is_stepped_over() {
        let mut inner = Writer::default();
        inner.list_of_ints(1, &[1, 2, 3]);
        inner.string(2, "a field from a later version");
        let mut writer = Writer::default();
        writer.nested(1, inner);
        writer.int(2, 42);
        let bytes = writer.stop();
        let mut reader = Reader::new(&bytes);
        let first = reader.field_begin().expect("reads a field header").expect("has a field");
        reader.skip(first.kind).expect("steps over the structure");
        let second = reader.field_begin().expect("reads a field header").expect("has a field");
        assert_eq!(second.id, 2);
        assert_eq!(reader.read_int().expect("an int"), 42);
        assert!(reader.field_begin().expect("reads the stop byte").is_none());
    }

    #[test]
    fn bytes_that_end_early_are_an_error_and_not_a_panic() {
        let mut writer = Writer::default();
        writer.string(1, "SearchPhrase");
        let bytes = writer.stop();
        for cut in 1..bytes.len() {
            let mut reader = Reader::new(&bytes[..cut]);
            let mut fields = 0;
            let outcome = loop {
                match reader.field_begin() {
                    Err(e) => break Err(e),
                    Ok(None) => break Ok(fields),
                    Ok(Some(field)) => {
                        fields += 1;
                        if let Err(e) = reader.skip(field.kind) {
                            break Err(e);
                        }
                    }
                }
            };
            // Every prefix of a structure either ends in an error or is a shorter structure, and
            // the only prefix that is a shorter structure here is the one byte the field header
            // takes before the string that follows it is missing.
            assert!(outcome.is_err(), "a prefix of {cut} bytes parsed as a whole structure");
        }
    }
}
