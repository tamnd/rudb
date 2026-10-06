//! The `DataRow` encoder: the rows of a result, from rudb vectors to the messages of the protocol.
//!
//! A `DataRow` is the byte `D`, the length of the message, the column count, and for each column
//! the length of the value and its bytes, with -1 for NULL. The result is in columns and the
//! message is in rows, so the encoder works in three passes as document 06 section 6.15 says.
//! The first pass finds the length of each value, one column at a time. The second pass adds the
//! lengths of each row and reserves the whole output once. The third pass writes the values, one
//! column at a time. So the type of a column decides the code once for each call and not once for
//! each value.
//!
//! A value with a fixed length in the binary format and a string need no work in the first pass
//! except the null check. Any other value is written in the first pass to a buffer of the column
//! with the output function of its type, and the third pass copies it.

use std::borrow::Cow;
use std::ops::Range;

use rudb_common::{LogicalType, SqlState, time_tz, uuid};
use rudb_vector::{Data, Form, Live, Vector};

use crate::array::{Array, ArrayDim, array_out, array_send};
use crate::datetime::{
    DateFormat, Interval, IntervalStyle, TimeZone, date_from_unix, date_out, interval_out,
    interval_send, time_out, timestamp_from_unix, timestamp_out, timestamptz_out, timetz_out,
};
use crate::error::TypeError;
use crate::float::{float4_out, float8_out};
use crate::generated::oids;
use crate::number::{int_out, u64_out};
use crate::numeric::{Numeric, decimal_out, decimal_send, numeric_in, numeric_out, numeric_send};
use crate::reg::{RegKind, reg_out_oid};
use crate::scalar::{ByteaOutput, bool_out, bytea_out, char_out, uuid_out};
use crate::types::{Oid, PgType, TypeInfo, format_type};
use crate::typmod::numeric_typmod;

/// The settings of the session that the text output reads.
#[derive(Clone, Copy)]
pub struct OutputSettings<'a> {
    /// `DateStyle`.
    pub date_format: DateFormat,
    /// `IntervalStyle`.
    pub interval_style: IntervalStyle,
    /// `extra_float_digits`.
    pub extra_float_digits: i32,
    /// `bytea_output`.
    pub bytea_output: ByteaOutput,
    /// `TimeZone`, for `timestamptz`.
    pub time_zone: &'a dyn TimeZone,
}

impl std::fmt::Debug for OutputSettings<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputSettings")
            .field("date_format", &self.date_format)
            .field("interval_style", &self.interval_style)
            .field("extra_float_digits", &self.extra_float_digits)
            .field("bytea_output", &self.bytea_output)
            .finish_non_exhaustive()
    }
}

/// What the encoder does with the values of one column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Char,
    Int2,
    Int4,
    Int8,
    Oid,
    Float4,
    Float8,
    Decimal(u32),
    /// The `numeric` of PostgreSQL, in the bytes of `rudb_common::numeric`.
    Numeric,
    Text,
    /// A `jsonb`, held as its text in the normal form. The text format is the text, and the binary
    /// format is the version byte 1 and then the text.
    Jsonb,
    /// A `char(n)`, which goes out padded with spaces to n characters in both formats. The value
    /// is kept with no trailing spaces.
    Bpchar(u32),
    Bytea,
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Interval,
    Uuid,
    /// A one-dimensional array. The plan of the column has the kind of the elements.
    Array,
    /// An `int2vector` or an `oidvector`. The plan of the column has the kind of the elements.
    /// The text has a space between the numbers and no braces, and the binary format is an array
    /// with the lower bound 0.
    Vector,
    /// An OID alias type such as `regtype`. The binary format is the binary format of `oid`.
    Reg(RegKind),
    /// A column of the type of an untyped `NULL`. Every value is NULL.
    Null,
    /// A rudb type with no PostgreSQL type of its own, sent as the text of each value. When
    /// `numeric` is true the column is sent as `numeric`, and the binary format is the binary
    /// format of that text.
    Display {
        numeric: bool,
    },
}

/// The PostgreSQL type that a column of a rudb type is sent as, with its typmod.
///
/// Each rudb type with a PostgreSQL type of the same values gets that type. The unsigned integer
/// types get the next signed type that holds all their values, and the integers of 64 bits or more
/// without a sign or of 128 bits get `numeric`. A list gets the array type of its element type,
/// with the typmod of the element as PostgreSQL does, when the element type has an array type and
/// is not itself a list. The other rudb types, such as the nested lists, the structs and the
/// enums, get `text` until their PostgreSQL types come.
pub fn pg_type(logical: &LogicalType) -> PgType {
    use LogicalType as L;
    let (oid, typmod) = match logical {
        L::List(element) | L::Array(element, _) => {
            let element = pg_type(element);
            match TypeInfo::get(element.oid).map(|info| info.array) {
                Some(array) if array != 0 => (array, element.typmod),
                _ => (oids::TEXT, -1),
            }
        }
        L::Boolean => (oids::BOOL, -1),
        L::TinyInt | L::SmallInt | L::UTinyInt => (oids::INT2, -1),
        L::Integer | L::USmallInt => (oids::INT4, -1),
        L::BigInt | L::UInteger => (oids::INT8, -1),
        L::UBigInt | L::HugeInt | L::UHugeInt => (oids::NUMERIC, -1),
        L::Float => (oids::FLOAT4, -1),
        L::Double => (oids::FLOAT8, -1),
        L::Decimal { width, scale } => {
            (oids::NUMERIC, numeric_typmod(i32::from(*width), i32::from(*scale)))
        }
        L::Numeric => (oids::NUMERIC, -1),
        L::Blob => (oids::BYTEA, -1),
        L::Uuid => (oids::UUID, -1),
        L::Date => (oids::DATE, -1),
        L::Time => (oids::TIME, -1),
        L::TimeTz => (oids::TIMETZ, -1),
        L::Timestamp => (oids::TIMESTAMP, -1),
        L::TimestampTz => (oids::TIMESTAMPTZ, -1),
        L::Interval => (oids::INTERVAL, -1),
        L::Json => (oids::JSON, -1),
        L::Jsonb => (oids::JSONB, -1),
        _ => (oids::TEXT, -1),
    };
    PgType { oid, typmod }
}

/// Whether the encoder can send the values of a rudb type as the PostgreSQL type `oid`, which is
/// what a type that a declaration or a cast wrote has to meet before `RowDescription` names it.
pub fn encodable(logical: &LogicalType, oid: Oid) -> bool {
    Kind::of(logical, oid).is_some()
}

impl Kind {
    /// The kind for a rudb type sent as a PostgreSQL type, or `None` when the encoder cannot send
    /// it. The binder casts a rudb type with no PostgreSQL type to the nearest one, so this takes
    /// only the pairs in the map of document 06 section 6.6.
    fn of(logical: &LogicalType, oid: Oid) -> Option<Kind> {
        use LogicalType as L;
        let int = |widths: &[&LogicalType]| widths.contains(&logical);
        if *logical == L::Null {
            return Some(Kind::Null);
        }
        let kind = match oid {
            oids::BOOL if *logical == L::Boolean => Kind::Bool,
            oids::CHAR if *logical == L::UTinyInt => Kind::Char,
            oids::INT2 if int(&[&L::TinyInt, &L::UTinyInt, &L::SmallInt]) => Kind::Int2,
            oids::INT4
                if int(&[&L::TinyInt, &L::UTinyInt, &L::SmallInt, &L::USmallInt, &L::Integer]) =>
            {
                Kind::Int4
            }
            oids::INT8
                if int(&[
                    &L::TinyInt,
                    &L::UTinyInt,
                    &L::SmallInt,
                    &L::USmallInt,
                    &L::Integer,
                    &L::UInteger,
                    &L::BigInt,
                ]) =>
            {
                Kind::Int8
            }
            oids::OID if *logical == L::UInteger => Kind::Oid,
            oid if *logical == L::UInteger
                && let Some(kind) = RegKind::from_oid(oid) =>
            {
                Kind::Reg(kind)
            }
            oids::FLOAT4 if *logical == L::Float => Kind::Float4,
            oids::FLOAT8 if *logical == L::Double => Kind::Float8,
            oids::NUMERIC => match *logical {
                L::Decimal { scale, .. } => Kind::Decimal(u32::from(scale)),
                L::Numeric => Kind::Numeric,
                L::UBigInt | L::HugeInt | L::UHugeInt => Kind::Display { numeric: true },
                _ => return None,
            },
            oids::TEXT | oids::VARCHAR | oids::BPCHAR | oids::NAME | oids::UNKNOWN | oids::VOID
                if *logical == L::Varchar =>
            {
                Kind::Text
            }
            oids::JSON if matches!(logical, L::Json | L::Varchar) => Kind::Text,
            oids::JSONB if *logical == L::Jsonb => Kind::Jsonb,
            oids::BYTEA if *logical == L::Blob => Kind::Bytea,
            oids::DATE if *logical == L::Date => Kind::Date,
            oids::TIME if *logical == L::Time => Kind::Time,
            oids::TIMETZ if *logical == L::TimeTz => Kind::TimeTz,
            oids::TIMESTAMP if *logical == L::Timestamp => Kind::Timestamp,
            oids::TIMESTAMPTZ if *logical == L::TimestampTz => Kind::TimestampTz,
            oids::INTERVAL if *logical == L::Interval => Kind::Interval,
            oids::UUID if *logical == L::Uuid => Kind::Uuid,
            oids::INT2VECTOR | oids::OIDVECTOR if element_of(logical, oid).is_some() => {
                Kind::Vector
            }
            oid if element_of(logical, oid).is_some() => Kind::Array,
            oids::TEXT if pg_type(logical).oid == oids::TEXT => Kind::Display { numeric: false },
            _ => return None,
        };
        Some(kind)
    }

    /// The plan of a column of this kind, with the path of its values in the first and the third
    /// pass.
    fn plan(self, binary: bool, element: Option<Element>) -> Plan {
        let path = match (self, binary) {
            (Kind::Text | Kind::Jsonb, false) | (Kind::Text | Kind::Bytea, true) => Path::Bytes,
            (kind, true) => kind.binary_width().map_or(Path::Staged, Path::Fixed),
            (_, false) => Path::Staged,
        };
        Plan { kind: self, binary, path, element }
    }

    /// The length of a value in the binary format when it is the same for every value.
    fn binary_width(self) -> Option<i32> {
        Some(match self {
            Kind::Bool | Kind::Char => 1,
            Kind::Int2 => 2,
            Kind::Int4 | Kind::Oid | Kind::Reg(_) | Kind::Float4 | Kind::Date => 4,
            Kind::Int8 | Kind::Float8 | Kind::Time | Kind::Timestamp | Kind::TimestampTz => 8,
            Kind::TimeTz => 12,
            Kind::Interval | Kind::Uuid => 16,
            Kind::Decimal(_)
            | Kind::Numeric
            | Kind::Text
            | Kind::Jsonb
            | Kind::Bpchar(_)
            | Kind::Bytea
            | Kind::Array
            | Kind::Vector
            | Kind::Null
            | Kind::Display { .. } => {
                return None;
            }
        })
    }
}

/// How the first pass finds the lengths of a column, and where the third pass takes the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    /// The binary format of a type with one length. The third pass converts each value.
    Fixed(i32),
    /// A string in either format, or `bytea` in the binary format. The bytes are the value.
    Bytes,
    /// Any other value. The first pass writes it to the buffer of the column.
    Staged,
}

#[derive(Debug, Clone, Copy)]
struct Plan {
    kind: Kind,
    binary: bool,
    path: Path,
    /// The elements of an array column.
    element: Option<Element>,
}

/// The elements of an array column: their kind and path, their type and the delimiter of the
/// text format.
#[derive(Debug, Clone, Copy)]
struct Element {
    kind: Kind,
    path: Path,
    oid: Oid,
    delim: u8,
}

impl Element {
    fn plan(self, binary: bool) -> Plan {
        Plan { kind: self.kind, binary, path: self.path, element: None }
    }
}

/// The element type and the element kind of a list sent as the array type `oid`. An element
/// that is itself an array is not one of these, as PostgreSQL has no arrays of arrays.
fn element_of(logical: &LogicalType, oid: Oid) -> Option<(Oid, Kind)> {
    let (LogicalType::List(element) | LogicalType::Array(element, _)) = logical else {
        return None;
    };
    let elem = match oid {
        oids::INT2VECTOR => oids::INT2,
        oids::OIDVECTOR => oids::OID,
        _ => TypeInfo::get(oid).filter(|info| info.is_array())?.elem,
    };
    let kind = Kind::of(element, elem)?;
    (!matches!(kind, Kind::Array | Kind::Vector)).then_some((elem, kind))
}

/// The `DataRow` encoder of one result. It keeps its buffers between calls, so the rows of a
/// result after the first vector need no allocation.
#[derive(Debug, Default)]
pub struct RowEncoder {
    plans: Vec<Plan>,
    /// The length of each value, column by column, with -1 for NULL.
    lens: Vec<i32>,
    /// The place in the output where the next value of each row goes.
    cursor: Vec<usize>,
    /// The values of the staged columns, one after the other.
    staged: Vec<Vec<u8>>,
}

impl RowEncoder {
    /// An encoder for columns of these rudb types, sent as these PostgreSQL types, each in the
    /// binary format when its flag is true. A pair that the encoder cannot send is an error, which
    /// is a bug in the binder.
    pub fn new(columns: &[(LogicalType, Oid, bool)]) -> Result<RowEncoder, TypeError> {
        RowEncoder::with_typmods(columns, &[])
    }

    /// [`RowEncoder::new`] with the typmod of each column. A `char(n)` column pads its values to
    /// n characters. A column with no typmod in `typmods` has the typmod -1.
    pub fn with_typmods(
        columns: &[(LogicalType, Oid, bool)],
        typmods: &[i32],
    ) -> Result<RowEncoder, TypeError> {
        let plans = columns
            .iter()
            .enumerate()
            .map(|(c, (logical, oid, binary))| {
                let typmod = typmods.get(c).copied().unwrap_or(-1);
                if *oid == oids::BPCHAR && *logical == LogicalType::Varchar && typmod >= 4 {
                    return Ok(Kind::Bpchar((typmod - 4) as u32).plan(*binary, None));
                }
                let kind = Kind::of(logical, *oid).ok_or_else(|| {
                    TypeError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cannot send a value of type {logical} as type {}",
                            format_type(*oid)
                        ),
                    )
                })?;
                let element = element_of(logical, *oid).map(|(oid, kind)| {
                    let path = kind.plan(*binary, None).path;
                    let delim = TypeInfo::get(oid).map_or(b',', |info| info.delim);
                    Element { kind, path, oid, delim }
                });
                Ok(kind.plan(*binary, element))
            })
            .collect::<Result<Vec<_>, TypeError>>()?;
        let staged = vec![Vec::new(); plans.len()];
        Ok(RowEncoder { plans, lens: Vec::new(), cursor: Vec::new(), staged })
    }

    /// The number of columns.
    pub fn width(&self) -> usize {
        self.plans.len()
    }

    /// Appends one `DataRow` message to `out` for each row in `rows` of the vectors, which are
    /// the columns in the order of [`RowEncoder::new`]. A value that PostgreSQL cannot hold, such
    /// as a date after the year 5874897, is an error, and then `out` is as it was.
    pub fn encode(
        &mut self,
        vectors: &[Vector],
        rows: Range<usize>,
        settings: &OutputSettings<'_>,
        out: &mut Vec<u8>,
    ) -> Result<(), TypeError> {
        if vectors.len() != self.plans.len() {
            return Err(internal(format!(
                "the encoder has {} columns and the result has {}",
                self.plans.len(),
                vectors.len()
            )));
        }
        let n = rows.len();
        if n == 0 {
            return Ok(());
        }
        let flat = vectors
            .iter()
            .map(|vector| {
                if vector.len() < rows.end {
                    return Err(internal(format!(
                        "the rows {rows:?} are not in a vector of {}",
                        vector.len()
                    )));
                }
                match vector.form() {
                    // A list has no other form, and its elements are flattened in the first pass.
                    Form::Flat | Form::List => Ok(Cow::Borrowed(vector)),
                    // flatten: the encoder reads each value by its place in a flat column.
                    _ => vector.flatten().map(Cow::Owned).map_err(|e| internal(e.to_string())),
                }
            })
            .collect::<Result<Vec<_>, TypeError>>()?;

        // The first pass: the length of each value.
        self.lens.clear();
        self.lens.resize(n * self.plans.len(), -1);
        for (c, (plan, vector)) in self.plans.iter().zip(&flat).enumerate() {
            let lens = &mut self.lens[c * n..(c + 1) * n];
            let staged = &mut self.staged[c];
            staged.clear();
            lengths(*plan, vector, rows.clone(), settings, lens, staged)?;
        }

        // The second pass: the start of each row, and one reserve for all of them.
        let columns = self.plans.len();
        let header = 1 + 4 + 2 + 4 * columns;
        self.cursor.clear();
        self.cursor.resize(n, header);
        for c in 0..columns {
            for (size, &len) in self.cursor.iter_mut().zip(&self.lens[c * n..(c + 1) * n]) {
                *size += len.max(0) as usize;
            }
        }
        let mut total = 0usize;
        for &size in &self.cursor {
            // The length of a message does not count the type byte.
            if size - 1 > i32::MAX as usize {
                return Err(TypeError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    "a row is too large to send".to_owned(),
                ));
            }
            total += size;
        }
        let base = out.len();
        out.resize(base + total, 0);
        let column_count = (columns as u16).to_be_bytes();
        let mut start = base;
        for at in &mut self.cursor {
            let size = *at;
            let message = &mut out[start..start + 7];
            message[0] = b'D';
            message[1..5].copy_from_slice(&((size - 1) as i32).to_be_bytes());
            message[5..7].copy_from_slice(&column_count);
            *at = start + 7;
            start += size;
        }

        // The third pass: the values.
        for (c, (plan, vector)) in self.plans.iter().zip(&flat).enumerate() {
            let lens = &self.lens[c * n..(c + 1) * n];
            let result =
                put(*plan, vector, rows.start, lens, &self.staged[c], &mut self.cursor, out);
            if let Err(error) = result {
                out.truncate(base);
                return Err(error);
            }
        }
        Ok(())
    }
}

fn internal(message: String) -> TypeError {
    TypeError::new(SqlState::INTERNAL_ERROR, message)
}

fn wrong_data(plan: Plan) -> TypeError {
    internal(format!("the vector of a column of {:?} has values of another type", plan.kind))
}

/// The values of a flat vector of integers as `i64`, with one copy of `$body` for each width.
macro_rules! with_ints {
    ($data:expr, $plan:expr, |$values:ident| $body:expr) => {
        match $data {
            Data::Int8(b) => {
                let $values = &b[..];
                $body
            }
            Data::UInt8(b) => {
                let $values = &b[..];
                $body
            }
            Data::Int16(b) => {
                let $values = &b[..];
                $body
            }
            Data::UInt16(b) => {
                let $values = &b[..];
                $body
            }
            Data::Int32(b) => {
                let $values = &b[..];
                $body
            }
            Data::UInt32(b) => {
                let $values = &b[..];
                $body
            }
            Data::Int64(b) => {
                let $values = &b[..];
                $body
            }
            Data::Int128(b) => {
                let $values = &b[..];
                $body
            }
            _ => return Err(wrong_data($plan)),
        }
    };
}

/// The first pass for one column: the length of each value into `lens`, which starts as all -1,
/// and the bytes of each staged value into `staged`.
fn lengths(
    plan: Plan,
    vector: &Vector,
    rows: Range<usize>,
    settings: &OutputSettings<'_>,
    lens: &mut [i32],
    staged: &mut Vec<u8>,
) -> Result<(), TypeError> {
    let live = vector.validity().live();
    match plan.kind {
        Kind::Null => return Ok(()),
        Kind::Display { numeric } => {
            let start = rows.start;
            // row at a time: the types with no encoder of their own are written from their display text.
            for i in (0..lens.len()).filter(|&i| live.at(start + i)) {
                let text = vector.value_at(start + i).to_string();
                try_stage(lens, staged, i, |out| {
                    if numeric && plan.binary {
                        numeric_send(&numeric_in(&text, -1)?, out);
                    } else {
                        out.extend_from_slice(text.as_bytes());
                    }
                    Ok(())
                })?;
            }
            return Ok(());
        }
        Kind::Array | Kind::Vector => {
            return array_lengths(plan, vector, rows, settings, lens, staged);
        }
        _ => {}
    }
    let data = vector.data().ok_or_else(|| wrong_data(plan))?;
    if matches!(data, Data::Empty) {
        return Ok(());
    }
    let start = rows.start;
    match plan.path {
        Path::Fixed(width) => {
            for (i, len) in lens.iter_mut().enumerate() {
                if live.at(start + i) {
                    *len = width;
                }
            }
            // The epoch shift can fail, so the binary dates and timestamps are checked here.
            match (plan.kind, data) {
                (Kind::Date, Data::Int32(values)) => {
                    for (i, len) in lens.iter().enumerate() {
                        if *len >= 0 {
                            date_from_unix(values[start + i])?;
                        }
                    }
                }
                (Kind::Timestamp | Kind::TimestampTz, Data::Int64(values)) => {
                    for (i, len) in lens.iter().enumerate() {
                        if *len >= 0 {
                            timestamp_from_unix(values[start + i])?;
                        }
                    }
                }
                _ => {}
            }
        }
        Path::Bytes => {
            let Data::Varlen(strings) = data else { return Err(wrong_data(plan)) };
            for (i, len) in lens.iter_mut().enumerate() {
                if live.at(start + i) {
                    let bytes = strings.bytes(start + i).ok_or_else(|| wrong_data(plan))?;
                    *len = value_len(bytes.len())?;
                }
            }
        }
        Path::Staged => stage_column(plan, data, start, live, settings, lens, staged)?,
    }
    Ok(())
}

fn value_len(len: usize) -> Result<i32, TypeError> {
    i32::try_from(len).map_err(|_| {
        TypeError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "a value is too large to send".to_owned())
    })
}

/// Writes each value of a staged column that is not NULL with the output function of its type.
// `with_ints!` casts each integer width, and one of the widths is the type of the cast.
#[allow(clippy::unnecessary_cast)]
fn stage_column(
    plan: Plan,
    data: &Data,
    start: usize,
    live: Live<'_>,
    settings: &OutputSettings<'_>,
    lens: &mut [i32],
    staged: &mut Vec<u8>,
) -> Result<(), TypeError> {
    let rows = (0..lens.len()).filter(|&i| live.at(start + i));
    match (plan.kind, plan.binary) {
        (Kind::Bool, _) => {
            let Data::Bool(values) = data else { return Err(wrong_data(plan)) };
            for i in rows {
                stage(lens, staged, i, |out| bool_out(values[start + i], out))?;
            }
        }
        (Kind::Char, _) => {
            let Data::UInt8(values) = data else { return Err(wrong_data(plan)) };
            for i in rows {
                stage(lens, staged, i, |out| char_out(values[start + i], out))?;
            }
        }
        (Kind::Int2 | Kind::Int4 | Kind::Int8, _) => with_ints!(data, plan, |values| {
            for i in rows {
                stage(lens, staged, i, |out| int_out(values[start + i] as i64, out))?;
            }
        }),
        (Kind::Oid, _) => {
            let Data::UInt32(values) = data else { return Err(wrong_data(plan)) };
            for i in rows {
                stage(lens, staged, i, |out| u64_out(u64::from(values[start + i]), out))?;
            }
        }
        (Kind::Reg(kind), _) => {
            let Data::UInt32(values) = data else { return Err(wrong_data(plan)) };
            for i in rows {
                stage(lens, staged, i, |out| reg_out(kind, values[start + i], out))?;
            }
        }
        (Kind::Float4, _) => {
            let Data::Float32(values) = data else { return Err(wrong_data(plan)) };
            let digits = settings.extra_float_digits;
            for i in rows {
                stage(lens, staged, i, |out| float4_out(values[start + i], digits, out))?;
            }
        }
        (Kind::Float8, _) => {
            let Data::Float64(values) = data else { return Err(wrong_data(plan)) };
            let digits = settings.extra_float_digits;
            for i in rows {
                stage(lens, staged, i, |out| float8_out(values[start + i], digits, out))?;
            }
        }
        (Kind::Decimal(scale), binary) => with_ints!(data, plan, |values| {
            for i in rows {
                let value = values[start + i] as i128;
                stage(lens, staged, i, |out| match binary {
                    true => decimal_send(value, scale, out),
                    false => decimal_out(value, scale, out),
                })?;
            }
        }),
        (Kind::Numeric, binary) => {
            let Data::Varlen(strings) = data else { return Err(wrong_data(plan)) };
            for i in rows {
                let bytes = strings.bytes(start + i).ok_or_else(|| wrong_data(plan))?;
                let value = Numeric::from_bytes(bytes);
                stage(lens, staged, i, |out| match binary {
                    true => numeric_send(&value, out),
                    false => numeric_out(&value, out),
                })?;
            }
        }
        (Kind::Jsonb, _) => {
            let Data::Varlen(strings) = data else { return Err(wrong_data(plan)) };
            for i in rows {
                let bytes = strings.bytes(start + i).ok_or_else(|| wrong_data(plan))?;
                stage(lens, staged, i, |out| {
                    out.push(1);
                    out.extend_from_slice(bytes);
                })?;
            }
        }
        (Kind::Bpchar(n), _) => {
            let Data::Varlen(strings) = data else { return Err(wrong_data(plan)) };
            for i in rows {
                let bytes = strings.bytes(start + i).ok_or_else(|| wrong_data(plan))?;
                stage(lens, staged, i, |out| bpchar_out(bytes, n as usize, out))?;
            }
        }
        (Kind::Bytea, _) => {
            let Data::Varlen(strings) = data else { return Err(wrong_data(plan)) };
            let style = settings.bytea_output;
            for i in rows {
                let bytes = strings.bytes(start + i).ok_or_else(|| wrong_data(plan))?;
                stage(lens, staged, i, |out| bytea_out(bytes, style, out))?;
            }
        }
        (Kind::Date, _) => {
            let Data::Int32(values) = data else { return Err(wrong_data(plan)) };
            let format = settings.date_format;
            for i in rows {
                let date = date_from_unix(values[start + i])?;
                stage(lens, staged, i, |out| date_out(date, format, out))?;
            }
        }
        (Kind::Time, _) => {
            let Data::Int64(values) = data else { return Err(wrong_data(plan)) };
            for i in rows {
                stage(lens, staged, i, |out| time_out(values[start + i], out))?;
            }
        }
        (Kind::TimeTz, _) => {
            let Data::Int64(values) = data else { return Err(wrong_data(plan)) };
            for i in rows {
                let key = values[start + i];
                stage(lens, staged, i, |out| {
                    timetz_out(time_tz::micros(key), -time_tz::offset(key), out)
                })?;
            }
        }
        (Kind::Timestamp, _) => {
            let Data::Int64(values) = data else { return Err(wrong_data(plan)) };
            let format = settings.date_format;
            for i in rows {
                let ts = timestamp_from_unix(values[start + i])?;
                try_stage(lens, staged, i, |out| timestamp_out(ts, format, out))?;
            }
        }
        (Kind::TimestampTz, _) => {
            let Data::Int64(values) = data else { return Err(wrong_data(plan)) };
            let format = settings.date_format;
            let zone = settings.time_zone;
            for i in rows {
                let ts = timestamp_from_unix(values[start + i])?;
                try_stage(lens, staged, i, |out| timestamptz_out(ts, format, zone, out))?;
            }
        }
        (Kind::Interval, _) => {
            let Data::Interval(values) = data else { return Err(wrong_data(plan)) };
            let style = settings.interval_style;
            for i in rows {
                let iv = interval(values[start + i]);
                stage(lens, staged, i, |out| interval_out(&iv, style, out))?;
            }
        }
        (Kind::Uuid, _) => {
            let Data::Int128(values) = data else { return Err(wrong_data(plan)) };
            for i in rows {
                let bytes = uuid::to_bytes(values[start + i]);
                stage(lens, staged, i, |out| uuid_out(&bytes, out))?;
            }
        }
        (Kind::Text | Kind::Array | Kind::Vector | Kind::Null | Kind::Display { .. }, _) => {
            return Err(wrong_data(plan));
        }
    }
    Ok(())
}

/// The first pass for an array column. The elements of all the rows go through the first and the
/// third pass of their own kind at once, which writes each element after its length as the binary
/// format of an array has it. Then each row is the text or the binary format of its part of them.
fn array_lengths(
    plan: Plan,
    vector: &Vector,
    rows: Range<usize>,
    settings: &OutputSettings<'_>,
    lens: &mut [i32],
    staged: &mut Vec<u8>,
) -> Result<(), TypeError> {
    let element = plan.element.ok_or_else(|| wrong_data(plan))?;
    let (entries, child) = vector.list_parts().ok_or_else(|| wrong_data(plan))?;
    let live = vector.validity().live();
    let start = rows.start;
    let parts = |i: usize| {
        let (at, len) = entries[start + i];
        at as usize..at as usize + len as usize
    };
    let (low, high) = (0..lens.len())
        .filter(|&i| live.at(start + i))
        .map(parts)
        .fold((usize::MAX, 0), |(low, high), part| (low.min(part.start), high.max(part.end)));
    let low = low.min(high);
    let child = match child.form() {
        Form::Flat | Form::List => Cow::Borrowed(child),
        // flatten: the elements are read by their place in a flat column.
        _ => child.flatten().map(Cow::Owned).map_err(|e| internal(e.to_string()))?,
    };
    if child.len() < high {
        return Err(wrong_data(plan));
    }
    // The elements, each after its length.
    let inner = element.plan(plan.binary);
    let n = high - low;
    let mut element_lens = vec![-1; n];
    let mut element_staged = Vec::new();
    lengths(inner, &child, low..high, settings, &mut element_lens, &mut element_staged)?;
    let mut cursor = Vec::with_capacity(n);
    let mut size = 0;
    for &len in &element_lens {
        cursor.push(size);
        size += 4 + len.max(0) as usize;
    }
    let mut elements = vec![0; size];
    let mut at = cursor.clone();
    put(inner, &child, low, &element_lens, &element_staged, &mut at, &mut elements)?;
    let value = |j: usize| {
        let len = element_lens[j];
        (len >= 0).then(|| &elements[cursor[j] + 4..cursor[j] + 4 + len as usize])
    };
    for i in (0..lens.len()).filter(|&i| live.at(start + i)) {
        let mut array = Array::one(parts(i).map(|j| value(j - low)).collect());
        if plan.kind == Kind::Vector {
            vector_value(plan, &mut array)?;
            if !plan.binary {
                stage(lens, staged, i, |out| {
                    for (at, value) in array.values.iter().flatten().enumerate() {
                        if at > 0 {
                            out.push(b' ');
                        }
                        out.extend_from_slice(value);
                    }
                })?;
                continue;
            }
        }
        stage(lens, staged, i, |out| match plan.binary {
            true => array_send(&array, element.oid, out, |bytes, out| out.extend_from_slice(bytes)),
            false => {
                array_out(&array, element.delim, out, |bytes, out| out.extend_from_slice(bytes));
            }
        })?;
    }
    Ok(())
}

/// An `int2vector` or an `oidvector` has one dimension with the lower bound 0, also when it is
/// empty, and it has no null.
fn vector_value(plan: Plan, array: &mut Array<&[u8]>) -> Result<(), TypeError> {
    if array.values.iter().any(Option::is_none) {
        return Err(TypeError::new(
            SqlState::NULL_VALUE_NOT_ALLOWED,
            "array must not contain nulls".to_owned(),
        ));
    }
    let len = i32::try_from(array.values.len()).map_err(|_| wrong_data(plan))?;
    array.dims = vec![ArrayDim { len, lower: 0 }];
    Ok(())
}

/// The text output of an OID alias type. A `regtype` of a built-in type is its name, as
/// `format_type` gives it. Every other value is the number until the catalog of PG3 has the names.
fn reg_out(kind: RegKind, oid: Oid, out: &mut Vec<u8>) {
    if kind == RegKind::Type && oid != 0 && TypeInfo::get(oid).is_some() {
        out.extend_from_slice(format_type(oid).as_bytes());
    } else {
        reg_out_oid(kind, oid, out);
    }
}

/// Writes one staged value and keeps its length.
/// A `char(n)` value padded with spaces to n characters. A longer value goes out as it is.
fn bpchar_out(bytes: &[u8], n: usize, out: &mut Vec<u8>) {
    out.extend_from_slice(bytes);
    // Most values are ASCII, and then the byte length is the character count.
    let chars = match bytes.is_ascii() {
        true => bytes.len(),
        false => bytes.iter().filter(|&&b| (b as i8) >= -0x40).count(),
    };
    out.resize(out.len() + n.saturating_sub(chars), b' ');
}

fn stage(
    lens: &mut [i32],
    staged: &mut Vec<u8>,
    i: usize,
    write: impl FnOnce(&mut Vec<u8>),
) -> Result<(), TypeError> {
    let before = staged.len();
    write(staged);
    lens[i] = value_len(staged.len() - before)?;
    Ok(())
}

/// [`stage`] for an output function that can fail.
fn try_stage(
    lens: &mut [i32],
    staged: &mut Vec<u8>,
    i: usize,
    write: impl FnOnce(&mut Vec<u8>) -> Result<(), TypeError>,
) -> Result<(), TypeError> {
    let before = staged.len();
    write(staged)?;
    lens[i] = value_len(staged.len() - before)?;
    Ok(())
}

fn interval((months, days, micros): (i32, i32, i64)) -> Interval {
    Interval { time: micros, day: days, month: months }
}

/// The third pass for one column: the length and the bytes of each value, at the cursor of its
/// row.
fn put(
    plan: Plan,
    vector: &Vector,
    start: usize,
    lens: &[i32],
    staged: &[u8],
    cursor: &mut [usize],
    out: &mut [u8],
) -> Result<(), TypeError> {
    match plan.path {
        Path::Staged => {
            let mut from = 0;
            each(lens, cursor, out, |_, value| {
                value.copy_from_slice(&staged[from..from + value.len()]);
                from += value.len();
            });
        }
        Path::Bytes => {
            let Some(Data::Varlen(strings)) = vector.data() else { return Err(wrong_data(plan)) };
            each(lens, cursor, out, |i, value| {
                // The first pass found every value, so this cannot fail.
                if let Some(bytes) = strings.bytes(start + i) {
                    value.copy_from_slice(bytes);
                }
            });
        }
        Path::Fixed(_) => {
            let data = vector.data().ok_or_else(|| wrong_data(plan))?;
            fixed(plan, data, start, lens, cursor, out)?;
        }
    }
    Ok(())
}

/// Writes the length of each value at the cursor of its row and gives `write` the place of the
/// bytes of each value that is not NULL and not empty.
fn each(
    lens: &[i32],
    cursor: &mut [usize],
    out: &mut [u8],
    mut write: impl FnMut(usize, &mut [u8]),
) {
    for (i, (&len, at)) in lens.iter().zip(cursor.iter_mut()).enumerate() {
        out[*at..*at + 4].copy_from_slice(&len.to_be_bytes());
        *at += 4;
        if len > 0 {
            let end = *at + len as usize;
            write(i, &mut out[*at..end]);
            *at = end;
        }
    }
}

/// The binary format of a type with one length.
// `with_ints!` casts each integer width, and one of the widths is the type of the cast.
#[allow(clippy::unnecessary_cast)]
fn fixed(
    plan: Plan,
    data: &Data,
    start: usize,
    lens: &[i32],
    cursor: &mut [usize],
    out: &mut [u8],
) -> Result<(), TypeError> {
    match plan.kind {
        Kind::Bool => {
            let Data::Bool(values) = data else { return Err(wrong_data(plan)) };
            each(lens, cursor, out, |i, value| value[0] = u8::from(values[start + i]));
        }
        Kind::Char => {
            let Data::UInt8(values) = data else { return Err(wrong_data(plan)) };
            each(lens, cursor, out, |i, value| value[0] = values[start + i]);
        }
        Kind::Int2 => with_ints!(data, plan, |values| {
            each(lens, cursor, out, |i, value| {
                value.copy_from_slice(&(values[start + i] as i16).to_be_bytes());
            })
        }),
        Kind::Int4 => with_ints!(data, plan, |values| {
            each(lens, cursor, out, |i, value| {
                value.copy_from_slice(&(values[start + i] as i32).to_be_bytes());
            })
        }),
        Kind::Int8 => with_ints!(data, plan, |values| {
            each(lens, cursor, out, |i, value| {
                value.copy_from_slice(&(values[start + i] as i64).to_be_bytes());
            })
        }),
        Kind::Oid | Kind::Reg(_) => {
            let Data::UInt32(values) = data else { return Err(wrong_data(plan)) };
            each(lens, cursor, out, |i, value| {
                value.copy_from_slice(&values[start + i].to_be_bytes())
            });
        }
        Kind::Float4 => {
            let Data::Float32(values) = data else { return Err(wrong_data(plan)) };
            each(lens, cursor, out, |i, value| {
                value.copy_from_slice(&values[start + i].to_bits().to_be_bytes());
            });
        }
        Kind::Float8 => {
            let Data::Float64(values) = data else { return Err(wrong_data(plan)) };
            each(lens, cursor, out, |i, value| {
                value.copy_from_slice(&values[start + i].to_bits().to_be_bytes());
            });
        }
        Kind::Date => {
            let Data::Int32(values) = data else { return Err(wrong_data(plan)) };
            // The first pass checked the range.
            each(lens, cursor, out, |i, value| {
                let date = date_from_unix(values[start + i]).unwrap_or_default();
                value.copy_from_slice(&date.to_be_bytes());
            });
        }
        Kind::Time => {
            let Data::Int64(values) = data else { return Err(wrong_data(plan)) };
            each(lens, cursor, out, |i, value| {
                value.copy_from_slice(&values[start + i].to_be_bytes())
            });
        }
        Kind::TimeTz => {
            let Data::Int64(values) = data else { return Err(wrong_data(plan)) };
            each(lens, cursor, out, |i, value| {
                let key = values[start + i];
                value[..8].copy_from_slice(&time_tz::micros(key).to_be_bytes());
                value[8..].copy_from_slice(&(-time_tz::offset(key)).to_be_bytes());
            });
        }
        Kind::Timestamp | Kind::TimestampTz => {
            let Data::Int64(values) = data else { return Err(wrong_data(plan)) };
            // The first pass checked the range.
            each(lens, cursor, out, |i, value| {
                let ts = timestamp_from_unix(values[start + i]).unwrap_or_default();
                value.copy_from_slice(&ts.to_be_bytes());
            });
        }
        Kind::Interval => {
            let Data::Interval(values) = data else { return Err(wrong_data(plan)) };
            let mut buf = Vec::with_capacity(16);
            each(lens, cursor, out, |i, value| {
                buf.clear();
                interval_send(&interval(values[start + i]), &mut buf);
                value.copy_from_slice(&buf);
            });
        }
        Kind::Uuid => {
            let Data::Int128(values) = data else { return Err(wrong_data(plan)) };
            each(lens, cursor, out, |i, value| {
                value.copy_from_slice(&uuid::to_bytes(values[start + i]))
            });
        }
        Kind::Decimal(_)
        | Kind::Numeric
        | Kind::Text
        | Kind::Jsonb
        | Kind::Bpchar(_)
        | Kind::Bytea
        | Kind::Array
        | Kind::Vector
        | Kind::Null
        | Kind::Display { .. } => {
            return Err(wrong_data(plan));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rudb_common::Value;

    use super::*;
    use crate::datetime::{DateStyle, FixedZone};

    fn settings(zone: &FixedZone) -> OutputSettings<'_> {
        OutputSettings {
            date_format: DateFormat {
                style: DateStyle::Postgres,
                order: DateFormat::ISO_MDY.order,
            },
            interval_style: IntervalStyle::Postgres,
            extra_float_digits: 1,
            bytea_output: ByteaOutput::Escape,
            time_zone: zone,
        }
    }

    /// One value as the output functions write it, row by row. The encoder must give the same
    /// bytes.
    fn reference(
        value: &Value,
        oid: Oid,
        binary: bool,
        settings: &OutputSettings<'_>,
    ) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let digits = settings.extra_float_digits;
        let int = |v: i64, out: &mut Vec<u8>| match (binary, oid) {
            (false, _) => int_out(v, out),
            (true, oids::INT2) => out.extend_from_slice(&(v as i16).to_be_bytes()),
            (true, oids::INT4) => out.extend_from_slice(&(v as i32).to_be_bytes()),
            (true, _) => out.extend_from_slice(&v.to_be_bytes()),
        };
        match value {
            Value::Null => return None,
            Value::Boolean(v) if binary => out.push(u8::from(*v)),
            Value::Boolean(v) => bool_out(*v, &mut out),
            Value::UTinyInt(v) if oid == oids::CHAR => match binary {
                true => out.push(*v),
                false => char_out(*v, &mut out),
            },
            Value::TinyInt(v) => int(i64::from(*v), &mut out),
            Value::UTinyInt(v) => int(i64::from(*v), &mut out),
            Value::SmallInt(v) => int(i64::from(*v), &mut out),
            Value::USmallInt(v) => int(i64::from(*v), &mut out),
            Value::Integer(v) => int(i64::from(*v), &mut out),
            Value::BigInt(v) => int(*v, &mut out),
            Value::UInteger(v) if oid == oids::INT8 => int(i64::from(*v), &mut out),
            Value::UInteger(v) if binary => out.extend_from_slice(&v.to_be_bytes()),
            Value::UInteger(v) => u64_out(u64::from(*v), &mut out),
            Value::Float(v) if binary => out.extend_from_slice(&v.to_bits().to_be_bytes()),
            Value::Float(v) => float4_out(*v, digits, &mut out),
            Value::Double(v) if binary => out.extend_from_slice(&v.to_bits().to_be_bytes()),
            Value::Double(v) => float8_out(*v, digits, &mut out),
            Value::Decimal { unscaled, scale, .. } if binary => {
                decimal_send(*unscaled, u32::from(*scale), &mut out)
            }
            Value::Decimal { unscaled, scale, .. } => {
                decimal_out(*unscaled, u32::from(*scale), &mut out)
            }
            Value::Numeric(v) if binary => numeric_send(&Numeric::from_bytes(v), &mut out),
            Value::Numeric(v) => numeric_out(&Numeric::from_bytes(v), &mut out),
            Value::Varchar(v) if binary && oid == oids::JSONB => crate::jsonb_send(v, &mut out),
            Value::Varchar(v) => out.extend_from_slice(v.as_bytes()),
            Value::Blob(v) if binary => out.extend_from_slice(v),
            Value::Blob(v) => bytea_out(v, settings.bytea_output, &mut out),
            Value::Date(v) => {
                let date = date_from_unix(*v).ok()?;
                match binary {
                    true => out.extend_from_slice(&date.to_be_bytes()),
                    false => date_out(date, settings.date_format, &mut out),
                }
            }
            Value::Time(v) if binary => out.extend_from_slice(&v.to_be_bytes()),
            Value::Time(v) => time_out(*v, &mut out),
            Value::TimeTz(key) => {
                let (time, zone) = (time_tz::micros(*key), -time_tz::offset(*key));
                match binary {
                    true => {
                        out.extend_from_slice(&time.to_be_bytes());
                        out.extend_from_slice(&zone.to_be_bytes());
                    }
                    false => timetz_out(time, zone, &mut out),
                }
            }
            Value::Timestamp(v) | Value::TimestampTz(v) => {
                let ts = timestamp_from_unix(*v).ok()?;
                match (binary, value) {
                    (true, _) => out.extend_from_slice(&ts.to_be_bytes()),
                    (false, Value::Timestamp(_)) => {
                        timestamp_out(ts, settings.date_format, &mut out).ok()?
                    }
                    (false, _) => {
                        timestamptz_out(ts, settings.date_format, settings.time_zone, &mut out)
                            .ok()?
                    }
                }
            }
            Value::Interval { months, days, micros } => {
                let iv = Interval { time: *micros, day: *days, month: *months };
                match binary {
                    true => interval_send(&iv, &mut out),
                    false => interval_out(&iv, settings.interval_style, &mut out),
                }
            }
            Value::Uuid(v) if binary => out.extend_from_slice(&uuid::to_bytes(*v)),
            Value::Uuid(v) => uuid_out(&uuid::to_bytes(*v), &mut out),
            other => panic!("the test has no reference for {other:?}"),
        }
        Some(out)
    }

    /// The values of each `DataRow` message in `bytes`.
    fn decode(mut bytes: &[u8]) -> Vec<Vec<Option<Vec<u8>>>> {
        let mut rows = Vec::new();
        while !bytes.is_empty() {
            assert_eq!(bytes[0], b'D');
            let len = i32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
            let mut message = &bytes[5..1 + len];
            bytes = &bytes[1 + len..];
            let count = u16::from_be_bytes(message[..2].try_into().unwrap());
            message = &message[2..];
            let mut row = Vec::new();
            for _ in 0..count {
                let len = i32::from_be_bytes(message[..4].try_into().unwrap());
                message = &message[4..];
                if len < 0 {
                    row.push(None);
                } else {
                    row.push(Some(message[..len as usize].to_vec()));
                    message = &message[len as usize..];
                }
            }
            assert!(message.is_empty(), "a message has bytes after its last value");
            rows.push(row);
        }
        rows
    }

    fn columns() -> Vec<(LogicalType, Oid, Vec<Value>)> {
        use LogicalType as L;
        let decimal = |unscaled, width, scale| Value::Decimal { unscaled, width, scale };
        vec![
            (
                L::Boolean,
                oids::BOOL,
                vec![Value::Boolean(true), Value::Null, Value::Boolean(false)],
            ),
            (
                L::UTinyInt,
                oids::CHAR,
                vec![Value::UTinyInt(b'a'), Value::UTinyInt(0), Value::UTinyInt(200)],
            ),
            (L::TinyInt, oids::INT2, vec![Value::TinyInt(-128), Value::TinyInt(7), Value::Null]),
            (
                L::SmallInt,
                oids::INT2,
                vec![Value::SmallInt(i16::MIN), Value::Null, Value::SmallInt(1)],
            ),
            (
                L::USmallInt,
                oids::INT4,
                vec![Value::USmallInt(u16::MAX), Value::USmallInt(0), Value::Null],
            ),
            (
                L::Integer,
                oids::INT4,
                vec![Value::Integer(i32::MIN), Value::Integer(-1), Value::Integer(10)],
            ),
            (
                L::UInteger,
                oids::INT8,
                vec![Value::UInteger(u32::MAX), Value::Null, Value::UInteger(3)],
            ),
            (
                L::BigInt,
                oids::INT8,
                vec![Value::BigInt(i64::MIN), Value::BigInt(0), Value::BigInt(i64::MAX)],
            ),
            (
                L::UInteger,
                oids::OID,
                vec![Value::UInteger(u32::MAX), Value::UInteger(0), Value::Null],
            ),
            (
                L::UInteger,
                oids::REGCLASS,
                vec![Value::UInteger(1259), Value::Null, Value::UInteger(9)],
            ),
            (
                L::Float,
                oids::FLOAT4,
                vec![Value::Float(1.5), Value::Float(f32::NAN), Value::Float(-0.0)],
            ),
            (
                L::Double,
                oids::FLOAT8,
                vec![Value::Double(0.1), Value::Double(f64::INFINITY), Value::Null],
            ),
            (
                L::Decimal { width: 4, scale: 2 },
                oids::NUMERIC,
                vec![decimal(-9999, 4, 2), decimal(5, 4, 2), Value::Null],
            ),
            (
                L::Decimal { width: 18, scale: 0 },
                oids::NUMERIC,
                vec![decimal(123456789012345678, 18, 0), decimal(0, 18, 0), decimal(-1, 18, 0)],
            ),
            (
                L::Decimal { width: 38, scale: 10 },
                oids::NUMERIC,
                vec![decimal(10i128.pow(37) + 1, 38, 10), Value::Null, decimal(-10, 38, 10)],
            ),
            (
                L::Varchar,
                oids::TEXT,
                vec![
                    Value::Varchar("a string longer than twelve bytes".into()),
                    Value::Varchar(String::new()),
                    Value::Varchar("é".into()),
                ],
            ),
            (
                L::Varchar,
                oids::VARCHAR,
                vec![Value::Null, Value::Varchar("x".into()), Value::Varchar("yz".into())],
            ),
            (
                L::Blob,
                oids::BYTEA,
                vec![Value::Blob(vec![0, b'\\', 200]), Value::Blob(Vec::new()), Value::Null],
            ),
            (
                L::Date,
                oids::DATE,
                vec![Value::Date(0), Value::Date(i32::MAX), Value::Date(-i32::MAX)],
            ),
            (L::Date, oids::DATE, vec![Value::Date(-719528), Value::Null, Value::Date(20000)]),
            (
                L::Time,
                oids::TIME,
                vec![Value::Time(0), Value::Time(86_400_000_000), Value::Time(1)],
            ),
            (
                L::TimeTz,
                oids::TIMETZ,
                vec![
                    Value::TimeTz(time_tz::pack(3_600_000_000, 3600)),
                    Value::TimeTz(time_tz::pack(0, -57599)),
                    Value::Null,
                ],
            ),
            (
                L::Timestamp,
                oids::TIMESTAMP,
                vec![Value::Timestamp(0), Value::Timestamp(i64::MAX), Value::Timestamp(-i64::MAX)],
            ),
            (
                L::TimestampTz,
                oids::TIMESTAMPTZ,
                vec![
                    Value::TimestampTz(1_700_000_000_123_456),
                    Value::Null,
                    Value::TimestampTz(-1),
                ],
            ),
            (
                L::Interval,
                oids::INTERVAL,
                vec![
                    Value::Interval { months: 14, days: -3, micros: 1_000_001 },
                    Value::Interval { months: 0, days: 0, micros: 0 },
                    Value::Null,
                ],
            ),
            (
                L::Uuid,
                oids::UUID,
                vec![
                    Value::Uuid(uuid::parse("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11").unwrap()),
                    Value::Null,
                    Value::Uuid(0),
                ],
            ),
        ]
    }

    /// Each column in each format, flat and as a constant, over each range of rows, against the
    /// reference.
    #[test]
    fn each_value_is_the_output_of_its_type() {
        let zone = FixedZone { offset: -5 * 3600, abbrev: "EST".into() };
        let settings = settings(&zone);
        let columns = columns();
        for binary in [false, true] {
            let types: Vec<_> =
                columns.iter().map(|(t, oid, _)| (t.clone(), *oid, binary)).collect();
            let mut encoder = RowEncoder::new(&types).unwrap();
            let flat: Vec<Vector> = columns
                .iter()
                .map(|(t, _, values)| Vector::from_values(t.clone(), values).unwrap())
                .collect();
            for rows in [0..3, 1..3, 2..3, 1..1] {
                let mut out = vec![b'x'];
                encoder.encode(&flat, rows.clone(), &settings, &mut out).unwrap();
                let got = decode(&out[1..]);
                assert_eq!(got.len(), rows.len());
                for (r, row) in rows.clone().enumerate() {
                    for (c, (_, oid, values)) in columns.iter().enumerate() {
                        let want = reference(&values[row], *oid, binary, &settings);
                        assert_eq!(
                            got[r][c], want,
                            "row {row} of {:?}, binary {binary}",
                            values[row]
                        );
                    }
                }
            }
            // A constant column is flattened first.
            let constant: Vec<Vector> = columns
                .iter()
                .map(|(t, _, values)| Vector::constant(t.clone(), values[0].clone(), 4))
                .collect();
            let mut out = Vec::new();
            encoder.encode(&constant, 0..4, &settings, &mut out).unwrap();
            let got = decode(&out);
            for row in &got {
                for (c, (_, oid, values)) in columns.iter().enumerate() {
                    assert_eq!(row[c], reference(&values[0], *oid, binary, &settings));
                }
            }
        }
    }

    /// The types with no PostgreSQL type of their own are sent as the text of each value.
    #[test]
    fn other_types_are_sent_as_text() {
        let zone = FixedZone::utc();
        let settings = settings(&zone);
        let big = Vector::from_values(
            LogicalType::HugeInt,
            &[Value::HugeInt(-(10i128.pow(30))), Value::Null, Value::HugeInt(7)],
        )
        .unwrap();
        let none = Vector::from_values(LogicalType::Null, &[Value::Null, Value::Null, Value::Null])
            .unwrap();
        // PostgreSQL has no arrays of arrays, so a list of lists is text.
        let ints = LogicalType::List(Box::new(LogicalType::Integer));
        let pair = Value::List {
            element: ints.clone(),
            values: vec![Value::List {
                element: LogicalType::Integer,
                values: vec![Value::Integer(1), Value::Integer(2)],
            }],
        };
        let list_type = LogicalType::List(Box::new(ints.clone()));
        let list = Vector::from_values(
            list_type.clone(),
            &[pair.clone(), Value::Null, Value::List { element: ints, values: Vec::new() }],
        )
        .unwrap();
        let columns = [big, none, list];
        let types = [LogicalType::HugeInt, LogicalType::Null, list_type];
        let oids: Vec<_> = types.iter().map(|t| pg_type(t).oid).collect();
        assert_eq!(oids, [oids::NUMERIC, oids::TEXT, oids::TEXT]);
        for binary in [false, true] {
            let spec: Vec<_> =
                types.iter().zip(&oids).map(|(t, oid)| (t.clone(), *oid, binary)).collect();
            let mut encoder = RowEncoder::new(&spec).unwrap();
            let mut out = Vec::new();
            encoder.encode(&columns, 0..3, &settings, &mut out).unwrap();
            let rows = decode(&out);
            let numeric = |text: &str| {
                let mut out = Vec::new();
                match binary {
                    true => numeric_send(&numeric_in(text, -1).unwrap(), &mut out),
                    false => out.extend_from_slice(text.as_bytes()),
                }
                Some(out)
            };
            assert_eq!(rows[0][0], numeric("-1000000000000000000000000000000"));
            assert_eq!(rows[2][0], numeric("7"));
            assert_eq!(rows[1][0], None);
            assert!(rows.iter().all(|row| row[1].is_none()));
            assert_eq!(rows[0][2].as_deref(), Some(pair.to_string().as_bytes()));
            assert_eq!(rows[1][2], None);
        }
        let decimal = pg_type(&LogicalType::Decimal { width: 10, scale: 2 });
        assert_eq!((decimal.oid, decimal.typmod), (oids::NUMERIC, numeric_typmod(10, 2)));
    }

    /// A list is an array of its element type, in the text and the binary format of
    /// `array_out` and `array_send`.
    #[test]
    fn a_list_is_an_array() {
        let zone = FixedZone::utc();
        let settings = settings(&zone);
        let list = |element: LogicalType, values: Vec<Value>| Value::List { element, values };
        let ints = LogicalType::List(Box::new(LogicalType::Integer));
        let texts = LogicalType::List(Box::new(LogicalType::Varchar));
        let numbers = Vector::from_values(
            ints.clone(),
            &[
                list(LogicalType::Integer, vec![Value::Integer(1), Value::Null, Value::Integer(3)]),
                Value::Null,
                list(LogicalType::Integer, Vec::new()),
            ],
        )
        .unwrap();
        let words = Vector::from_values(
            texts.clone(),
            &[
                list(LogicalType::Varchar, vec![Value::Varchar("a b".into())]),
                list(LogicalType::Varchar, vec![Value::Varchar("NULL".into()), Value::Null]),
                list(LogicalType::Varchar, vec![Value::Varchar(String::new())]),
            ],
        )
        .unwrap();
        assert_eq!(pg_type(&ints).oid, oids::INT4_ARRAY);
        assert_eq!(pg_type(&texts).oid, oids::TEXT_ARRAY);
        let columns = [numbers, words];
        let text = |binary| {
            let spec = [
                (ints.clone(), oids::INT4_ARRAY, binary),
                (texts.clone(), oids::TEXT_ARRAY, binary),
            ];
            let mut out = Vec::new();
            RowEncoder::new(&spec).unwrap().encode(&columns, 0..3, &settings, &mut out).unwrap();
            decode(&out)
        };
        let rows = text(false);
        let cell = |row: usize, column: usize| {
            rows[row][column].clone().map(|v| String::from_utf8(v).unwrap())
        };
        assert_eq!(cell(0, 0).as_deref(), Some("{1,NULL,3}"));
        assert_eq!(cell(1, 0), None);
        assert_eq!(cell(2, 0).as_deref(), Some("{}"));
        assert_eq!(cell(0, 1).as_deref(), Some(r#"{"a b"}"#));
        assert_eq!(cell(1, 1).as_deref(), Some(r#"{"NULL",NULL}"#));
        assert_eq!(cell(2, 1).as_deref(), Some(r#"{""}"#));
        let rows = text(true);
        let words: Vec<u8> =
            [1i32, 1, 23, 3, 1, 4, 1, -1, 4, 3].iter().flat_map(|w| w.to_be_bytes()).collect();
        assert_eq!(rows[0][0].as_deref(), Some(&words[..]));
        assert_eq!(rows[2][0].as_deref(), Some(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 23][..]));
        let mut empty = Vec::new();
        for w in [1i32, 0, 25, 1, 1, 0] {
            empty.extend_from_slice(&w.to_be_bytes());
        }
        assert_eq!(rows[2][1].as_deref(), Some(&empty[..]));
    }

    #[test]
    fn a_vector_has_no_braces_and_an_oid_alias_has_a_name() {
        let zone = FixedZone::utc();
        let settings = settings(&zone);
        let list = |element: LogicalType, values: Vec<Value>| Value::List { element, values };
        let int2s = LogicalType::List(Box::new(LogicalType::SmallInt));
        let oids_type = LogicalType::List(Box::new(LogicalType::UInteger));
        let vectors = Vector::from_values(
            int2s.clone(),
            &[
                list(LogicalType::SmallInt, vec![Value::SmallInt(1), Value::SmallInt(-2)]),
                list(LogicalType::SmallInt, Vec::new()),
                Value::Null,
            ],
        )
        .unwrap();
        let oid_vectors = Vector::from_values(
            oids_type.clone(),
            &[
                list(LogicalType::UInteger, vec![Value::UInteger(23), Value::UInteger(25)]),
                list(LogicalType::UInteger, vec![Value::UInteger(0)]),
                list(LogicalType::UInteger, Vec::new()),
            ],
        )
        .unwrap();
        let ids = [Value::UInteger(23), Value::UInteger(0), Value::UInteger(oids::INT4_ARRAY)];
        let types = Vector::from_values(LogicalType::UInteger, &ids).unwrap();
        let ids = [Value::UInteger(1259), Value::UInteger(0), Value::Null];
        let classes = Vector::from_values(LogicalType::UInteger, &ids).unwrap();
        let columns = [vectors, oid_vectors, types, classes];
        let rows = |binary| {
            let spec = [
                (int2s.clone(), oids::INT2VECTOR, binary),
                (oids_type.clone(), oids::OIDVECTOR, binary),
                (LogicalType::UInteger, oids::REGTYPE, binary),
                (LogicalType::UInteger, oids::REGCLASS, binary),
            ];
            let mut out = Vec::new();
            RowEncoder::new(&spec).unwrap().encode(&columns, 0..3, &settings, &mut out).unwrap();
            decode(&out)
        };
        let text = rows(false);
        let cells: Vec<Vec<Option<String>>> = text
            .iter()
            .map(|row| {
                row.iter().map(|v| v.clone().map(|v| String::from_utf8(v).unwrap())).collect()
            })
            .collect();
        let some = |s: &str| Some(s.to_owned());
        assert_eq!(cells[0], [some("1 -2"), some("23 25"), some("integer"), some("1259")]);
        assert_eq!(cells[1], [some(""), some("0"), some("-"), some("-")]);
        assert_eq!(cells[2], [None, some(""), some("integer[]"), None]);
        let binary = rows(true);
        let mut expected = Vec::new();
        crate::array::int2vector_send(&[1, -2], &mut expected);
        assert_eq!(binary[0][0].as_deref(), Some(&expected[..]));
        expected.clear();
        crate::array::int2vector_send(&[], &mut expected);
        assert_eq!(binary[1][0].as_deref(), Some(&expected[..]));
        expected.clear();
        crate::array::oidvector_send(&[23, 25], &mut expected);
        assert_eq!(binary[0][1].as_deref(), Some(&expected[..]));
        assert_eq!(binary[0][2].as_deref(), Some(&23u32.to_be_bytes()[..]));
    }

    #[test]
    fn a_char_column_is_padded_to_its_length() {
        let zone = FixedZone::utc();
        let settings = settings(&zone);
        let values = ["ab", "", "é", "abcd"].map(|v| Value::Varchar(v.into()));
        let mut values = values.to_vec();
        values.push(Value::Null);
        let column = Vector::from_values(LogicalType::Varchar, &values).unwrap();
        for binary in [false, true] {
            let columns = [(LogicalType::Varchar, oids::BPCHAR, binary)];
            let mut encoder = RowEncoder::with_typmods(&columns, &[8]).unwrap();
            let mut out = Vec::new();
            encoder.encode(std::slice::from_ref(&column), 0..5, &settings, &mut out).unwrap();
            let values: Vec<_> = decode(&out).into_iter().map(|row| row[0].clone()).collect();
            let padded = ["ab  ", "    ", "é   ", "abcd"].map(|v| Some(v.as_bytes().to_vec()));
            assert_eq!(values[..4], padded);
            assert_eq!(values[4], None);
        }
        // A `bpchar` with no length is sent as it is.
        let mut encoder = RowEncoder::new(&[(LogicalType::Varchar, oids::BPCHAR, false)]).unwrap();
        let mut out = Vec::new();
        encoder.encode(std::slice::from_ref(&column), 0..1, &settings, &mut out).unwrap();
        assert_eq!(decode(&out)[0][0], Some(b"ab".to_vec()));
    }

    #[test]
    fn a_dictionary_column_and_errors() {
        let zone = FixedZone::utc();
        let settings = settings(&zone);
        let words = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("one".into()), Value::Varchar("two".into())],
        )
        .unwrap();
        let column = Vector::dictionary(vec![1, 0, 1], words).unwrap();
        let mut encoder = RowEncoder::new(&[(LogicalType::Varchar, oids::TEXT, false)]).unwrap();
        let mut out = Vec::new();
        encoder.encode(&[column], 0..3, &settings, &mut out).unwrap();
        let values: Vec<_> = decode(&out).into_iter().map(|row| row[0].clone().unwrap()).collect();
        assert_eq!(values, [b"two".to_vec(), b"one".to_vec(), b"two".to_vec()]);

        let error = RowEncoder::new(&[(LogicalType::HugeInt, oids::INT8, false)]).unwrap_err();
        assert_eq!(error.message, "cannot send a value of type HUGEINT as type bigint");

        // A date after 5874897 AD has no PostgreSQL form, and the output is left as it was.
        let late =
            Vector::from_values(LogicalType::Date, &[Value::Date(0), Value::Date(i32::MAX - 1)])
                .unwrap();
        for binary in [false, true] {
            let mut encoder = RowEncoder::new(&[(LogicalType::Date, oids::DATE, binary)]).unwrap();
            let mut out = vec![1, 2];
            let error =
                encoder.encode(std::slice::from_ref(&late), 0..2, &settings, &mut out).unwrap_err();
            assert_eq!(error.message, "date out of range");
            assert_eq!(out, [1, 2]);
        }
    }
}
