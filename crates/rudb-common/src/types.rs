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

/// A named field of a `STRUCT` or a `UNION`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Field {
    /// The field name, unquoted and case sensitive as stored.
    pub name: String,
    /// The field type.
    pub ty: LogicalType,
}

impl Field {
    /// A field with a name and a type.
    pub fn new(name: impl Into<String>, ty: LogicalType) -> Self {
        Self { name: name.into(), ty }
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
