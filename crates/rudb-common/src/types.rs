//! The type system, per `spec/10-sql-and-types.md` section 10.1.
//!
//! A logical type is what SQL talks about. A physical type is how it is laid out. Keeping the two
//! apart is what lets `DECIMAL(9, 2)` be stored in an `i32` without the planner having to know,
//! and it is the same separation that later lets a `VARCHAR` column be handed to an operator as
//! dictionary codes.
//!
//! Type names are spelled the way DuckDB spells them, including the aliases, because
//! `spec/12-duckdb-compat.md` makes the dialect a compatibility surface and `CREATE TABLE t (a
//! INT4)` is a thing people write.

use std::fmt;

use crate::error::{Error, Result};

/// A named field of a `STRUCT` or a `UNION`, and a named column of a table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Field {
    /// The field name, unquoted and case sensitive as stored.
    pub name: String,
    /// The field type.
    pub ty: LogicalType,
    /// Whether the column refuses nulls, which is the `NOT NULL` the DDL declared it with.
    ///
    /// Always false inside a `STRUCT` or a `UNION`, because a null is a property of a value at
    /// every nesting level and no type in SQL says a value cannot be one. This is here rather than
    /// on a separate column type because a table column is already spelled with this struct, and a
    /// second one that was this one plus a flag would have to be threaded through the binder, the
    /// scope and every operator schema to carry a bit that only the insert path reads.
    pub not_null: bool,
}

impl Field {
    /// A field with a name and a type, which accepts nulls.
    pub fn new(name: impl Into<String>, ty: LogicalType) -> Self {
        Self { name: name.into(), ty, not_null: false }
    }

    /// A column with a name and a type, which refuses nulls.
    pub fn required(name: impl Into<String>, ty: LogicalType) -> Self {
        Self { name: name.into(), ty, not_null: true }
    }
}

/// What SQL thinks a value is.
///
/// Nulls are not in here. `spec/10-sql-and-types.md` says null is a per-value property at every
/// nesting level, which makes it a property of a vector's validity mask rather than of a type.
/// The one exception is [`LogicalType::Null`], which is the type of a literal `NULL` before
/// anything has told it what it is, and which every other type absorbs during resolution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LogicalType {
    /// The type of an untyped `NULL` literal.
    Null,
    /// `BOOLEAN`.
    Boolean,
    /// `TINYINT`, 8 bits signed.
    TinyInt,
    /// `SMALLINT`, 16 bits signed.
    SmallInt,
    /// `INTEGER`, 32 bits signed.
    Integer,
    /// `BIGINT`, 64 bits signed.
    BigInt,
    /// `HUGEINT`, 128 bits signed.
    HugeInt,
    /// `UTINYINT`, 8 bits unsigned.
    UTinyInt,
    /// `USMALLINT`, 16 bits unsigned.
    USmallInt,
    /// `UINTEGER`, 32 bits unsigned.
    UInteger,
    /// `UBIGINT`, 64 bits unsigned.
    UBigInt,
    /// `UHUGEINT`, 128 bits unsigned.
    UHugeInt,
    /// `FLOAT`, IEEE 754 binary32.
    Float,
    /// `DOUBLE`, IEEE 754 binary64.
    Double,
    /// `DECIMAL(width, scale)`, stored in the narrowest integer that holds `width` digits.
    Decimal {
        /// Total number of decimal digits, 1 through 38.
        width: u8,
        /// Digits to the right of the point, no greater than `width`.
        scale: u8,
    },
    /// `VARCHAR`. Length modifiers parse and are then ignored, as they are in DuckDB.
    Varchar,
    /// `BLOB`.
    Blob,
    /// `BIT`, a bit string.
    Bit,
    /// `UUID`.
    Uuid,
    /// `DATE`, days since 1970-01-01.
    Date,
    /// `TIME`, microseconds since midnight.
    Time,
    /// `TIME WITH TIME ZONE`.
    TimeTz,
    /// `TIMESTAMP`, microseconds since the epoch.
    Timestamp,
    /// `TIMESTAMP_S`, seconds since the epoch.
    TimestampS,
    /// `TIMESTAMP_MS`, milliseconds since the epoch.
    TimestampMs,
    /// `TIMESTAMP_NS`, nanoseconds since the epoch.
    TimestampNs,
    /// `TIMESTAMP WITH TIME ZONE`.
    TimestampTz,
    /// `INTERVAL`, the months, days and microseconds triple.
    Interval,
    /// `T[]`, a variable length list.
    List(Box<LogicalType>),
    /// `T[n]`, a fixed length array.
    Array(Box<LogicalType>, u32),
    /// `STRUCT(name type, ...)`.
    Struct(Vec<Field>),
    /// `MAP(key, value)`.
    Map(Box<LogicalType>, Box<LogicalType>),
    /// `UNION(tag type, ...)`.
    Union(Vec<Field>),
}

/// How a value is actually laid out in a vector.
///
/// The planner picks operators off this rather than off the logical type, which is why `DATE` and
/// `INTEGER` share an implementation of everything that does not care what the number means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PhysicalType {
    /// One byte per value, zero or one.
    Bool,
    /// 8 bit signed.
    Int8,
    /// 16 bit signed.
    Int16,
    /// 32 bit signed.
    Int32,
    /// 64 bit signed.
    Int64,
    /// 128 bit signed.
    Int128,
    /// 8 bit unsigned.
    UInt8,
    /// 16 bit unsigned.
    UInt16,
    /// 32 bit unsigned.
    UInt32,
    /// 64 bit unsigned.
    UInt64,
    /// 128 bit unsigned.
    UInt128,
    /// IEEE 754 binary32.
    Float32,
    /// IEEE 754 binary64.
    Float64,
    /// The months, days and microseconds triple.
    Interval,
    /// The 16 byte string representation from `spec/07-execution.md` section 7.1.
    Varlen,
    /// Offsets plus one child column.
    List,
    /// A fixed stride into one child column.
    Array,
    /// Named child columns, one per field.
    Struct,
    /// No storage. Only the validity mask says anything.
    Empty,
}

impl LogicalType {
    /// A `DECIMAL(width, scale)`, checked.
    ///
    /// # Errors
    ///
    /// If the width is zero or above 38, or the scale is greater than the width. Those are the
    /// same bounds DuckDB enforces and the message is the same message.
    pub fn decimal(width: u8, scale: u8) -> Result<Self> {
        if width == 0 || width > MAX_DECIMAL_WIDTH {
            return Err(Error::binder(format!("Width must be between 1 and {MAX_DECIMAL_WIDTH}!")));
        }
        if scale > width {
            return Err(Error::binder(format!(
                "Scale cannot be bigger than width, {scale} is bigger than {width}"
            )));
        }
        Ok(Self::Decimal { width, scale })
    }

    /// A list of `element`.
    #[must_use]
    pub fn list(element: Self) -> Self {
        Self::List(Box::new(element))
    }

    /// A fixed length array of `element`.
    #[must_use]
    pub fn array(element: Self, length: u32) -> Self {
        Self::Array(Box::new(element), length)
    }

    /// A map from `key` to `value`.
    #[must_use]
    pub fn map(key: Self, value: Self) -> Self {
        Self::Map(Box::new(key), Box::new(value))
    }

    /// How this type is laid out.
    #[must_use]
    pub fn physical(&self) -> PhysicalType {
        match self {
            Self::Null => PhysicalType::Empty,
            Self::Boolean => PhysicalType::Bool,
            Self::TinyInt => PhysicalType::Int8,
            Self::SmallInt => PhysicalType::Int16,
            Self::Integer | Self::Date => PhysicalType::Int32,
            Self::BigInt
            | Self::Time
            | Self::TimeTz
            | Self::Timestamp
            | Self::TimestampS
            | Self::TimestampMs
            | Self::TimestampNs
            | Self::TimestampTz => PhysicalType::Int64,
            Self::HugeInt | Self::Uuid => PhysicalType::Int128,
            Self::UTinyInt => PhysicalType::UInt8,
            Self::USmallInt => PhysicalType::UInt16,
            Self::UInteger => PhysicalType::UInt32,
            Self::UBigInt => PhysicalType::UInt64,
            Self::UHugeInt => PhysicalType::UInt128,
            Self::Float => PhysicalType::Float32,
            Self::Double => PhysicalType::Float64,
            // The narrowest integer that holds the requested number of digits, which is the
            // standard representation and the one DuckDB uses. A DECIMAL(9, 2) column costs four
            // bytes a value and not sixteen.
            Self::Decimal { width, .. } => match width {
                0..=4 => PhysicalType::Int16,
                5..=9 => PhysicalType::Int32,
                10..=18 => PhysicalType::Int64,
                _ => PhysicalType::Int128,
            },
            Self::Varchar | Self::Blob | Self::Bit => PhysicalType::Varlen,
            Self::Interval => PhysicalType::Interval,
            // A map is a list of two-field structs, which is how Arrow does it and how every
            // engine that has to interoperate with Arrow ends up doing it.
            Self::List(_) | Self::Map(_, _) => PhysicalType::List,
            Self::Array(_, _) => PhysicalType::Array,
            Self::Struct(_) | Self::Union(_) => PhysicalType::Struct,
        }
    }

    /// The name DuckDB's messages give this type, which is the integer it is stored in rather than
    /// the type it is written as.
    ///
    /// An overflow says `INT32` and not `INTEGER`, a failed cast says `INT8` and not `TINYINT`, and
    /// a boolean is `BOOL` in both. A decimal carries the width of the integer behind it rather
    /// than the width that was declared, so a DECIMAL(18,8) and a DECIMAL(11,0) are both
    /// `DECIMAL(18)`. Everything that is not a number is written the way it is spelled.
    #[must_use]
    pub fn physical_name(&self) -> String {
        if let Some(width) = self.decimal_storage() {
            return format!("DECIMAL({width})");
        }
        let name = match self {
            Self::Boolean => "BOOL",
            Self::TinyInt => "INT8",
            Self::SmallInt => "INT16",
            Self::Integer => "INT32",
            Self::BigInt => "INT64",
            Self::HugeInt => "INT128",
            Self::UTinyInt => "UINT8",
            Self::USmallInt => "UINT16",
            Self::UInteger => "UINT32",
            Self::UBigInt => "UINT64",
            Self::UHugeInt => "UINT128",
            other => return other.to_string(),
        };
        name.to_string()
    }

    /// The widest decimal the integer behind this one holds, or `None` when this is not a decimal.
    ///
    /// A decimal is stored in the narrowest of `i16`, `i32`, `i64` and `i128` that fits its width,
    /// and a message names the bucket rather than the declaration, so a DECIMAL(18,8) and a
    /// DECIMAL(11,0) are both `DECIMAL(18)`. The two wide buckets were measured. The two narrow
    /// ones follow the same rule and are hard to reach, since a decimal that narrow widens before
    /// it can overflow.
    #[must_use]
    pub fn decimal_storage(&self) -> Option<u8> {
        let Self::Decimal { width, .. } = self else {
            return None;
        };
        Some(match width {
            0..=4 => 4,
            5..=9 => 9,
            10..=18 => 18,
            _ => MAX_DECIMAL_WIDTH,
        })
    }

    /// Whether arithmetic applies.
    #[must_use]
    pub fn is_numeric(&self) -> bool {
        self.is_integer() || matches!(self, Self::Float | Self::Double | Self::Decimal { .. })
    }

    /// Whether this is one of the integer types, signed or unsigned.
    #[must_use]
    pub fn is_integer(&self) -> bool {
        matches!(
            self,
            Self::TinyInt
                | Self::SmallInt
                | Self::Integer
                | Self::BigInt
                | Self::HugeInt
                | Self::UTinyInt
                | Self::USmallInt
                | Self::UInteger
                | Self::UBigInt
                | Self::UHugeInt
        )
    }

    /// The width and scale of the decimal that holds every value of this type exactly.
    ///
    /// A decimal is its own, an integer is one with no fraction and room for its digits, and
    /// nothing else has one. This is what the rule for the type of a product is written in terms
    /// of, since `DECIMAL(4,2) * INTEGER` is as wide as `DECIMAL(4,2) * DECIMAL(10,0)` upstream and
    /// the two ought to be the same line of code here.
    #[must_use]
    pub fn decimal_shape(&self) -> Option<(u8, u8)> {
        match self {
            Self::Decimal { width, scale } => Some((*width, *scale)),
            other if other.is_integer() => Some((decimal_digits(other), 0)),
            _ => None,
        }
    }

    /// Whether this is a date, a time, a timestamp or an interval.
    #[must_use]
    pub fn is_temporal(&self) -> bool {
        matches!(
            self,
            Self::Date
                | Self::Time
                | Self::TimeTz
                | Self::Timestamp
                | Self::TimestampS
                | Self::TimestampMs
                | Self::TimestampNs
                | Self::TimestampTz
                | Self::Interval
        )
    }

    /// Whether this type contains other types.
    ///
    /// Nested types are stored columnar all the way down, so this is the question of whether a
    /// column of this type is one column chunk or several.
    #[must_use]
    pub fn is_nested(&self) -> bool {
        matches!(
            self,
            Self::List(_) | Self::Array(_, _) | Self::Struct(_) | Self::Map(_, _) | Self::Union(_)
        )
    }

    /// The type both of these can be cast to without losing a value, if there is one.
    ///
    /// This is DuckDB's `MaxLogicalType` and it is where the type of `a + b` starts, and it is the
    /// whole answer for the arms of a `CASE` and the columns of a `UNION`. An addition takes one
    /// more digit than this when the answer is a decimal, because two eighteen digit numbers add to
    /// nineteen, and that part is the signature table's rather than this function's: a `UNION` of
    /// two `DECIMAL(18,0)` columns is a `DECIMAL(18,0)` and their sum is not. The rule is a total
    /// order over the numeric types
    /// with everything else absorbing into `VARCHAR` only when it is asked to, and `NULL` absorbing
    /// into anything, which is what makes `CASE WHEN c THEN NULL ELSE 1 END` an integer.
    ///
    /// Mixing signed and unsigned widens rather than reinterprets, so `INTEGER` and `UINTEGER`
    /// promote to `BIGINT` and not to either of themselves. That costs a byte per value on a case
    /// that is rare and it is the only version that never silently changes a number, which matters
    /// more here than the byte does: a wrong answer that is off by 4,294,967,296 is the worst kind
    /// of bug this engine can have.
    ///
    /// Returns `None` when there is no such type, which is the binder's cue to raise rather than to
    /// guess. Two different structs are `None` and not a struct of promoted fields, because field
    /// order and field names would have to match and a rule that sometimes works is worse here than
    /// one that never does.
    #[must_use]
    pub fn promote(&self, other: &Self) -> Option<Self> {
        if self == other {
            return Some(self.clone());
        }
        match (self, other) {
            (Self::Null, ty) | (ty, Self::Null) => Some(ty.clone()),
            (Self::List(left), Self::List(right)) => Some(Self::list(left.promote(right)?)),
            _ if self.is_numeric() && other.is_numeric() => {
                Some(promote_numeric(self.clone(), other.clone()))
            }
            // A date and a timestamp meet at the wider one, which is the timestamp, and the same
            // holds for the timestamp units. `rank_temporal` is what says which is wider.
            _ if self.is_temporal() && other.is_temporal() => {
                match (rank_temporal(self), rank_temporal(other)) {
                    (Some(left), Some(right)) => {
                        Some(if left >= right { self.clone() } else { other.clone() })
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The types this one contains, in child column order, or empty for a scalar type.
    #[must_use]
    pub fn children(&self) -> Vec<Self> {
        match self {
            Self::List(inner) | Self::Array(inner, _) => vec![inner.as_ref().clone()],
            Self::Map(key, value) => vec![key.as_ref().clone(), value.as_ref().clone()],
            Self::Struct(fields) | Self::Union(fields) => {
                fields.iter().map(|f| f.ty.clone()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Parses a SQL type name, aliases included.
    ///
    /// # Errors
    ///
    /// If the text is not a type name this understands. The message names the offending word,
    /// because a type parse failure two levels inside a `STRUCT` is otherwise unreadable.
    pub fn parse(text: &str) -> Result<Self> {
        let tokens = lex(text)?;
        let mut parser = TypeParser { tokens: &tokens, position: 0 };
        let ty = parser.parse_type()?;
        if parser.position != parser.tokens.len() {
            return Err(Error::parser(format!("Type \"{text}\" has trailing text")));
        }
        Ok(ty)
    }
}

/// The largest number of decimal digits a `DECIMAL` can carry, because 38 digits is what fits in
/// 128 bits and there is no wider physical type.
pub const MAX_DECIMAL_WIDTH: u8 = 38;

impl fmt::Display for LogicalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("\"NULL\""),
            Self::Boolean => f.write_str("BOOLEAN"),
            Self::TinyInt => f.write_str("TINYINT"),
            Self::SmallInt => f.write_str("SMALLINT"),
            Self::Integer => f.write_str("INTEGER"),
            Self::BigInt => f.write_str("BIGINT"),
            Self::HugeInt => f.write_str("HUGEINT"),
            Self::UTinyInt => f.write_str("UTINYINT"),
            Self::USmallInt => f.write_str("USMALLINT"),
            Self::UInteger => f.write_str("UINTEGER"),
            Self::UBigInt => f.write_str("UBIGINT"),
            Self::UHugeInt => f.write_str("UHUGEINT"),
            Self::Float => f.write_str("FLOAT"),
            Self::Double => f.write_str("DOUBLE"),
            Self::Decimal { width, scale } => write!(f, "DECIMAL({width},{scale})"),
            Self::Varchar => f.write_str("VARCHAR"),
            Self::Blob => f.write_str("BLOB"),
            Self::Bit => f.write_str("BIT"),
            Self::Uuid => f.write_str("UUID"),
            Self::Date => f.write_str("DATE"),
            Self::Time => f.write_str("TIME"),
            Self::TimeTz => f.write_str("TIME WITH TIME ZONE"),
            Self::Timestamp => f.write_str("TIMESTAMP"),
            Self::TimestampS => f.write_str("TIMESTAMP_S"),
            Self::TimestampMs => f.write_str("TIMESTAMP_MS"),
            Self::TimestampNs => f.write_str("TIMESTAMP_NS"),
            Self::TimestampTz => f.write_str("TIMESTAMP WITH TIME ZONE"),
            Self::Interval => f.write_str("INTERVAL"),
            Self::List(inner) => write!(f, "{inner}[]"),
            Self::Array(inner, length) => write!(f, "{inner}[{length}]"),
            Self::Map(key, value) => write!(f, "MAP({key}, {value})"),
            Self::Struct(fields) => write_fields(f, "STRUCT", fields),
            Self::Union(fields) => write_fields(f, "UNION", fields),
        }
    }
}

/// Where a numeric type sits in the widening order.
///
/// The integers are ordered by how many values they hold, which is why an unsigned type ranks
/// above the signed type of the same width. That order alone is not enough to promote a signed
/// type with an unsigned one, since neither holds the other, and [`promote_integers`] is what
/// handles that case before this function is reached.
fn rank_numeric(ty: &LogicalType) -> u8 {
    match ty {
        LogicalType::TinyInt => 1,
        LogicalType::UTinyInt => 2,
        LogicalType::SmallInt => 3,
        LogicalType::USmallInt => 4,
        LogicalType::Integer => 5,
        LogicalType::UInteger => 6,
        LogicalType::BigInt => 7,
        LogicalType::UBigInt => 8,
        LogicalType::HugeInt => 9,
        LogicalType::UHugeInt => 10,
        LogicalType::Decimal { .. } => 11,
        LogicalType::Float => 12,
        LogicalType::Double => 13,
        _ => 0,
    }
}

/// Whether an integer type is signed, and how many bits it is.
fn integer_shape(ty: &LogicalType) -> (bool, u8) {
    match ty {
        LogicalType::TinyInt => (true, 8),
        LogicalType::SmallInt => (true, 16),
        LogicalType::Integer => (true, 32),
        LogicalType::BigInt => (true, 64),
        LogicalType::HugeInt => (true, 128),
        LogicalType::UTinyInt => (false, 8),
        LogicalType::USmallInt => (false, 16),
        LogicalType::UInteger => (false, 32),
        LogicalType::UBigInt => (false, 64),
        _ => (false, 128),
    }
}

/// The integer type of that many bits and that signedness.
fn integer_of(signed: bool, bits: u8) -> Option<LogicalType> {
    Some(match (signed, bits) {
        (true, 8) => LogicalType::TinyInt,
        (true, 16) => LogicalType::SmallInt,
        (true, 32) => LogicalType::Integer,
        (true, 64) => LogicalType::BigInt,
        (true, 128) => LogicalType::HugeInt,
        (false, 8) => LogicalType::UTinyInt,
        (false, 16) => LogicalType::USmallInt,
        (false, 32) => LogicalType::UInteger,
        (false, 64) => LogicalType::UBigInt,
        (false, 128) => LogicalType::UHugeInt,
        _ => return None,
    })
}

/// The narrowest integer type that holds every value of both.
///
/// Two of the same signedness are just the wider one. A signed and an unsigned need a signed type
/// strictly wider than the unsigned one, because a `UBIGINT` of 2^63 does not fit in a `BIGINT`
/// and a `BIGINT` of -1 does not fit in a `UBIGINT`. When that runs off the end of the integer
/// types, which is only `UHUGEINT` against a signed type, the answer is `DOUBLE`: it loses
/// precision past 2^53 and it is the only thing left, and DuckDB does the same.
fn promote_integers(left: &LogicalType, right: &LogicalType) -> LogicalType {
    let (left_signed, left_bits) = integer_shape(left);
    let (right_signed, right_bits) = integer_shape(right);
    if left_signed == right_signed {
        return if left_bits >= right_bits { left.clone() } else { right.clone() };
    }
    let (signed_bits, unsigned_bits) =
        if left_signed { (left_bits, right_bits) } else { (right_bits, left_bits) };
    let wanted = signed_bits.max(unsigned_bits.saturating_mul(2));
    integer_of(true, wanted).unwrap_or(LogicalType::Double)
}

/// The narrowest numeric type that holds every value of both.
fn promote_numeric(left: LogicalType, right: LogicalType) -> LogicalType {
    // A decimal and a float meet at the float, since the ranks already say so, but two decimals
    // meet at one wide enough for both the integer part and the fraction of each, which the ranks
    // cannot express.
    if let (
        LogicalType::Decimal { width: left_width, scale: left_scale },
        LogicalType::Decimal { width: right_width, scale: right_scale },
    ) = (&left, &right)
    {
        let scale = (*left_scale).max(*right_scale);
        let integral =
            left_width.saturating_sub(*left_scale).max(right_width.saturating_sub(*right_scale));
        let width = integral.saturating_add(scale).min(MAX_DECIMAL_WIDTH);
        return LogicalType::Decimal { width, scale: scale.min(width) };
    }
    // An integer and a decimal have to leave room for the integer's digits to the left of the
    // point, so the decimal widens rather than the integer simply casting into it.
    let widened = match (&left, &right) {
        (LogicalType::Decimal { width, scale }, other)
        | (other, LogicalType::Decimal { width, scale })
            if other.is_integer() =>
        {
            let needed = decimal_digits(other).saturating_add(*scale).min(MAX_DECIMAL_WIDTH);
            Some(LogicalType::Decimal { width: (*width).max(needed), scale: *scale })
        }
        _ => None,
    };
    if let Some(ty) = widened {
        return ty;
    }
    if left.is_integer() && right.is_integer() {
        return promote_integers(&left, &right);
    }
    if rank_numeric(&left) >= rank_numeric(&right) { left } else { right }
}

/// How many decimal digits an integer type needs, which is what a decimal has to leave room for.
///
/// It is the digits of the largest value the type holds, which is why `BIGINT` is nineteen and
/// `UBIGINT` is twenty: 9,223,372,036,854,775,807 against 18,446,744,073,709,551,615. The pair
/// below it is not like that, because a signed type and the unsigned type of the same width have
/// the same digit count once the sign is off the front. Measured on `v2.0.0-dev84237` through the
/// type of a sum: `DECIMAL(4,2)` with a `BIGINT` is `DECIMAL(22,2)` and with a `UBIGINT` is
/// `DECIMAL(23,2)`.
fn decimal_digits(ty: &LogicalType) -> u8 {
    match ty {
        LogicalType::TinyInt | LogicalType::UTinyInt => 3,
        LogicalType::SmallInt | LogicalType::USmallInt => 5,
        LogicalType::Integer | LogicalType::UInteger => 10,
        LogicalType::BigInt => 19,
        LogicalType::UBigInt => 20,
        _ => MAX_DECIMAL_WIDTH,
    }
}

/// Where a temporal type sits in the widening order, or `None` if it does not widen into another.
///
/// An interval is a duration and not a point in time, so it has no rank and never promotes with a
/// timestamp. That is the difference between `t + INTERVAL 1 DAY`, which is a function call the
/// binder resolves, and `CASE WHEN c THEN t ELSE INTERVAL 1 DAY END`, which has no type.
fn rank_temporal(ty: &LogicalType) -> Option<u8> {
    match ty {
        LogicalType::Date => Some(1),
        LogicalType::TimestampS => Some(2),
        LogicalType::TimestampMs => Some(3),
        LogicalType::Timestamp => Some(4),
        LogicalType::TimestampNs => Some(5),
        LogicalType::TimestampTz => Some(6),
        _ => None,
    }
}

fn write_fields(f: &mut fmt::Formatter<'_>, keyword: &str, fields: &[Field]) -> fmt::Result {
    f.write_str(keyword)?;
    f.write_str("(")?;
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            f.write_str(", ")?;
        }
        write_identifier(f, &field.name)?;
        write!(f, " {}", field.ty)?;
    }
    f.write_str(")")
}

/// Writes a field name, quoting it if it would not survive being read back unquoted.
fn write_identifier(f: &mut fmt::Formatter<'_>, name: &str) -> fmt::Result {
    let plain = !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        f.write_str(name)
    } else {
        f.write_str("\"")?;
        for c in name.chars() {
            if c == '"' {
                f.write_str("\"\"")?;
            } else {
                write!(f, "{c}")?;
            }
        }
        f.write_str("\"")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Quoted(String),
    Number(u32),
    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    Comma,
}

fn lex(text: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                tokens.push(Token::LeftParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RightParen);
                i += 1;
            }
            '[' => {
                tokens.push(Token::LeftBracket);
                i += 1;
            }
            ']' => {
                tokens.push(Token::RightBracket);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '"' => {
                let mut name = String::new();
                i += 1;
                loop {
                    let Some(&c) = chars.get(i) else {
                        return Err(Error::parser(format!(
                            "Type \"{text}\" has an unterminated quoted name"
                        )));
                    };
                    i += 1;
                    if c == '"' {
                        if chars.get(i) == Some(&'"') {
                            name.push('"');
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    name.push(c);
                }
                tokens.push(Token::Quoted(name));
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while chars.get(i).is_some_and(char::is_ascii_digit) {
                    i += 1;
                }
                let digits: String = chars[start..i].iter().collect();
                let number = digits.parse::<u32>().map_err(|_| {
                    Error::parser(format!("Type \"{text}\" has a number that is too large"))
                })?;
                tokens.push(Token::Number(number));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while chars.get(i).is_some_and(|&c| c.is_alphanumeric() || c == '_') {
                    i += 1;
                }
                tokens.push(Token::Word(chars[start..i].iter().collect()));
            }
            other => {
                return Err(Error::parser(format!(
                    "Type \"{text}\" has an unexpected character {other:?}"
                )));
            }
        }
    }
    Ok(tokens)
}

struct TypeParser<'a> {
    tokens: &'a [Token],
    position: usize,
}

impl TypeParser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == Some(token) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    /// Consumes `word` if it is next, case insensitively.
    fn eat_word(&mut self, word: &str) -> bool {
        match self.peek() {
            Some(Token::Word(found)) if found.eq_ignore_ascii_case(word) => {
                self.position += 1;
                true
            }
            _ => false,
        }
    }

    fn parse_type(&mut self) -> Result<LogicalType> {
        let mut ty = self.parse_base()?;
        // Suffixes bind left to right, so INTEGER[][3] is an array of three lists.
        loop {
            if !self.eat(&Token::LeftBracket) {
                break;
            }
            if let Some(&Token::Number(length)) = self.peek() {
                self.position += 1;
                expect(self.eat(&Token::RightBracket), "]")?;
                ty = LogicalType::array(ty, length);
            } else {
                expect(self.eat(&Token::RightBracket), "]")?;
                ty = LogicalType::list(ty);
            }
        }
        Ok(ty)
    }

    fn parse_base(&mut self) -> Result<LogicalType> {
        // A quoted name is accepted here because the null type prints as "NULL" with the quotes,
        // which is DuckDB's spelling and which has to read back.
        let word = match self.peek().cloned() {
            Some(Token::Word(word) | Token::Quoted(word)) => {
                self.position += 1;
                word
            }
            _ => return Err(Error::parser("Expected a type name".to_string())),
        };
        let upper = word.to_ascii_uppercase();

        match upper.as_str() {
            "STRUCT" | "ROW" => return self.parse_fields().map(LogicalType::Struct),
            "UNION" => return self.parse_fields().map(LogicalType::Union),
            "MAP" => {
                expect(self.eat(&Token::LeftParen), "(")?;
                let key = self.parse_type()?;
                expect(self.eat(&Token::Comma), ",")?;
                let value = self.parse_type()?;
                expect(self.eat(&Token::RightParen), ")")?;
                return Ok(LogicalType::map(key, value));
            }
            "DECIMAL" | "NUMERIC" | "DEC" => {
                if !self.eat(&Token::LeftParen) {
                    // Bare DECIMAL is DECIMAL(18, 3) in DuckDB, which is a surprising default and
                    // is nonetheless the one people's queries depend on.
                    return LogicalType::decimal(18, 3);
                }
                let width = self.parse_number()?;
                let scale = if self.eat(&Token::Comma) { self.parse_number()? } else { 0 };
                expect(self.eat(&Token::RightParen), ")")?;
                let narrow = |n: u32| u8::try_from(n).unwrap_or(u8::MAX);
                return LogicalType::decimal(narrow(width), narrow(scale));
            }
            // Multiword names. Each of these is a word that only means something with the words
            // after it, so the lookahead is checked before the alias table is consulted.
            "DOUBLE" => {
                self.eat_word("PRECISION");
                return Ok(LogicalType::Double);
            }
            "CHARACTER" => {
                self.eat_word("VARYING");
                self.eat_length_modifier()?;
                return Ok(LogicalType::Varchar);
            }
            "TIME" | "TIMESTAMP" => {
                let with_zone = self.eat_time_zone_suffix();
                return Ok(match (upper.as_str(), with_zone) {
                    ("TIME", false) => LogicalType::Time,
                    ("TIME", true) => LogicalType::TimeTz,
                    (_, false) => LogicalType::Timestamp,
                    (_, true) => LogicalType::TimestampTz,
                });
            }
            _ => {}
        }

        // A length modifier on a string type parses and is discarded, which is what DuckDB does:
        // VARCHAR(10) does not truncate and does not reject, it is VARCHAR.
        self.eat_length_modifier()?;
        alias(&upper).ok_or_else(|| Error::parser(format!("Unrecognized type name \"{word}\"")))
    }

    /// `WITH TIME ZONE` or `WITHOUT TIME ZONE`, returning whether the zone is carried.
    fn eat_time_zone_suffix(&mut self) -> bool {
        let start = self.position;
        let with = if self.eat_word("WITH") {
            true
        } else if self.eat_word("WITHOUT") {
            false
        } else {
            return false;
        };
        if self.eat_word("TIME") && self.eat_word("ZONE") {
            with
        } else {
            self.position = start;
            false
        }
    }

    fn eat_length_modifier(&mut self) -> Result<()> {
        if self.eat(&Token::LeftParen) {
            self.parse_number()?;
            expect(self.eat(&Token::RightParen), ")")?;
        }
        Ok(())
    }

    fn parse_fields(&mut self) -> Result<Vec<Field>> {
        expect(self.eat(&Token::LeftParen), "(")?;
        let mut fields = Vec::new();
        if self.eat(&Token::RightParen) {
            return Ok(fields);
        }
        loop {
            let name = match self.peek().cloned() {
                Some(Token::Word(name) | Token::Quoted(name)) => {
                    self.position += 1;
                    name
                }
                _ => return Err(Error::parser("Expected a field name".to_string())),
            };
            let ty = self.parse_type()?;
            fields.push(Field::new(name, ty));
            if self.eat(&Token::Comma) {
                continue;
            }
            expect(self.eat(&Token::RightParen), ")")?;
            return Ok(fields);
        }
    }

    fn parse_number(&mut self) -> Result<u32> {
        match self.peek() {
            Some(&Token::Number(n)) => {
                self.position += 1;
                Ok(n)
            }
            _ => Err(Error::parser("Expected a number".to_string())),
        }
    }
}

fn expect(matched: bool, what: &str) -> Result<()> {
    if matched { Ok(()) } else { Err(Error::parser(format!("Expected \"{what}\""))) }
}

/// The single word type names, aliases included.
///
/// The aliases are DuckDB's, and they are here rather than in the parser because `CREATE TABLE t
/// (a INT4)` and `CAST(x AS INT4)` have to agree and there is only one table.
fn alias(upper: &str) -> Option<LogicalType> {
    Some(match upper {
        "NULL" => LogicalType::Null,
        "BOOLEAN" | "BOOL" | "LOGICAL" => LogicalType::Boolean,
        "TINYINT" | "INT1" => LogicalType::TinyInt,
        "SMALLINT" | "INT2" | "SHORT" => LogicalType::SmallInt,
        "INTEGER" | "INT" | "INT4" | "SIGNED" => LogicalType::Integer,
        "BIGINT" | "INT8" | "LONG" => LogicalType::BigInt,
        "HUGEINT" | "INT128" => LogicalType::HugeInt,
        "UTINYINT" | "UINT1" => LogicalType::UTinyInt,
        "USMALLINT" | "UINT2" => LogicalType::USmallInt,
        "UINTEGER" | "UINT4" => LogicalType::UInteger,
        "UBIGINT" | "UINT8" => LogicalType::UBigInt,
        "UHUGEINT" | "UINT128" => LogicalType::UHugeInt,
        "FLOAT" | "FLOAT4" | "REAL" => LogicalType::Float,
        "FLOAT8" => LogicalType::Double,
        "VARCHAR" | "CHAR" | "BPCHAR" | "TEXT" | "STRING" => LogicalType::Varchar,
        "BLOB" | "BYTEA" | "BINARY" | "VARBINARY" => LogicalType::Blob,
        "BIT" | "BITSTRING" => LogicalType::Bit,
        "UUID" | "GUID" => LogicalType::Uuid,
        "DATE" => LogicalType::Date,
        "TIMETZ" => LogicalType::TimeTz,
        "DATETIME" => LogicalType::Timestamp,
        "TIMESTAMP_S" | "TIMESTAMP_SEC" | "TIMESTAMP_SECONDS" => LogicalType::TimestampS,
        "TIMESTAMP_MS" | "TIMESTAMP_MILLISECONDS" => LogicalType::TimestampMs,
        "TIMESTAMP_NS" | "TIMESTAMP_NANOSECONDS" => LogicalType::TimestampNs,
        "TIMESTAMPTZ" => LogicalType::TimestampTz,
        "INTERVAL" => LogicalType::Interval,
        _ => return None,
    })
}

#[cfg(test)]
mod promotion_tests {
    use super::LogicalType;

    #[test]
    fn a_type_promotes_with_itself_to_itself() {
        for ty in [
            LogicalType::Integer,
            LogicalType::Varchar,
            LogicalType::Boolean,
            LogicalType::Struct(vec![]),
        ] {
            assert_eq!(ty.promote(&ty), Some(ty.clone()), "{ty} does not promote with itself");
        }
    }

    #[test]
    fn null_takes_the_other_type() {
        assert_eq!(LogicalType::Null.promote(&LogicalType::Varchar), Some(LogicalType::Varchar));
        assert_eq!(LogicalType::Date.promote(&LogicalType::Null), Some(LogicalType::Date));
        assert_eq!(LogicalType::Null.promote(&LogicalType::Null), Some(LogicalType::Null));
    }

    #[test]
    fn the_wider_number_wins() {
        assert_eq!(
            LogicalType::Integer.promote(&LogicalType::SmallInt),
            Some(LogicalType::Integer)
        );
        assert_eq!(LogicalType::Integer.promote(&LogicalType::Double), Some(LogicalType::Double));
        assert_eq!(LogicalType::Float.promote(&LogicalType::Double), Some(LogicalType::Double));
    }

    /// The one that is worth a test of its own, because reinterpreting instead of widening here is
    /// a wrong answer off by four billion rather than a crash.
    #[test]
    fn signed_and_unsigned_widen_rather_than_reinterpret() {
        assert_eq!(LogicalType::Integer.promote(&LogicalType::UInteger), Some(LogicalType::BigInt));
        assert_eq!(
            LogicalType::TinyInt.promote(&LogicalType::UTinyInt),
            Some(LogicalType::SmallInt)
        );
        assert_eq!(LogicalType::BigInt.promote(&LogicalType::UBigInt), Some(LogicalType::HugeInt));
    }

    #[test]
    fn promotion_does_not_care_which_side_a_type_is_on() {
        let types = [
            LogicalType::TinyInt,
            LogicalType::UInteger,
            LogicalType::BigInt,
            LogicalType::Double,
            LogicalType::Decimal { width: 10, scale: 2 },
            LogicalType::Null,
            LogicalType::Varchar,
            LogicalType::Date,
            LogicalType::Timestamp,
        ];
        for left in &types {
            for right in &types {
                assert_eq!(
                    left.promote(right),
                    right.promote(left),
                    "{left} and {right} promote differently depending on the order"
                );
            }
        }
    }

    #[test]
    fn a_decimal_keeps_room_for_both_halves() {
        let left = LogicalType::Decimal { width: 5, scale: 4 };
        let right = LogicalType::Decimal { width: 5, scale: 1 };
        assert_eq!(left.promote(&right), Some(LogicalType::Decimal { width: 8, scale: 4 }));
    }

    #[test]
    fn an_integer_next_to_a_decimal_widens_the_decimal() {
        let decimal = LogicalType::Decimal { width: 5, scale: 2 };
        assert_eq!(
            decimal.promote(&LogicalType::Integer),
            Some(LogicalType::Decimal { width: 12, scale: 2 })
        );
    }

    /// A signed and an unsigned integer of the same width leave different room, at the top pair.
    ///
    /// 9,223,372,036,854,775,807 is nineteen digits and 18,446,744,073,709,551,615 is twenty, so
    /// the two do not promote with a decimal to the same type, and every narrower pair does.
    #[test]
    fn a_bigint_leaves_room_for_one_digit_fewer_than_a_ubigint() {
        let decimal = LogicalType::Decimal { width: 4, scale: 2 };
        assert_eq!(
            decimal.promote(&LogicalType::BigInt),
            Some(LogicalType::Decimal { width: 21, scale: 2 })
        );
        assert_eq!(
            decimal.promote(&LogicalType::UBigInt),
            Some(LogicalType::Decimal { width: 22, scale: 2 })
        );
        assert_eq!(decimal.promote(&LogicalType::Integer), decimal.promote(&LogicalType::UInteger));
    }

    #[test]
    fn a_date_and_a_timestamp_meet_at_the_timestamp() {
        assert_eq!(
            LogicalType::Date.promote(&LogicalType::Timestamp),
            Some(LogicalType::Timestamp)
        );
        assert_eq!(
            LogicalType::TimestampS.promote(&LogicalType::TimestampNs),
            Some(LogicalType::TimestampNs)
        );
    }

    /// An interval is a duration and a timestamp is a point, so there is no type that holds both
    /// and saying so is the binder's cue to raise instead of guessing.
    #[test]
    fn types_that_do_not_meet_say_so() {
        assert_eq!(LogicalType::Timestamp.promote(&LogicalType::Interval), None);
        assert_eq!(LogicalType::Integer.promote(&LogicalType::Varchar), None);
        assert_eq!(LogicalType::Boolean.promote(&LogicalType::Integer), None);
    }

    #[test]
    fn a_list_promotes_by_its_element() {
        let left = LogicalType::list(LogicalType::Integer);
        let right = LogicalType::list(LogicalType::BigInt);
        assert_eq!(left.promote(&right), Some(LogicalType::list(LogicalType::BigInt)));
        assert_eq!(left.promote(&LogicalType::list(LogicalType::Varchar)), None);
    }
}

#[cfg(test)]
mod tests {
    use super::{Field, LogicalType, PhysicalType};

    /// Every type this crate knows about, used by the round trip test and by anything else that
    /// wants to be exhaustive without listing them again.
    fn every_type() -> Vec<LogicalType> {
        vec![
            LogicalType::Null,
            LogicalType::Boolean,
            LogicalType::TinyInt,
            LogicalType::SmallInt,
            LogicalType::Integer,
            LogicalType::BigInt,
            LogicalType::HugeInt,
            LogicalType::UTinyInt,
            LogicalType::USmallInt,
            LogicalType::UInteger,
            LogicalType::UBigInt,
            LogicalType::UHugeInt,
            LogicalType::Float,
            LogicalType::Double,
            LogicalType::Decimal { width: 18, scale: 3 },
            LogicalType::Decimal { width: 38, scale: 0 },
            LogicalType::Varchar,
            LogicalType::Blob,
            LogicalType::Bit,
            LogicalType::Uuid,
            LogicalType::Date,
            LogicalType::Time,
            LogicalType::TimeTz,
            LogicalType::Timestamp,
            LogicalType::TimestampS,
            LogicalType::TimestampMs,
            LogicalType::TimestampNs,
            LogicalType::TimestampTz,
            LogicalType::Interval,
            LogicalType::list(LogicalType::Integer),
            LogicalType::list(LogicalType::list(LogicalType::Varchar)),
            LogicalType::array(LogicalType::Double, 3),
            LogicalType::map(LogicalType::Varchar, LogicalType::Integer),
            LogicalType::Struct(vec![
                Field::new("a", LogicalType::Integer),
                Field::new("b", LogicalType::list(LogicalType::Varchar)),
            ]),
            LogicalType::Union(vec![
                Field::new("num", LogicalType::Integer),
                Field::new("str", LogicalType::Varchar),
            ]),
        ]
    }

    #[test]
    fn every_type_survives_being_printed_and_read_back() {
        // The textual plan format in spec/04-architecture.md round trips, and a plan carries
        // types, so this is the bottom of that guarantee. Failing it means a plan that cannot be
        // reparsed, which is the whole reason the format exists.
        for ty in every_type() {
            let printed = ty.to_string();
            let parsed = LogicalType::parse(&printed)
                .unwrap_or_else(|e| panic!("{printed} did not parse back: {e}"));
            assert_eq!(parsed, ty, "{printed} parsed to something else");
        }
    }

    #[test]
    fn a_field_name_that_needs_quoting_gets_quoted() {
        let ty = LogicalType::Struct(vec![
            Field::new("plain", LogicalType::Integer),
            Field::new("has space", LogicalType::Integer),
            Field::new("has\"quote", LogicalType::Integer),
            Field::new("2leading", LogicalType::Integer),
        ]);
        assert_eq!(
            ty.to_string(),
            "STRUCT(plain INTEGER, \"has space\" INTEGER, \"has\"\"quote\" INTEGER, \
             \"2leading\" INTEGER)"
        );
        assert_eq!(LogicalType::parse(&ty.to_string()).unwrap(), ty);
    }

    #[test]
    fn the_duckdb_aliases_resolve() {
        let cases = [
            ("int4", LogicalType::Integer),
            ("INT", LogicalType::Integer),
            ("signed", LogicalType::Integer),
            ("int8", LogicalType::BigInt),
            ("float4", LogicalType::Float),
            ("float8", LogicalType::Double),
            ("double precision", LogicalType::Double),
            ("text", LogicalType::Varchar),
            ("varchar(10)", LogicalType::Varchar),
            ("character varying(255)", LogicalType::Varchar),
            ("bool", LogicalType::Boolean),
            ("datetime", LogicalType::Timestamp),
            ("numeric(9, 2)", LogicalType::Decimal { width: 9, scale: 2 }),
            ("decimal", LogicalType::Decimal { width: 18, scale: 3 }),
            ("timestamp without time zone", LogicalType::Timestamp),
            ("timestamp with time zone", LogicalType::TimestampTz),
            ("time with time zone", LogicalType::TimeTz),
        ];
        for (text, expected) in cases {
            assert_eq!(LogicalType::parse(text).unwrap(), expected, "{text}");
        }
    }

    #[test]
    fn list_and_array_suffixes_bind_left_to_right() {
        assert_eq!(
            LogicalType::parse("INTEGER[][3]").unwrap(),
            LogicalType::array(LogicalType::list(LogicalType::Integer), 3)
        );
        assert_eq!(
            LogicalType::parse("STRUCT(a INT)[]").unwrap(),
            LogicalType::list(LogicalType::Struct(vec![Field::new("a", LogicalType::Integer)]))
        );
    }

    #[test]
    fn a_decimal_is_stored_in_the_narrowest_integer_that_holds_it() {
        assert_eq!(LogicalType::decimal(4, 2).unwrap().physical(), PhysicalType::Int16);
        assert_eq!(LogicalType::decimal(9, 2).unwrap().physical(), PhysicalType::Int32);
        assert_eq!(LogicalType::decimal(18, 2).unwrap().physical(), PhysicalType::Int64);
        assert_eq!(LogicalType::decimal(38, 2).unwrap().physical(), PhysicalType::Int128);
    }

    /// The name a failed cast gives a type, which is the integer it is stored in. The decimal
    /// buckets are the same ones `physical` uses, written as a width rather than as a layout,
    /// which is why a DECIMAL(11,0) and a DECIMAL(18,8) are both DECIMAL(18).
    #[test]
    fn a_message_names_the_type_by_what_it_is_stored_in() {
        assert_eq!(LogicalType::Boolean.physical_name(), "BOOL");
        assert_eq!(LogicalType::TinyInt.physical_name(), "INT8");
        assert_eq!(LogicalType::Integer.physical_name(), "INT32");
        assert_eq!(LogicalType::UBigInt.physical_name(), "UINT64");
        assert_eq!(LogicalType::HugeInt.physical_name(), "INT128");
        assert_eq!(LogicalType::Float.physical_name(), "FLOAT");
        assert_eq!(LogicalType::Varchar.physical_name(), "VARCHAR");
        assert_eq!(LogicalType::Date.physical_name(), "DATE");
        assert_eq!(LogicalType::decimal(4, 2).unwrap().physical_name(), "DECIMAL(4)");
        assert_eq!(LogicalType::decimal(11, 0).unwrap().physical_name(), "DECIMAL(18)");
        assert_eq!(LogicalType::decimal(18, 8).unwrap().physical_name(), "DECIMAL(18)");
        assert_eq!(LogicalType::decimal(38, 2).unwrap().physical_name(), "DECIMAL(38)");
        assert_eq!(LogicalType::Integer.decimal_storage(), None);
    }

    #[test]
    fn a_decimal_outside_the_bounds_is_rejected_rather_than_clamped() {
        assert!(LogicalType::decimal(0, 0).is_err());
        assert!(LogicalType::decimal(39, 0).is_err());
        assert!(LogicalType::decimal(4, 5).is_err());
        assert!(LogicalType::parse("DECIMAL(39,0)").is_err());
    }

    #[test]
    fn a_date_and_an_integer_share_a_layout_and_not_a_meaning() {
        assert_eq!(LogicalType::Date.physical(), LogicalType::Integer.physical());
        assert_ne!(LogicalType::Date, LogicalType::Integer);
        assert!(LogicalType::Date.is_temporal());
        assert!(!LogicalType::Date.is_numeric());
    }

    #[test]
    fn nesting_reports_its_children_in_child_column_order() {
        let ty = LogicalType::map(LogicalType::Varchar, LogicalType::Integer);
        assert!(ty.is_nested());
        assert_eq!(ty.children(), vec![LogicalType::Varchar, LogicalType::Integer]);
        assert_eq!(LogicalType::Integer.children(), Vec::new());
    }

    #[test]
    fn text_that_is_not_a_type_is_rejected_with_the_word_that_broke_it() {
        let error = LogicalType::parse("INTEGRE").unwrap_err();
        assert!(error.message().contains("INTEGRE"), "{error}");
        assert!(LogicalType::parse("INTEGER JUNK").is_err());
        assert!(LogicalType::parse("STRUCT(a)").is_err());
        assert!(LogicalType::parse("").is_err());
    }
}
