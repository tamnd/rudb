//! The PostgreSQL type that a column declaration or a cast writes, such as `varchar(10)`,
//! `timestamp(3) with time zone` or `numeric(10, 2)[]`, as an OID and a typmod.
//!
//! The grammar is the `Typename` rule of `src/backend/parser/gram.y`, and the typmods are the ones
//! that the `typmodin` function of each type gives, as document 06 section 6.9 of the PostgreSQL
//! notes has them. A name that is not a built-in type, such as a type that `CREATE TYPE` made or a
//! type that only rudb has, gives `None`, and the caller then uses the type that the logical type
//! maps to.

use std::ops::Range;

use rudb_common::{DeclaredType, LogicalType, SqlState};

use crate::error::TypeError;
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

/// `MaxAttrSize` in characters for `varchar(n)` and `bpchar(n)`.
const MAX_LENGTH: i32 = 10 * 1024 * 1024;

/// `VARBITMAXLEN` for `bit(n)` and `varbit(n)`.
const MAX_BITS: i32 = i32::MAX - 7;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A word with no quotes, in lower case.
    Word(String),
    /// A name in double quotes, with its case kept.
    Quoted(String),
    Number(i32),
    /// A string in single quotes, which only the type modifiers of a type that is not built in
    /// can have.
    Literal,
    Open,
    Close,
    Comma,
    Dot,
    OpenBracket,
    CloseBracket,
}

fn lex(text: &str) -> Option<Vec<Token>> {
    lex_spanned(text).ok().map(|tokens| tokens.into_iter().map(|(token, _)| token).collect())
}

/// Where the lexer stopped: the byte where a token starts that is not one of the tokens of a type
/// name, and whether it is a quoted name with no closing quote.
struct LexError {
    at: usize,
    unterminated: bool,
}

/// The tokens of a type name, each with the bytes of the text it was read from.
fn lex_spanned(text: &str) -> Result<Vec<(Token, Range<usize>)>, LexError> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        let start = at;
        let token = match byte {
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' => {
                at += 1;
                continue;
            }
            b'(' | b')' | b',' | b'.' | b'[' | b']' => {
                at += 1;
                match byte {
                    b'(' => Token::Open,
                    b')' => Token::Close,
                    b',' => Token::Comma,
                    b'.' => Token::Dot,
                    b'[' => Token::OpenBracket,
                    _ => Token::CloseBracket,
                }
            }
            b'"' => {
                let mut name = String::new();
                at += 1;
                loop {
                    let Some(len) = text[at..].find('"') else {
                        return Err(LexError { at: start, unterminated: true });
                    };
                    name.push_str(&text[at..at + len]);
                    at += len + 1;
                    if bytes.get(at) == Some(&b'"') {
                        name.push('"');
                        at += 1;
                    } else {
                        break;
                    }
                }
                Token::Quoted(name)
            }
            b'\'' => {
                at += 1;
                loop {
                    let Some(len) = text[at..].find('\'') else {
                        return Err(LexError { at: start, unterminated: false });
                    };
                    at += len + 1;
                    if bytes.get(at) == Some(&b'\'') {
                        at += 1;
                    } else {
                        break;
                    }
                }
                Token::Literal
            }
            b'-' | b'+' | b'0'..=b'9' => {
                at += 1;
                while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                let number = text[start..at].parse();
                Token::Number(number.map_err(|_| LexError { at: start, unterminated: false })?)
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | 0x80.. => {
                while bytes.get(at).is_some_and(|&b| {
                    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
                }) {
                    at += 1;
                }
                Token::Word(text[start..at].to_ascii_lowercase())
            }
            _ => return Err(LexError { at: start, unterminated: false }),
        };
        tokens.push((token, start..at));
    }
    Ok(tokens)
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
    (1..=MAX_BITS).contains(&n).then_some(n)
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

/// The words that `gram.y` reserves, and the words that name a column but not a type, which
/// cannot start the name of a type.
const NOT_TYPE_NAMES: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "between",
    "both",
    "case",
    "cast",
    "check",
    "coalesce",
    "collate",
    "column",
    "constraint",
    "create",
    "current_catalog",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "exists",
    "extract",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "grant",
    "greatest",
    "group",
    "grouping",
    "having",
    "in",
    "initially",
    "inout",
    "intersect",
    "into",
    "json_array",
    "json_arrayagg",
    "json_exists",
    "json_object",
    "json_objectagg",
    "json_query",
    "json_scalar",
    "json_serialize",
    "json_table",
    "json_value",
    "lateral",
    "leading",
    "least",
    "limit",
    "localtime",
    "localtimestamp",
    "merge_action",
    "none",
    "normalize",
    "not",
    "null",
    "nullif",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "out",
    "overlay",
    "placing",
    "position",
    "precision",
    "primary",
    "references",
    "returning",
    "row",
    "select",
    "session_user",
    "setof",
    "some",
    "substring",
    "symmetric",
    "system_user",
    "table",
    "then",
    "to",
    "trailing",
    "treat",
    "trim",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "values",
    "variadic",
    "when",
    "where",
    "window",
    "with",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
];

/// The type that the text of a type name names, as `parseTypeString` reads it for
/// `pg_input_is_valid` and for the input of `regtype`. A built-in type is what [`declared_type`]
/// gives, and an array name such as `_int4` and a pseudo-type such as `void` are found too.
///
/// # Errors
///
/// The error of PostgreSQL for a name that is not a type: `42601` for text that is not a type
/// name, `42704` for a name that no type has, `22023` for a type modifier out of range, and
/// `42601` for a type modifier on a type that takes none.
pub fn type_name(text: &str) -> Result<DeclaredType, TypeError> {
    if let Some(declared) = declared_type(text) {
        return Ok(declared);
    }
    let context = || Some(format!("invalid type name \"{text}\""));
    let syntax = |near: Option<&str>| TypeError {
        context: context(),
        ..TypeError::new(
            SqlState::SYNTAX_ERROR,
            near.map_or_else(
                || "syntax error at end of input".to_owned(),
                |near| format!("syntax error at or near \"{near}\""),
            ),
        )
    };
    let tokens = match lex_spanned(text) {
        Ok(tokens) => tokens,
        Err(LexError { at, unterminated: true }) => {
            return Err(TypeError {
                context: context(),
                ..TypeError::new(
                    SqlState::SYNTAX_ERROR,
                    format!("unterminated quoted identifier at or near \"{}\"", &text[at..]),
                )
            });
        }
        Err(LexError { at, .. }) => {
            let end = text[at..].chars().next().map_or(at, |c| at + c.len_utf8());
            return Err(syntax(Some(&text[at..end])));
        }
    };
    if tokens.is_empty() || matches!(&tokens[0].0, Token::Word(word) if word == "setof") {
        return Err(TypeError::new(
            SqlState::SYNTAX_ERROR,
            format!("invalid type name \"{text}\""),
        ));
    }
    let mut reader = NameReader { text, tokens: &tokens, at: 0 };
    let written = reader.read().map_err(|near| syntax(near.map(|near| &text[near])))?;
    written.resolve()
}

/// A type name as the grammar reads it, before the name is looked up.
struct WrittenType {
    /// The names, with a schema first when there is one. A type that the grammar spells in words,
    /// such as `character varying`, has its name in `pg_type` here.
    names: Vec<String>,
    /// Whether the grammar spells the type in words.
    keyword: bool,
    /// The type modifiers, with `None` for one that is not a number.
    modifiers: Vec<Option<i32>>,
    array: bool,
}

/// Reads a type name from its tokens. An error is the bytes of the token where the grammar stops,
/// or `None` at the end of the text.
struct NameReader<'a> {
    text: &'a str,
    tokens: &'a [(Token, Range<usize>)],
    at: usize,
}

impl NameReader<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at).map(|(token, _)| token)
    }

    fn here(&self) -> Option<Range<usize>> {
        self.tokens.get(self.at).map(|(_, span)| span.clone())
    }

    fn eat(&mut self, token: &Token) -> bool {
        let found = self.peek() == Some(token);
        self.at += usize::from(found);
        found
    }

    fn eat_word(&mut self, word: &str) -> bool {
        let found = matches!(self.peek(), Some(Token::Word(held)) if held == word);
        self.at += usize::from(found);
        found
    }

    fn expect(&mut self, token: &Token) -> Result<(), Option<Range<usize>>> {
        if self.eat(token) { Ok(()) } else { Err(self.here()) }
    }

    /// An integer with no sign, the `Iconst` of the grammar. A sign is an error at the sign.
    fn iconst(&mut self) -> Result<i32, Option<Range<usize>>> {
        match (self.peek(), self.here()) {
            (Some(&Token::Number(n)), Some(span)) => {
                if matches!(self.text.as_bytes()[span.start], b'-' | b'+') {
                    return Err(Some(span.start..span.start + 1));
                }
                self.at += 1;
                Ok(n)
            }
            (_, span) => Err(span),
        }
    }

    /// `'(' Iconst ')'` when the next token is `(`.
    fn precision(&mut self) -> Result<Vec<Option<i32>>, Option<Range<usize>>> {
        if !self.eat(&Token::Open) {
            return Ok(Vec::new());
        }
        let n = self.iconst()?;
        self.expect(&Token::Close)?;
        Ok(vec![Some(n)])
    }

    /// One expression of a list of type modifiers. A number can have a sign, and a name or a
    /// string is kept as `None`.
    fn signed(&mut self) -> Result<Option<i32>, Option<Range<usize>>> {
        match self.peek() {
            Some(&Token::Number(n)) => {
                self.at += 1;
                Ok(Some(n))
            }
            Some(Token::Word(_) | Token::Quoted(_) | Token::Literal) => {
                self.at += 1;
                Ok(None)
            }
            _ => Err(self.here()),
        }
    }

    /// `'(' expr_list ')'` when the next token is `(`.
    fn modifiers(&mut self) -> Result<Vec<Option<i32>>, Option<Range<usize>>> {
        let mut modifiers = Vec::new();
        if !self.eat(&Token::Open) {
            return Ok(modifiers);
        }
        loop {
            modifiers.push(self.signed()?);
            if self.eat(&Token::Close) {
                return Ok(modifiers);
            }
            self.expect(&Token::Comma)?;
        }
    }

    fn read(&mut self) -> Result<WrittenType, Option<Range<usize>>> {
        let Some(first) = self.peek().cloned() else { return Err(None) };
        let start = self.here();
        self.at += 1;
        let keyword = |name: &str, modifiers| WrittenType {
            names: vec![name.to_owned()],
            keyword: true,
            modifiers,
            array: false,
        };
        let mut written = match &first {
            Token::Word(word) => match word.as_str() {
                "int" | "integer" => keyword("int4", Vec::new()),
                "smallint" => keyword("int2", Vec::new()),
                "bigint" => keyword("int8", Vec::new()),
                "real" => keyword("float4", Vec::new()),
                "boolean" => keyword("bool", Vec::new()),
                "json" => keyword("json", Vec::new()),
                "double" => {
                    if !self.eat_word("precision") {
                        return Err(self.here());
                    }
                    keyword("float8", Vec::new())
                }
                "float" => keyword("float", self.precision()?),
                "decimal" | "dec" | "numeric" => keyword("numeric", self.modifiers()?),
                "character" | "char" | "nchar" | "national" | "varchar" => {
                    if word == "national" && !(self.eat_word("character") || self.eat_word("char"))
                    {
                        return Err(self.here());
                    }
                    let varying = word == "varchar" || self.eat_word("varying");
                    keyword(if varying { "varchar" } else { "bpchar" }, self.precision()?)
                }
                "bit" => {
                    let varying = self.eat_word("varying");
                    keyword(if varying { "varbit" } else { "bit" }, self.modifiers()?)
                }
                "time" | "timestamp" => {
                    let precision = self.precision()?;
                    let zone = self.eat_word("with");
                    if (zone || self.eat_word("without"))
                        && !(self.eat_word("time") && self.eat_word("zone"))
                    {
                        return Err(self.here());
                    }
                    let name = match (word.as_str(), zone) {
                        ("time", false) => "time",
                        ("time", true) => "timetz",
                        (_, false) => "timestamp",
                        (_, true) => "timestamptz",
                    };
                    keyword(name, precision)
                }
                "interval" => {
                    let mut precision = self.precision()?;
                    while matches!(
                        self.peek(),
                        Some(Token::Word(word)) if matches!(
                            word.as_str(),
                            "year" | "month" | "day" | "hour" | "minute" | "second" | "to"
                        )
                    ) {
                        self.at += 1;
                    }
                    if precision.is_empty() {
                        precision = self.precision()?;
                    }
                    keyword("interval", precision)
                }
                word if NOT_TYPE_NAMES.contains(&word) => return Err(start),
                word => self.generic(word.to_owned())?,
            },
            Token::Quoted(name) => self.generic(name.clone())?,
            _ => return Err(start),
        };
        // The array brackets, any number of them, or `ARRAY` with at most one bound.
        if self.eat_word("array") {
            if self.eat(&Token::OpenBracket) {
                self.iconst()?;
                self.expect(&Token::CloseBracket)?;
            }
            written.array = true;
        } else {
            while self.eat(&Token::OpenBracket) {
                if matches!(self.peek(), Some(Token::Number(_))) {
                    self.iconst()?;
                }
                self.expect(&Token::CloseBracket)?;
                written.array = true;
            }
        }
        match self.here() {
            None => Ok(written),
            near => Err(near),
        }
    }

    /// A name that is not spelled in words, with the names after it and its type modifiers.
    fn generic(&mut self, first: String) -> Result<WrittenType, Option<Range<usize>>> {
        let mut names = vec![first];
        while self.eat(&Token::Dot) {
            match self.peek().cloned() {
                Some(Token::Word(name) | Token::Quoted(name)) => {
                    self.at += 1;
                    names.push(name);
                }
                _ => return Err(self.here()),
            }
        }
        Ok(WrittenType { names, keyword: false, modifiers: self.modifiers()?, array: false })
    }
}

impl WrittenType {
    /// The type that the names name, with the errors of the lookup and of the type modifiers.
    fn resolve(self) -> Result<DeclaredType, TypeError> {
        let shown = self.names.join(".");
        let name = match &self.names[..] {
            [name] => name,
            [schema, name] if schema == "pg_catalog" => name,
            [_, _] => return Err(undefined(&shown, self.array)),
            [_, _, _] => {
                return Err(TypeError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cross-database references are not implemented: {shown}"),
                ));
            }
            _ => {
                return Err(TypeError::new(
                    SqlState::SYNTAX_ERROR,
                    format!("improper qualified name (too many dotted names): {shown}"),
                ));
            }
        };
        if self.keyword && name == "float" {
            return match self.modifiers[..] {
                [Some(p)] if p < 1 => {
                    Err(invalid_modifier("precision for type float must be at least 1 bit"))
                }
                [Some(p)] if p > 53 => {
                    Err(invalid_modifier("precision for type float must be less than 54 bits"))
                }
                _ => Err(invalid_modifier("invalid type modifier")),
            };
        }
        let info = TypeInfo::by_name(name).ok_or_else(|| undefined(&shown, self.array))?;
        check_modifiers(info.oid, &shown, &self.modifiers)?;
        let oid = match self.array && !info.is_array() {
            false => info.oid,
            true if info.array != 0 => info.array,
            true => {
                return Err(TypeError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("could not find array type for data type {shown}"),
                ));
            }
        };
        Ok(DeclaredType { oid, typmod: -1 })
    }
}

/// `42704`, with the name as it is written and `[]` for an array.
fn undefined(shown: &str, array: bool) -> TypeError {
    let brackets = if array { "[]" } else { "" };
    TypeError::new(SqlState::UNDEFINED_OBJECT, format!("type \"{shown}{brackets}\" does not exist"))
}

fn invalid_modifier(message: &str) -> TypeError {
    TypeError::new(SqlState::INVALID_PARAMETER_VALUE, message.to_owned())
}

/// The errors of the `typmodin` function of the type `oid` for these type modifiers, as
/// `numerictypmodin`, `anychar_typmodin`, `anybit_typmodin` and `anytime_typmodin` give them.
fn check_modifiers(oid: Oid, shown: &str, modifiers: &[Option<i32>]) -> Result<(), TypeError> {
    if modifiers.is_empty() {
        return Ok(());
    }
    let one = || match modifiers {
        [Some(n)] => Ok(*n),
        _ => Err(invalid_modifier("invalid type modifier")),
    };
    let length = |kind: &str, max: i32| {
        let n = one()?;
        if n < 1 {
            return Err(invalid_modifier(&format!("length for type {kind} must be at least 1")));
        }
        if n > max {
            return Err(invalid_modifier(&format!("length for type {kind} cannot exceed {max}")));
        }
        Ok(())
    };
    match oid {
        oid::NUMERIC => {
            let (p, s) = match modifiers {
                [Some(p)] => (*p, 0),
                [Some(p), Some(s)] => (*p, *s),
                _ => return Err(invalid_modifier("invalid NUMERIC type modifier")),
            };
            if !(1..=NUMERIC_MAX_PRECISION).contains(&p) {
                return Err(invalid_modifier(&format!(
                    "NUMERIC precision {p} must be between 1 and {NUMERIC_MAX_PRECISION}"
                )));
            }
            if !(NUMERIC_MIN_SCALE..=NUMERIC_MAX_SCALE).contains(&s) {
                return Err(invalid_modifier(&format!(
                    "NUMERIC scale {s} must be between {NUMERIC_MIN_SCALE} and {NUMERIC_MAX_SCALE}"
                )));
            }
            Ok(())
        }
        oid::VARCHAR => length("varchar", MAX_LENGTH),
        oid::BPCHAR => length("char", MAX_LENGTH),
        oid::BIT => length("bit", MAX_BITS),
        oid::VARBIT => length("varbit", MAX_BITS),
        oid::TIME | oid::TIMETZ | oid::TIMESTAMP | oid::TIMESTAMPTZ | oid::INTERVAL => {
            let p = one()?;
            if p < 0 {
                let (name, zone) = match oid {
                    oid::TIME => ("TIME", ""),
                    oid::TIMETZ => ("TIME", " WITH TIME ZONE"),
                    oid::TIMESTAMP => ("TIMESTAMP", ""),
                    oid::TIMESTAMPTZ => ("TIMESTAMP", " WITH TIME ZONE"),
                    _ => ("INTERVAL", ""),
                };
                return Err(invalid_modifier(&format!(
                    "{name}({p}){zone} precision must not be negative"
                )));
            }
            Ok(())
        }
        _ => Err(TypeError::new(
            SqlState::SYNTAX_ERROR,
            format!("type modifier is not allowed for type \"{shown}\""),
        )),
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

    #[test]
    fn a_type_name_has_the_errors_of_postgresql() {
        let found = |text| type_name(text).map(|declared| declared.oid).unwrap();
        assert_eq!(found("INT4"), oid::INT4);
        assert_eq!(found("pg_catalog.int4"), oid::INT4);
        assert_eq!(found("_int4"), oid::INT4_ARRAY);
        assert_eq!(found("int4[3]"), oid::INT4_ARRAY);
        assert_eq!(found("int4 array[2]"), oid::INT4_ARRAY);
        assert_eq!(found("interval year to month"), oid::INTERVAL);
        assert_eq!(found("double precision"), oid::FLOAT8);
        assert_eq!(found("timestamp(2) with time zone"), oid::TIMESTAMPTZ);
        assert_eq!(found("void"), oid::VOID);
        assert_eq!(found("record"), oid::RECORD);
        let error = |text| {
            let error = type_name(text).unwrap_err();
            (error.sqlstate.as_str().to_owned(), error.message, error.context)
        };
        let context = |text: &str| Some(format!("invalid type name \"{text}\""));
        for (text, code, message) in [
            ("nosuch", "42704", "type \"nosuch\" does not exist"),
            ("public.nosuch", "42704", "type \"public.nosuch\" does not exist"),
            ("pg_catalog.nosuch[]", "42704", "type \"pg_catalog.nosuch[]\" does not exist"),
            ("\"INT4\"", "42704", "type \"INT4\" does not exist"),
            ("nosuch(3)", "42704", "type \"nosuch\" does not exist"),
            ("a.b.c", "0A000", "cross-database references are not implemented: a.b.c"),
            ("a.b.c.d", "42601", "improper qualified name (too many dotted names): a.b.c.d"),
            ("int4(3)", "42601", "type modifier is not allowed for type \"int4\""),
            ("numeric(1001)", "22023", "NUMERIC precision 1001 must be between 1 and 1000"),
            ("numeric(2,-1001)", "22023", "NUMERIC scale -1001 must be between -1000 and 1000"),
            ("numeric(1,2,3)", "22023", "invalid NUMERIC type modifier"),
            ("character varying(0)", "22023", "length for type varchar must be at least 1"),
            ("varchar(10485761)", "22023", "length for type varchar cannot exceed 10485760"),
            ("char(0)", "22023", "length for type char must be at least 1"),
            ("bit(0)", "22023", "length for type bit must be at least 1"),
            ("float(54)", "22023", "precision for type float must be less than 54 bits"),
            ("float(0)", "22023", "precision for type float must be at least 1 bit"),
            ("", "42601", "invalid type name \"\""),
            ("setof int4", "42601", "invalid type name \"setof int4\""),
        ] {
            assert_eq!(error(text), (code.to_owned(), message.to_owned(), None), "{text}");
        }
        for (text, message) in [
            ("int4(", "syntax error at end of input"),
            ("int4 int4", "syntax error at or near \"int4\""),
            ("int4;", "syntax error at or near \";\""),
            ("1", "syntax error at or near \"1\""),
            ("int4)", "syntax error at or near \")\""),
            ("select", "syntax error at or near \"select\""),
            ("time(-1)", "syntax error at or near \"-\""),
            ("\"unterminated", "unterminated quoted identifier at or near \"\"unterminated\""),
        ] {
            assert_eq!(error(text), ("42601".to_owned(), message.to_owned(), context(text)));
        }
    }
}
