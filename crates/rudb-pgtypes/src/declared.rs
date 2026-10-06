//! The PostgreSQL type that a column declaration or a cast writes, such as `varchar(10)`,
//! `timestamp(3) with time zone` or `numeric(10, 2)[]`, as an OID and a typmod.
//!
//! The grammar is the `Typename` rule of `src/backend/parser/gram.y`, and the typmods are the ones
//! that the `typmodin` function of each type gives, as document 06 section 6.9 of the PostgreSQL
//! notes has them. A name that is not a built-in type, such as a type that `CREATE TYPE` made or a
//! type that only rudb has, gives `None`, and the caller then uses the type that the logical type
//! maps to.

use rudb_common::{DeclaredType, LogicalType};

use crate::generated::oids as oid;
use crate::reg::RegKind;
use crate::types::{Oid, TypeInfo};
use crate::typmod::{
    INTERVAL_FULL_PRECISION, INTERVAL_FULL_RANGE, IntervalField, MAX_TIME_PRECISION, char_typmod,
    interval_range, interval_typmod, numeric_typmod,
};

/// `NUMERIC_MAX_PRECISION` and the limits of the scale, from `src/include/utils/numeric.h`.
const NUMERIC_MAX_PRECISION: i32 = 1000;
const NUMERIC_MIN_SCALE: i32 = -1000;
const NUMERIC_MAX_SCALE: i32 = 1000;

/// `MaxAttrSize` in characters for `varchar(n)` and `bpchar(n)`, and `VARBITMAXLEN` for `bit(n)`.
const MAX_LENGTH: i32 = 10 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A word with no quotes, in lower case.
    Word(String),
    /// A name in double quotes, with its case kept.
    Quoted(String),
    Number(i32),
    Open,
    Close,
    Comma,
    Dot,
    OpenBracket,
    CloseBracket,
}

fn lex(text: &str) -> Option<Vec<Token>> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match byte {
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' => at += 1,
            b'(' | b')' | b',' | b'.' | b'[' | b']' => {
                tokens.push(match byte {
                    b'(' => Token::Open,
                    b')' => Token::Close,
                    b',' => Token::Comma,
                    b'.' => Token::Dot,
                    b'[' => Token::OpenBracket,
                    _ => Token::CloseBracket,
                });
                at += 1;
            }
            b'"' => {
                let mut name = String::new();
                at += 1;
                loop {
                    let end = at + text[at..].find('"')?;
                    name.push_str(&text[at..end]);
                    at = end + 1;
                    if bytes.get(at) == Some(&b'"') {
                        name.push('"');
                        at += 1;
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Quoted(name));
            }
            b'-' | b'+' | b'0'..=b'9' => {
                let start = at;
                at += 1;
                while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                tokens.push(Token::Number(text[start..at].parse().ok()?));
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | 0x80.. => {
                let start = at;
                while bytes.get(at).is_some_and(|&b| {
                    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
                }) {
                    at += 1;
                }
                tokens.push(Token::Word(text[start..at].to_ascii_lowercase()));
            }
            _ => return None,
        }
    }
    Some(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == Some(token) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Some(Token::Word(held)) if held == word) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    /// The numbers in brackets after a type name, or an empty list when there are no brackets.
    fn modifiers(&mut self) -> Option<Vec<i32>> {
        let mut numbers = Vec::new();
        if !self.eat(&Token::Open) {
            return Some(numbers);
        }
        loop {
            let Some(&Token::Number(n)) = self.peek() else {
                return None;
            };
            self.at += 1;
            numbers.push(n);
            if self.eat(&Token::Close) {
                return Some(numbers);
            }
            if !self.eat(&Token::Comma) {
                return None;
            }
        }
    }

    /// The precision in brackets, if there is one.
    fn precision(&mut self) -> Option<Option<i32>> {
        match self.modifiers()?[..] {
            [] => Some(None),
            [p] => Some(Some(p)),
            _ => None,
        }
    }

    /// `WITH TIME ZONE` or `WITHOUT TIME ZONE`, and whether the zone is there.
    fn zone(&mut self) -> Option<bool> {
        let with = if self.eat_word("with") {
            true
        } else if self.eat_word("without") {
            false
        } else {
            return Some(false);
        };
        (self.eat_word("time") && self.eat_word("zone")).then_some(with)
    }

    /// The fields of `interval year to month` and the like, as a range mask, or `None` when no
    /// field is written.
    fn interval_fields(&mut self) -> Option<Option<i32>> {
        use IntervalField as F;
        let field = |parser: &mut Parser| {
            let found = match parser.peek() {
                Some(Token::Word(word)) => match word.as_str() {
                    "year" => F::Year,
                    "month" => F::Month,
                    "day" => F::Day,
                    "hour" => F::Hour,
                    "minute" => F::Minute,
                    "second" => F::Second,
                    _ => return None,
                },
                _ => return None,
            };
            parser.at += 1;
            Some(found)
        };
        let Some(first) = field(self) else {
            return Some(None);
        };
        if !self.eat_word("to") {
            return Some(Some(interval_range(&[first])));
        }
        let last = field(self)?;
        let fields: &[F] = match (first, last) {
            (F::Year, F::Month) => &[F::Year, F::Month],
            (F::Day, F::Hour) => &[F::Day, F::Hour],
            (F::Day, F::Minute) => &[F::Day, F::Hour, F::Minute],
            (F::Day, F::Second) => &[F::Day, F::Hour, F::Minute, F::Second],
            (F::Hour, F::Minute) => &[F::Hour, F::Minute],
            (F::Hour, F::Second) => &[F::Hour, F::Minute, F::Second],
            (F::Minute, F::Second) => &[F::Minute, F::Second],
            _ => return None,
        };
        Some(Some(interval_range(fields)))
    }
}

/// The rudb type of a PostgreSQL type that DuckDB reads as a different type or does not know.
/// DuckDB reads `oid` as `BIGINT` and `"char"` as `VARCHAR`. A PostgreSQL session keeps them as
/// `UINTEGER` and `UTINYINT`, which have the values and the width of the PostgreSQL types. The OID
/// alias types such as `regtype` are `UINTEGER` too, and `int2vector` and `oidvector` are lists of
/// `SMALLINT` and of `UINTEGER`. A `numeric` with no precision is the `numeric` of PostgreSQL. The
/// same is true for an array of them.
pub fn session_type(declared: DeclaredType) -> Option<LogicalType> {
    let unbounded = declared.typmod < 0;
    let of = |oid| match oid {
        oid::NUMERIC if unbounded => Some(LogicalType::Numeric),
        oid::OID => Some(LogicalType::UInteger),
        oid::CHAR => Some(LogicalType::UTinyInt),
        oid::INT2VECTOR => Some(LogicalType::List(Box::new(LogicalType::SmallInt))),
        oid::OIDVECTOR => Some(LogicalType::List(Box::new(LogicalType::UInteger))),
        oid if RegKind::from_oid(oid).is_some() => Some(LogicalType::UInteger),
        _ => None,
    };
    of(declared.oid).or_else(|| {
        let info = TypeInfo::get(declared.oid).filter(|info| info.is_array())?;
        Some(LogicalType::List(Box::new(of(info.elem)?)))
    })
}

/// The OID and the typmod of a written type, or `None` when the text is not a built-in PostgreSQL
/// type that this knows.
pub fn declared_type(text: &str) -> Option<DeclaredType> {
    let mut parser = Parser { tokens: lex(text)?, at: 0 };
    let (oid, typmod) = base(&mut parser)?;
    let mut array = false;
    loop {
        if parser.eat(&Token::OpenBracket) {
            if matches!(parser.peek(), Some(Token::Number(_))) {
                parser.at += 1;
            }
            if !parser.eat(&Token::CloseBracket) {
                return None;
            }
            array = true;
        } else if parser.eat_word("array") {
            if parser.eat(&Token::OpenBracket) {
                if !matches!(parser.peek(), Some(Token::Number(_))) {
                    return None;
                }
                parser.at += 1;
                if !parser.eat(&Token::CloseBracket) {
                    return None;
                }
            }
            array = true;
        } else {
            break;
        }
    }
    if parser.at != parser.tokens.len() {
        return None;
    }
    // An array keeps the typmod of its element, so `varchar(10)[]` has the typmod 14.
    let oid = if array { TypeInfo::get(oid)?.array } else { oid };
    (oid != 0).then_some(DeclaredType { oid, typmod })
}

/// The element type, before any array brackets.
fn base(parser: &mut Parser) -> Option<(Oid, i32)> {
    let first = parser.peek()?.clone();
    parser.at += 1;
    if parser.eat(&Token::Dot) {
        // A qualified name is a name in `pg_type`, so `pg_catalog.varchar(10)` is the type
        // `varchar` and `pg_catalog.char` is the one byte `"char"`.
        let schema = match first {
            Token::Word(word) | Token::Quoted(word) => word,
            _ => return None,
        };
        let name = match parser.peek()?.clone() {
            Token::Word(word) | Token::Quoted(word) => word,
            _ => return None,
        };
        parser.at += 1;
        if schema != "pg_catalog" {
            return None;
        }
        return named(parser, &name);
    }
    let word = match first {
        Token::Word(word) => word,
        Token::Quoted(name) => return named(parser, &name),
        _ => return None,
    };
    let time = |parser: &mut Parser, plain: Oid, zoned: Oid| {
        let p = parser.precision()?;
        let oid = if parser.zone()? { zoned } else { plain };
        Some((oid, time_typmod(p)?))
    };
    match word.as_str() {
        "double" if parser.eat_word("precision") => Some((oid::FLOAT8, -1)),
        "float" => match parser.precision()? {
            None => Some((oid::FLOAT8, -1)),
            Some(1..=24) => Some((oid::FLOAT4, -1)),
            Some(25..=53) => Some((oid::FLOAT8, -1)),
            Some(_) => None,
        },
        "real" => Some((oid::FLOAT4, -1)),
        "int" | "integer" => Some((oid::INT4, -1)),
        "smallint" => Some((oid::INT2, -1)),
        "bigint" => Some((oid::INT8, -1)),
        "boolean" => Some((oid::BOOL, -1)),
        "decimal" | "dec" => named(parser, "numeric"),
        "character" | "char" | "nchar" | "national" => {
            if word == "national" && !(parser.eat_word("character") || parser.eat_word("char")) {
                return None;
            }
            if parser.eat_word("varying") {
                return named(parser, "varchar");
            }
            // `char` with no length is `char(1)`, which is not the same as `bpchar`.
            match parser.precision()? {
                None => Some((oid::BPCHAR, char_typmod(1))),
                Some(n) => Some((oid::BPCHAR, length_typmod(n)?)),
            }
        }
        "bit" => {
            if parser.eat_word("varying") {
                return named(parser, "varbit");
            }
            match parser.precision()? {
                None => Some((oid::BIT, 1)),
                Some(n) => Some((oid::BIT, bit_typmod(n)?)),
            }
        }
        "time" => time(parser, oid::TIME, oid::TIMETZ),
        "timestamp" => time(parser, oid::TIMESTAMP, oid::TIMESTAMPTZ),
        "interval" => {
            // The precision comes after the name or after the fields, and only `SECOND` takes it
            // as the last field.
            let mut p = parser.precision()?;
            let range = parser.interval_fields()?;
            if p.is_none() && range.is_some() {
                p = parser.precision()?;
            }
            let typmod = match (range, p) {
                (None, None) => -1,
                (range, p) => interval_typmod(
                    p.map_or(Some(INTERVAL_FULL_PRECISION), clamp_time)?,
                    range.unwrap_or(INTERVAL_FULL_RANGE),
                ),
            };
            Some((oid::INTERVAL, typmod))
        }
        name => named(parser, name),
    }
}

/// A type by its name in `pg_type`, with the modifiers its `typmodin` takes.
fn named(parser: &mut Parser, name: &str) -> Option<(Oid, i32)> {
    let info = TypeInfo::by_name(name)?;
    // An array name such as `_int4` or a pseudo-type is not a column type that this reads.
    if info.kind != b'b' || name.starts_with('_') {
        return None;
    }
    let modifiers = parser.modifiers()?;
    let typmod = match (info.oid, &modifiers[..]) {
        (_, []) => -1,
        (oid::VARCHAR | oid::BPCHAR, &[n]) => length_typmod(n)?,
        (oid::NUMERIC, &[p]) => numeric(p, 0)?,
        (oid::NUMERIC, &[p, s]) => numeric(p, s)?,
        (oid::TIME | oid::TIMETZ | oid::TIMESTAMP | oid::TIMESTAMPTZ, &[p]) => {
            time_typmod(Some(p))?
        }
        (oid::INTERVAL, &[p]) => interval_typmod(clamp_time(p)?, INTERVAL_FULL_RANGE),
        (oid::BIT | oid::VARBIT, &[n]) => bit_typmod(n)?,
        _ => return None,
    };
    // The time types take `with time zone` after the name in `pg_type` too.
    let oid = match info.oid {
        oid::TIME | oid::TIMESTAMP if name == "time" || name == "timestamp" => {
            if parser.zone()? {
                if info.oid == oid::TIME { oid::TIMETZ } else { oid::TIMESTAMPTZ }
            } else {
                info.oid
            }
        }
        oid => oid,
    };
    Some((oid, typmod))
}

fn length_typmod(n: i32) -> Option<i32> {
    (1..=MAX_LENGTH).contains(&n).then(|| char_typmod(n))
}

fn bit_typmod(n: i32) -> Option<i32> {
    (1..=MAX_LENGTH).contains(&n).then_some(n)
}

fn numeric(p: i32, s: i32) -> Option<i32> {
    let fits = (1..=NUMERIC_MAX_PRECISION).contains(&p)
        && (NUMERIC_MIN_SCALE..=NUMERIC_MAX_SCALE).contains(&s);
    fits.then(|| numeric_typmod(p, s))
}

/// A precision above six becomes six, as PostgreSQL does with a warning.
fn clamp_time(p: i32) -> Option<i32> {
    (p >= 0).then(|| p.min(MAX_TIME_PRECISION))
}

fn time_typmod(p: Option<i32>) -> Option<i32> {
    match p {
        None => Some(-1),
        Some(p) => clamp_time(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_written_types_give_the_typmods_of_postgresql() {
        let cases: &[(&str, Oid, i32)] = &[
            ("int", oid::INT4, -1),
            ("INTEGER", oid::INT4, -1),
            ("int8", oid::INT8, -1),
            ("varchar(10)", oid::VARCHAR, 14),
            ("character varying(255)", oid::VARCHAR, 259),
            ("varchar", oid::VARCHAR, -1),
            ("char(3)", oid::BPCHAR, 7),
            ("char", oid::BPCHAR, 5),
            ("bpchar", oid::BPCHAR, -1),
            ("\"char\"", oid::CHAR, -1),
            ("pg_catalog.char", oid::CHAR, -1),
            ("numeric(10,2)", oid::NUMERIC, 655_366),
            ("numeric(4, 2)", oid::NUMERIC, 262_150),
            ("decimal(5)", oid::NUMERIC, 327_684),
            ("numeric", oid::NUMERIC, -1),
            ("timestamp(3)", oid::TIMESTAMP, 3),
            ("timestamp(1) without time zone", oid::TIMESTAMP, 1),
            ("timestamptz(0)", oid::TIMESTAMPTZ, 0),
            ("timestamp with time zone", oid::TIMESTAMPTZ, -1),
            ("time(2)", oid::TIME, 2),
            ("time with time zone", oid::TIMETZ, -1),
            ("timestamp(9)", oid::TIMESTAMP, 6),
            ("interval", oid::INTERVAL, -1),
            ("interval(0)", oid::INTERVAL, 2_147_418_112),
            ("interval year to month", oid::INTERVAL, interval_typmod(0xffff, 6)),
            ("interval day to second(3)", oid::INTERVAL, interval_typmod(3, 0x1c08)),
            ("bit(5)", oid::BIT, 5),
            ("bit", oid::BIT, 1),
            ("bit varying(7)", oid::VARBIT, 7),
            ("varbit", oid::VARBIT, -1),
            ("double precision", oid::FLOAT8, -1),
            ("float(10)", oid::FLOAT4, -1),
            ("float", oid::FLOAT8, -1),
            ("text", oid::TEXT, -1),
            ("name", oid::NAME, -1),
            ("int[]", oid::INT4_ARRAY, -1),
            ("varchar(10)[]", oid::VARCHAR_ARRAY, 14),
            ("integer array[3]", oid::INT4_ARRAY, -1),
        ];
        for &(text, oid, typmod) in cases {
            assert_eq!(declared_type(text), Some(DeclaredType { oid, typmod }), "{text}");
        }
    }

    #[test]
    fn other_names_give_nothing() {
        for text in
            ["hugeint", "blob", "my_type", "public.t", "varchar(0)", "int(3)", "_int4", "x y"]
        {
            assert_eq!(declared_type(text), None, "{text}");
        }
    }
}
