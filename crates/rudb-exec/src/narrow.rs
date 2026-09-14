//! The group key when it is one or two integers, which on ClickBench is most of them.
//!
//! `table.rs` holds a general key: any number of columns of any type, each one kept in whatever
//! width suits it, and compared against the vector the row arrived in. That is the only thing that
//! works for a string key or a key of five columns, and it is what a group by falls back to. It is
//! also more machinery than `GROUP BY UserID` needs, because a `BIGINT` key is eight bytes and the
//! comparison it wants is one that a compiler can see is a comparison of two integers.
//!
//! So the shape of the key is decided once, when the aggregate is built, out of the types. One or
//! two columns whose values are signed integers of 64 bits or less become a [`Narrow`] key, which is
//! a run of words with nothing else in it, and everything else stays general. `GROUP BY UserID`,
//! `GROUP BY RegionID`, `GROUP BY EventDate` and `GROUP BY WatchID, ClientIP` are all the first kind.
//!
//! # The chunk is flattened once
//!
//! The point is not only that the stored key is narrow. It is that the probe never looks at a vector
//! at all. [`Narrow::begin`] walks each key column once with its type matched on once, writing one word
//! per row into a flat buffer and mixing the same word into the running hash on the way past. After
//! that the row loop is two array reads and a compare, with no validity bitmap, no body
//! discriminant and no `Value` anywhere in it.
//!
//! Doing the hash in the same pass is also what keeps the two agreeing. A narrow table and a general
//! one have to send the same key to the same bucket, or a group by that spills and comes back finds
//! the same number twice, and the cheapest way to be sure of that is for both to mix the identical
//! word. There is a test that checks it column by column.
//!
//! # Nulls
//!
//! A null groups with a null, so the word alone is not the key. Each row carries a small bitmap of
//! which of its columns have a value, and a null's word is zero, so a null and a real zero differ in
//! the bitmap and match in the word. Both are compared and the whole thing is still two loads from
//! the same cache line.
//!
//! # Why not one 128 bit word for the pair
//!
//! Because the single column case is the common one and it would pay eight bytes a group for a half
//! it never uses. A run of words with a stride is the same two loads from the same cache line for
//! the pair, and exactly eight bytes a group for the single, which is what the memory budget in the
//! roadmap is asking for.

use std::ops::Range;

use rudb_common::{Error, LogicalType, PhysicalType, Result, Value};
use rudb_vector::{Data, Validity, Vector};

use crate::key::{mix, spread};
use crate::table::NOTHING;

/// The most key columns that fit in this shape.
///
/// Two, because the bitmap of which columns have a value is a byte and because a third column is
/// another cache line as often as not. A key of three integers goes the general way, which is
/// correct and slower, and the day a benchmark says the third one is worth having this is the
/// constant that moves.
const MOST: usize = 2;

/// Whether a key of these types is one this module can hold.
///
/// The physical type and not the logical one, so a `DATE`, a `TIMESTAMP` and a `DECIMAL(10, 2)` all
/// qualify by being stored in an integer, and the same arm rebuilds all three on the way out.
pub(crate) fn fits(types: &[LogicalType]) -> bool {
    !types.is_empty() && types.len() <= MOST && types.iter().all(one_fits)
}

/// Whether one column's type is stored in a signed integer of 64 bits or less.
fn one_fits(ty: &LogicalType) -> bool {
    matches!(
        ty.physical(),
        PhysicalType::Int8 | PhysicalType::Int16 | PhysicalType::Int32 | PhysicalType::Int64
    )
}

/// One chunk's key columns, flattened to words so that the row loop never touches a vector.
///
/// Kept by the table between chunks rather than allocated per chunk, for the same reason the hashes
/// buffer is: a group by over a million rows is a thousand chunks and a thousand trips to the
/// allocator for a buffer of a known size is a thousand too many.
#[derive(Debug, Default)]
pub(crate) struct Chunk {
    /// `width` words per row, in key column order.
    words: Vec<u64>,
    /// One bit per key column per row, set where that column has a value in that row.
    present: Vec<u8>,
}

/// The stored keys, one row of words per group.
#[derive(Debug)]
pub(crate) struct Narrow {
    /// `width` words per group, in key column order.
    words: Vec<u64>,
    /// One bit per key column per group, the same bitmap [`Chunk`] carries.
    present: Vec<u8>,
    /// How many key columns there are, which is one or two and never changes.
    width: usize,
    /// The chunk in hand, flattened.
    chunk: Chunk,
}

impl Narrow {
    /// An empty key store over `width` columns.
    pub(crate) fn new(width: usize) -> Self {
        Self { words: Vec::new(), present: Vec::new(), width, chunk: Chunk::default() }
    }

    /// Flattens the chunk in hand and hashes it, in one pass per key column.
    ///
    /// # Errors
    ///
    /// [`rudb_common::ErrorCode::Internal`] if a column hands back a value that is not the integer
    /// its type says it is, which is a bug in whatever built the vector rather than anything a query
    /// can cause.
    pub(crate) fn begin(
        &mut self,
        keys: &[Vector],
        rows: usize,
        hashes: &mut Vec<u64>,
    ) -> Result<()> {
        let width = self.width;
        let chunk = &mut self.chunk;
        chunk.words.clear();
        chunk.words.resize(rows * width, 0);
        chunk.present.clear();
        chunk.present.resize(rows, 0);
        hashes.clear();
        hashes.resize(rows, 0);

        for (at, column) in keys.iter().enumerate().take(width) {
            let validity = column.validity();
            let bit = 1u8 << at;

            /// One pass over a run of values, one word each, mixed as it goes.
            macro_rules! run {
                ($values:expr, $word:expr) => {{
                    let values = $values.as_slice();
                    let word = $word;
                    for row in 0..rows {
                        let one = match values.get(row) {
                            Some(value) if validity.is_valid(row) => {
                                chunk.present[row] |= bit;
                                let one = word(*value);
                                chunk.words[row * width + at] = one;
                                one
                            }
                            _ => NOTHING,
                        };
                        hashes[row] = mix(hashes[row], one);
                    }
                    continue;
                }};
            }

            if let Some(data) = column.data() {
                match data {
                    Data::Int8(values) => run!(values, |x: i8| i64::from(x) as u64),
                    Data::Int16(values) => run!(values, |x: i16| i64::from(x) as u64),
                    Data::Int32(values) => run!(values, |x: i32| i64::from(x) as u64),
                    Data::Int64(values) => run!(values, |x: i64| x as u64),
                    _ => {}
                }
            }

            // row at a time: the forms with no run of integers to walk, which is a dictionary, a
            // constant, a sequence, a run length column and the two that have to be unpacked first.
            // `signed_at` reads the first four where they lie and answers nothing for the last two,
            // and nothing there means the row becomes a value, which is what the general path would
            // have done with every one of these anyway.
            for (row, state) in hashes.iter_mut().enumerate() {
                let found = if validity.is_valid(row) {
                    match column.signed_at(row) {
                        Some(value) => Some(value),
                        None => integer_of(&column.value_at(row), column.logical_type())?,
                    }
                } else {
                    None
                };
                let one = match found {
                    Some(found) => {
                        chunk.present[row] |= bit;
                        let one = found as i64 as u64;
                        chunk.words[row * width + at] = one;
                        one
                    }
                    None => NOTHING,
                };
                *state = mix(*state, one);
            }
        }

        for state in hashes.iter_mut() {
            *state = spread(*state);
        }
        Ok(())
    }

    /// Whether the group in `slot` has the key the chunk in hand holds at `row`.
    ///
    /// This is the line that runs once per input row per probe step, and there is nothing in it but
    /// two loads and a compare. The bitmap is compared as well as the word, because a null's word is
    /// a zero and a column holding real zeroes would otherwise put them in the null's group.
    pub(crate) fn holds(&self, slot: usize, row: usize) -> bool {
        if self.present[slot] != self.chunk.present[row] {
            return false;
        }
        let (group, key) = (slot * self.width, row * self.width);
        self.words[group..group + self.width] == self.chunk.words[key..key + self.width]
    }

    /// Adds the key the chunk in hand holds at `row` as a new group.
    pub(crate) fn push(&mut self, row: usize) {
        let key = row * self.width;
        for at in 0..self.width {
            self.words.push(self.chunk.words[key + at]);
        }
        self.present.push(self.chunk.present[row]);
    }

    /// What the stored groups have taken from the allocator, capacity rather than length.
    ///
    /// The flattened chunk is not counted, for the same reason the caller's hashes buffer is not.
    /// It is one chunk wide whatever the table holds, it is scratch that the next chunk overwrites,
    /// and charging it to the groups would make a table of four groups look like a table of two
    /// thousand.
    pub(crate) fn footprint(&self) -> usize {
        self.words.capacity() * size_of::<u64>() + self.present.capacity()
    }

    /// One key column of a range of groups, in slot order, as a vector.
    ///
    /// Back into the width the type is stored in, which is the width the word came from, so this
    /// round trips. The truncation is not a narrowing: a word was written from a value of this type
    /// and sign extended into 64 bits, so the bits being dropped are the sign it was given.
    ///
    /// # Errors
    ///
    /// [`rudb_common::ErrorCode::Internal`] if the type is not one [`fits`] accepts, which is a
    /// caller asking a narrow key for a column it never held.
    pub(crate) fn vector(
        &self,
        at: usize,
        ty: &LogicalType,
        range: Range<usize>,
    ) -> Result<Vector> {
        let (start, len) = (range.start, range.len());
        let word = |slot: usize| self.words[slot * self.width + at] as i64;
        let data = match ty.physical() {
            PhysicalType::Int8 => {
                Data::Int8(range.map(|slot| word(slot) as i8).collect::<Vec<_>>().into())
            }
            PhysicalType::Int16 => {
                Data::Int16(range.map(|slot| word(slot) as i16).collect::<Vec<_>>().into())
            }
            PhysicalType::Int32 => {
                Data::Int32(range.map(|slot| word(slot) as i32).collect::<Vec<_>>().into())
            }
            PhysicalType::Int64 => Data::Int64(range.map(word).collect::<Vec<_>>().into()),
            other => {
                return Err(Error::internal(format!(
                    "a narrow group key was asked for a {other:?} column, which it never held"
                )));
            }
        };
        let bit = 1u8 << at;
        let present = &self.present;
        let validity = Validity::from_iter(len, |index| present[start + index] & bit != 0);
        Ok(Vector::flat(ty.clone(), data)?.with_validity(validity))
    }
}

/// The integer a value of one of these types holds, or nothing if the row is null.
///
/// Only reached for the forms `signed_at` refuses, which are the packed and compressed ones, and
/// only for types [`fits`] accepted. A value that is none of these is a vector whose data does not
/// match its type, which is a bug somewhere below this and is reported rather than turned into a
/// group of its own.
///
/// Null is one of the answers because a dictionary keeps its nulls in the values it points at rather
/// than in the validity of the column itself, so a row can be valid, have no signed value and read
/// back as null. The general path treats that as a null and so does this one, which is what makes
/// the two agree on a dictionary column that carries a null inside.
fn integer_of(value: &Value, ty: &LogicalType) -> Result<Option<i128>> {
    match value {
        Value::Null => Ok(None),
        Value::TinyInt(x) => Ok(Some(i128::from(*x))),
        Value::SmallInt(x) => Ok(Some(i128::from(*x))),
        Value::Integer(x) | Value::Date(x) => Ok(Some(i128::from(*x))),
        Value::BigInt(x) | Value::Time(x) | Value::Timestamp(x) => Ok(Some(i128::from(*x))),
        Value::Decimal { unscaled, .. } => Ok(Some(*unscaled)),
        other => Err(Error::internal(format!(
            "a {ty} group key column holds {other:?}, which is not the integer the type calls for"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(ty: LogicalType, values: &[Value]) -> Vector {
        Vector::from_values(ty, values).expect("a flat vector of these values")
    }

    /// One column of keys, flattened, with the hash that came out of the same pass.
    fn one(column: &Vector) -> (Narrow, Vec<u64>) {
        let mut narrow = Narrow::new(1);
        let mut hashes = Vec::new();
        narrow
            .begin(std::slice::from_ref(column), column.len(), &mut hashes)
            .expect("an integer column flattens");
        (narrow, hashes)
    }

    /// The hash the general path gives the same column.
    fn general(column: &Vector) -> Vec<u64> {
        let mut hashes = Vec::new();
        crate::table::hash(std::slice::from_ref(column), column.len(), &mut hashes);
        hashes
    }

    /// The invariant the whole module rests on. A narrow table and a general one have to send the
    /// same key to the same bucket, because the same column arrives flat in one chunk and as a
    /// dictionary in the next, and because a combine will one day merge two of them.
    #[test]
    fn a_narrow_key_hashes_a_column_the_way_the_general_path_does() {
        let values = [Value::BigInt(0), Value::BigInt(-1), Value::Null, Value::BigInt(i64::MAX)];
        let plain = flat(LogicalType::BigInt, &values);
        assert_eq!(one(&plain).1, general(&plain), "flat");

        let distinct = flat(LogicalType::BigInt, &values);
        let dictionary = Vector::dictionary(vec![3, 2, 1, 0], distinct).expect("a dictionary");
        assert_eq!(one(&dictionary).1, general(&dictionary), "dictionary");

        let constant = Vector::constant(LogicalType::Integer, Value::Integer(-7), 3);
        assert_eq!(one(&constant).1, general(&constant), "constant");

        let sequence = Vector::sequence(10, 2, 4);
        assert_eq!(one(&sequence).1, general(&sequence), "sequence");

        let ends = flat(LogicalType::Integer, &[Value::Integer(4), Value::Null]);
        let runs = Vector::runs(vec![2, 5], ends).expect("two runs of an integer");
        assert_eq!(one(&runs).1, general(&runs), "runs");

        let small: Vec<Value> = (0..64).map(|row| Value::BigInt(row % 7)).collect();
        let packed = flat(LogicalType::BigInt, &small).bit_packed().expect("small values pack");
        assert!(packed.signed_at(0).is_none(), "a packed row is not an integer a read can reach");
        assert_eq!(one(&packed).1, general(&packed), "packed, through the value fallback");
    }

    /// Every width the shape takes, in and back out again, because the word is stored sign extended
    /// to 64 bits and the way out has to put the sign back where the type keeps it.
    #[test]
    fn every_width_a_narrow_key_takes_comes_back_as_what_went_in() {
        let cases = [
            (LogicalType::TinyInt, vec![Value::TinyInt(-128), Value::TinyInt(127)]),
            (LogicalType::SmallInt, vec![Value::SmallInt(i16::MIN), Value::SmallInt(1)]),
            (LogicalType::Integer, vec![Value::Integer(i32::MIN), Value::Integer(5)]),
            (LogicalType::BigInt, vec![Value::BigInt(i64::MIN), Value::BigInt(9)]),
            (LogicalType::Date, vec![Value::Date(-1), Value::Date(19_000)]),
            (LogicalType::Timestamp, vec![Value::Timestamp(-1), Value::Timestamp(1_700_000_000)]),
            (
                LogicalType::Decimal { width: 18, scale: 2 },
                vec![
                    Value::Decimal { unscaled: -1234, width: 18, scale: 2 },
                    Value::Decimal { unscaled: 99, width: 18, scale: 2 },
                ],
            ),
        ];
        for (ty, values) in cases {
            assert!(fits(std::slice::from_ref(&ty)), "{ty} should be a narrow key");
            let column = flat(ty.clone(), &values);
            let (mut narrow, _) = one(&column);
            for row in 0..values.len() {
                narrow.push(row);
            }
            let out = narrow.vector(0, &ty, 0..values.len()).expect("a key column comes back");
            // row at a time: two values of each of seven types, read back to check the sign
            for (row, want) in values.iter().enumerate() {
                assert_eq!(&out.value_at(row), want, "{ty} at row {row}");
            }
        }
    }

    /// A null's word is a zero, so the bitmap beside it is the only thing telling the two apart.
    #[test]
    fn a_null_key_is_not_the_zero_next_to_it_and_comes_back_null() {
        let values = [Value::BigInt(0), Value::Null];
        let column = flat(LogicalType::BigInt, &values);
        let (mut narrow, hashes) = one(&column);
        narrow.push(0);
        narrow.push(1);

        assert!(narrow.holds(0, 0), "a zero is itself");
        assert!(narrow.holds(1, 1), "a null is itself");
        assert!(!narrow.holds(0, 1), "a null is not the zero beside it");
        assert!(!narrow.holds(1, 0), "and the zero is not the null");
        assert_ne!(hashes[0], hashes[1], "or they would be two entries in one bucket");

        let out = narrow.vector(0, &LogicalType::BigInt, 0..2).expect("a key column comes back");
        assert_eq!(out.value_at(0), Value::BigInt(0));
        assert_eq!(out.value_at(1), Value::Null);
    }

    /// The order of the columns is part of the key, or `GROUP BY a, b` would put `(1, 2)` and
    /// `(2, 1)` in one group whenever the two columns held each other's values.
    #[test]
    fn a_key_of_two_columns_is_two_keys_and_their_order_is_part_of_it() {
        let ones = flat(LogicalType::Integer, &[Value::Integer(1), Value::Integer(2)]);
        let twos = flat(LogicalType::Integer, &[Value::Integer(2), Value::Integer(1)]);
        let mut narrow = Narrow::new(2);
        let mut hashes = Vec::new();
        narrow.begin(&[ones, twos], 2, &mut hashes).expect("two integer columns flatten");
        narrow.push(0);

        assert!(narrow.holds(0, 0));
        assert!(!narrow.holds(0, 1), "(1, 2) and (2, 1) are two groups");
        assert_ne!(hashes[0], hashes[1]);
    }

    #[test]
    fn the_keys_this_shape_takes_are_the_integers_of_64_bits_or_less() {
        assert!(fits(&[LogicalType::BigInt]));
        assert!(fits(&[LogicalType::Date, LogicalType::Integer]));
        assert!(fits(&[LogicalType::Decimal { width: 18, scale: 2 }]));
        assert!(!fits(&[LogicalType::Decimal { width: 19, scale: 2 }]), "that one is 128 bits");
        assert!(!fits(&[LogicalType::Varchar]));
        assert!(!fits(&[LogicalType::Double]));
        assert!(!fits(&[LogicalType::HugeInt]));
        assert!(!fits(&[]), "no key at all is the aggregate's own case and not this one");
        let three = [LogicalType::Integer, LogicalType::Integer, LogicalType::Integer];
        assert!(!fits(&three), "three columns is one more than this holds");
    }

    /// The memory half of the point. A thousand `BIGINT` groups are the thousand integers and the
    /// bit beside each one saying it is there, and nothing else at all.
    #[test]
    fn a_stored_narrow_key_is_the_key_and_little_else() {
        let values: Vec<Value> = (0..1000).map(Value::BigInt).collect();
        let column = flat(LogicalType::BigInt, &values);
        let (mut narrow, _) = one(&column);
        for row in 0..values.len() {
            narrow.push(row);
        }
        let bytes = narrow.footprint();
        assert!(bytes < values.len() * 12, "{bytes} bytes held a thousand eight-byte keys");
    }
}
