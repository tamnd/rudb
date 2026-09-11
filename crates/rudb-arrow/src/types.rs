//! Arrow types, and the format strings the C data interface names them with.

use rudb_common::{Error, LogicalType, Result};

/// An Arrow type.
///
/// The subset our own types map onto, which is every type the engine can produce a value of today.
/// The nested types are missing for the same reason `rudb-vector` has no nested vector: a list is
/// offsets plus a child array, and there is no child array until the storage layer has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataType {
    /// No values, all null, no buffers at all.
    Null,
    /// One bit per value.
    Boolean,
    /// 8 bit signed.
    Int8,
    /// 16 bit signed.
    Int16,
    /// 32 bit signed.
    Int32,
    /// 64 bit signed.
    Int64,
    /// 8 bit unsigned.
    UInt8,
    /// 16 bit unsigned.
    UInt16,
    /// 32 bit unsigned.
    UInt32,
    /// 64 bit unsigned.
    UInt64,
    /// IEEE 754 binary32.
    Float32,
    /// IEEE 754 binary64.
    Float64,
    /// UTF-8, with 32 bit offsets.
    Utf8,
    /// Bytes, with 32 bit offsets.
    Binary,
    /// Days since 1970-01-01, 32 bit.
    Date32,
    /// Microseconds since midnight, 64 bit.
    Time64,
    /// Microseconds since the epoch, 64 bit, with a time zone when there is one.
    Timestamp(TimeUnit, Option<String>),
    /// Months, days and nanoseconds, sixteen bytes.
    Interval,
    /// A 128 bit integer with a decimal point in it.
    Decimal128 {
        /// How many digits it can hold.
        precision: u8,
        /// How many of them are after the point.
        scale: u8,
    },
}

/// How finely a timestamp counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeUnit {
    /// Seconds.
    Second,
    /// Milliseconds.
    Millisecond,
    /// Microseconds, which is what our own `TIMESTAMP` is.
    Microsecond,
    /// Nanoseconds.
    Nanosecond,
}

impl TimeUnit {
    /// The letter the format string uses.
    fn letter(self) -> char {
        match self {
            Self::Second => 's',
            Self::Millisecond => 'm',
            Self::Microsecond => 'u',
            Self::Nanosecond => 'n',
        }
    }
}

impl DataType {
    /// The Arrow type one of ours becomes.
    ///
    /// `HUGEINT` becomes `DECIMAL128(38, 0)`, which is what DuckDB exports it as and the only thing
    /// it can be: Arrow has no 128 bit integer and a decimal with no fractional digits is one.
    ///
    /// # Errors
    ///
    /// For a type with no Arrow counterpart yet, which is the nested types, `UHUGEINT`, `BIT` and
    /// `UUID`.
    pub fn of(ty: &LogicalType) -> Result<Self> {
        Ok(match ty {
            LogicalType::Null => Self::Null,
            LogicalType::Boolean => Self::Boolean,
            LogicalType::TinyInt => Self::Int8,
            LogicalType::SmallInt => Self::Int16,
            LogicalType::Integer => Self::Int32,
            LogicalType::BigInt => Self::Int64,
            LogicalType::HugeInt => Self::Decimal128 { precision: 38, scale: 0 },
            LogicalType::UTinyInt => Self::UInt8,
            LogicalType::USmallInt => Self::UInt16,
            LogicalType::UInteger => Self::UInt32,
            LogicalType::UBigInt => Self::UInt64,
            LogicalType::Float => Self::Float32,
            LogicalType::Double => Self::Float64,
            LogicalType::Decimal { width, scale } => {
                Self::Decimal128 { precision: *width, scale: *scale }
            }
            LogicalType::Varchar => Self::Utf8,
            LogicalType::Blob => Self::Binary,
            LogicalType::Date => Self::Date32,
            LogicalType::Time => Self::Time64,
            LogicalType::Timestamp => Self::Timestamp(TimeUnit::Microsecond, None),
            LogicalType::TimestampS => Self::Timestamp(TimeUnit::Second, None),
            LogicalType::TimestampMs => Self::Timestamp(TimeUnit::Millisecond, None),
            LogicalType::TimestampNs => Self::Timestamp(TimeUnit::Nanosecond, None),
            LogicalType::TimestampTz => {
                Self::Timestamp(TimeUnit::Microsecond, Some("UTC".to_string()))
            }
            LogicalType::Interval => Self::Interval,
            other => {
                return Err(Error::not_implemented(format!("exporting {other} to Arrow")));
            }
        })
    }

    /// The format string the Arrow C data interface names this type with.
    ///
    /// Written now, before there is an FFI boundary to hand it across, because it is the part of
    /// the mapping that is defined by somebody else's document and the part a test can check
    /// against that document. The export itself is then a struct with this string in it.
    #[must_use]
    pub fn format(&self) -> String {
        match self {
            Self::Null => "n".to_string(),
            Self::Boolean => "b".to_string(),
            Self::Int8 => "c".to_string(),
            Self::Int16 => "s".to_string(),
            Self::Int32 => "i".to_string(),
            Self::Int64 => "l".to_string(),
            Self::UInt8 => "C".to_string(),
            Self::UInt16 => "S".to_string(),
            Self::UInt32 => "I".to_string(),
            Self::UInt64 => "L".to_string(),
            Self::Float32 => "f".to_string(),
            Self::Float64 => "g".to_string(),
            Self::Utf8 => "u".to_string(),
            Self::Binary => "z".to_string(),
            Self::Date32 => "tdD".to_string(),
            Self::Time64 => "ttu".to_string(),
            Self::Timestamp(unit, zone) => {
                format!("ts{}:{}", unit.letter(), zone.clone().unwrap_or_default())
            }
            Self::Interval => "tin".to_string(),
            Self::Decimal128 { precision, scale } => format!("d:{precision},{scale}"),
        }
    }

    /// How many buffers an array of this type has, which the C data interface also has to say.
    ///
    /// Two for anything fixed width, which is the validity bitmap and the values. Three for a
    /// variable width type, which puts the offsets in between. None at all for the null type, which
    /// has no values to be valid or invalid.
    #[must_use]
    pub fn buffer_count(&self) -> usize {
        match self {
            Self::Null => 0,
            Self::Utf8 | Self::Binary => 3,
            _ => 2,
        }
    }

    /// How wide one value is, for the fixed width types.
    #[must_use]
    pub fn width(&self) -> Option<usize> {
        Some(match self {
            Self::Null | Self::Utf8 | Self::Binary => return None,
            // A boolean is a bit rather than a byte, and the caller that asks this is asking about
            // bytes, so it is not a fixed width type for this purpose either.
            Self::Boolean => return None,
            Self::Int8 | Self::UInt8 => 1,
            Self::Int16 | Self::UInt16 => 2,
            Self::Int32 | Self::UInt32 | Self::Float32 | Self::Date32 => 4,
            Self::Int64 | Self::UInt64 | Self::Float64 | Self::Time64 | Self::Timestamp(_, _) => 8,
            Self::Interval | Self::Decimal128 { .. } => 16,
        })
    }
}

/// One column's name and type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// What the column is called.
    pub name: String,
    /// What it holds.
    pub data_type: DataType,
    /// Whether it may hold nulls. Everything a query produces may, so this is true unless somebody
    /// building a schema by hand says otherwise.
    pub nullable: bool,
}

impl Field {
    /// A nullable field, which is what a query result column is.
    #[must_use]
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self { name: name.into(), data_type, nullable: true }
    }
}

/// The columns of a record batch, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schema {
    /// The fields, left to right.
    pub fields: Vec<Field>,
}

impl Schema {
    /// A schema of these fields.
    #[must_use]
    pub fn new(fields: Vec<Field>) -> Self {
        Self { fields }
    }

    /// How many columns.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether there are no columns.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}
