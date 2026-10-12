//! `string_agg` as PostgreSQL computes it, over `text` and over `bytea`.
//!
//! PostgreSQL reads the delimiter on each row and not once: `string_agg_transfn` adds the
//! delimiter of the row and then its value, and the final function drops the delimiter of the
//! first row. A row whose value is null adds nothing, and a row whose delimiter is null adds its
//! value with no delimiter in front. The pin wants one constant separator and answers null when it
//! is null. The binder gives a PostgreSQL session the names of these states, which are the names
//! of PostgreSQL's final functions.

use rudb_common::{Error, Result, Value};

/// The state of `string_agg_transfn` and of `bytea_string_agg_transfn`: the delimiters and the
/// values so far, and how long the delimiter in front is, which the answer leaves out.
#[derive(Debug, Clone)]
pub(crate) struct Delimited {
    bytes: Vec<u8>,
    head: usize,
    seen: bool,
    binary: bool,
}

impl Delimited {
    /// A fresh state for `name`, or `None` when the name is not one of these.
    pub(crate) fn named(name: &str) -> Option<Self> {
        let binary = match name {
            "string_agg_finalfn" => false,
            "bytea_string_agg_finalfn" => true,
            _ => return None,
        };
        Some(Self { bytes: Vec::new(), head: 0, seen: false, binary })
    }

    /// Adds one row, its value and then its delimiter, where a null value adds nothing.
    pub(crate) fn update(&mut self, args: &[Value]) -> Result<()> {
        let Some(value) = args.first() else {
            return Err(Error::internal("string_agg over 0 arguments"));
        };
        let Some(value) = self.bytes_of(value)? else { return Ok(()) };
        let delimiter = match args.get(1) {
            Some(delimiter) => self.bytes_of(delimiter)?.unwrap_or_default(),
            None => &[],
        };
        if !self.seen {
            self.head = delimiter.len();
            self.seen = true;
        }
        self.bytes.extend_from_slice(delimiter);
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    /// Puts the rows of `theirs` after the rows of this state. The delimiter in front of theirs
    /// stays, since it is no longer the first.
    pub(crate) fn combine(&mut self, theirs: &Self) {
        if !theirs.seen {
            return;
        }
        if !self.seen {
            self.head = theirs.head;
            self.seen = true;
        }
        self.bytes.extend_from_slice(&theirs.bytes);
    }

    /// The text or the bytes with the first delimiter left out, or null when no row had a value.
    pub(crate) fn finish(&self) -> Result<Value> {
        if !self.seen {
            return Ok(Value::Null);
        }
        let joined = self.bytes[self.head..].to_vec();
        if self.binary {
            return Ok(Value::Blob(joined));
        }
        String::from_utf8(joined)
            .map(Value::Varchar)
            .map_err(|_| Error::internal("string_agg joined text that is not UTF-8"))
    }

    /// The bytes of `value`, or `None` when it is null.
    fn bytes_of<'a>(&self, value: &'a Value) -> Result<Option<&'a [u8]>> {
        match (value, self.binary) {
            (Value::Null, _) => Ok(None),
            (Value::Varchar(text), false) => Ok(Some(text.as_bytes())),
            (Value::Blob(bytes), true) => Ok(Some(bytes)),
            _ => Err(Error::internal(format!("string_agg over {value:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        Value::Varchar(value.to_string())
    }

    fn joined(name: &str, rows: &[[Value; 2]]) -> Value {
        let mut state = Delimited::named(name).unwrap();
        for row in rows {
            state.update(row).unwrap();
        }
        state.finish().unwrap()
    }

    #[test]
    fn each_row_adds_its_own_delimiter_and_the_first_one_is_left_out() {
        let rows = [
            [text("a"), text("-")],
            [text("b"), Value::Null],
            [text("c"), text("+")],
            [Value::Null, text("*")],
            [text("e"), text("/")],
        ];
        assert_eq!(joined("string_agg_finalfn", &rows), text("ab+c/e"));
        assert_eq!(joined("string_agg_finalfn", &rows[3..4]), Value::Null);
        assert_eq!(joined("string_agg_finalfn", &[]), Value::Null);
    }

    #[test]
    fn bytea_joins_the_bytes() {
        let rows = [
            [Value::Blob(vec![0xff]), Value::Null],
            [Value::Blob(vec![0xaa]), Value::Null],
            [Value::Blob(vec![0x01]), Value::Blob(vec![0xee])],
        ];
        assert_eq!(
            joined("bytea_string_agg_finalfn", &rows),
            Value::Blob(vec![0xff, 0xaa, 0xee, 0x01])
        );
    }

    #[test]
    fn a_combine_keeps_the_delimiter_in_front_of_the_second_half() {
        let mut first = Delimited::named("string_agg_finalfn").unwrap();
        let mut second = first.clone();
        first.update(&[text("a"), text(",")]).unwrap();
        second.update(&[text("b"), text(";")]).unwrap();
        let mut empty = Delimited::named("string_agg_finalfn").unwrap();
        empty.combine(&first);
        empty.combine(&second);
        assert_eq!(empty.finish().unwrap(), text("a;b"));
    }
}
