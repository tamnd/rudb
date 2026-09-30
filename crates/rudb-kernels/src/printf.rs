//! `format` and `printf`, which are ports of the copy of the fmt library the pin vendors.
//!
//! That copy is fmt 6 with DuckDB's changes on top of it, so neither the modern library nor C's
//! printf is the reference. The changes that show are these. An error is an `InvalidInputException`
//! in fmt's words, so `format('{')` is `invalid format string`. `,`, `_`, `'` and `t` followed by
//! any character set a thousands separator, and a `.` separator turns the decimal point into a
//! comma. In `printf` a `d` with an empty precision groups by `.`, so `printf('%.d', 123456)` is
//! `123.456`. Width and precision count code points and not bytes. A name where an argument index
//! goes is always refused, since the pin passes no named arguments.
//!
//! The binder casts every argument after the format to one of the kinds fmt tells apart: BOOLEAN,
//! BIGINT, UBIGINT, HUGEINT, UHUGEINT, DOUBLE or VARCHAR. The digits of a double with a precision
//! are Grisu's, which fmt falls back from to snprintf when it cannot tell which way to round, and
//! both are kept as they are. A general format with precision 0 falls back to six digits after the
//! point on an exact tie, so `format('{:.0}', 2.5)` is `2.5` while `format('{:.0}', 1.25)` is `1`,
//! and a fixed format with precision 0 and `#` keeps snprintf's point on a tie as well as adding
//! its own, so `format('{:#.0f}', 2.5)` is `2..`.

use rudb_common::{Error, Result, Value};

/// An argument in the kinds fmt tells apart, after `printf` converted it for a length modifier.
#[derive(Clone, Copy, Debug)]
enum Arg<'a> {
    Int(Int),
    Bool(bool),
    Char(u8),
    Double(f64),
    Text(&'a str),
}

/// The integer kinds, which matter for the sign checks and for `printf`'s conversions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    I32,
    U32,
    I64,
    U64,
    I128,
    U128,
}

impl Kind {
    fn signed(self) -> bool {
        matches!(self, Kind::I32 | Kind::I64 | Kind::I128)
    }

    fn size(self) -> usize {
        match self {
            Kind::I32 | Kind::U32 => 4,
            Kind::I64 | Kind::U64 => 8,
            Kind::I128 | Kind::U128 => 16,
        }
    }
}

/// An integer as its two's complement bits, sign extended to 128 when its kind is signed.
#[derive(Clone, Copy, Debug)]
struct Int {
    bits: u128,
    kind: Kind,
}

impl Int {
    fn signed(value: i128, kind: Kind) -> Int {
        Int { bits: value as u128, kind }
    }

    fn unsigned(value: u128, kind: Kind) -> Int {
        Int { bits: value, kind }
    }

    fn negative(self) -> bool {
        self.kind.signed() && (self.bits as i128) < 0
    }

    fn magnitude(self) -> u128 {
        if self.negative() { (self.bits as i128).unsigned_abs() } else { self.bits }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Align {
    None,
    Left,
    Right,
    Center,
    Numeric,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sign {
    None,
    Minus,
    Plus,
    Space,
}

#[derive(Clone, Copy, Debug)]
struct Specs {
    width: usize,
    precision: i32,
    fill: u8,
    align: Align,
    sign: Sign,
    alt: bool,
    thousands: u8,
    ty: u8,
}

impl Default for Specs {
    fn default() -> Specs {
        Specs {
            width: 0,
            precision: -1,
            fill: b' ',
            align: Align::None,
            sign: Sign::None,
            alt: false,
            thousands: 0,
            ty: 0,
        }
    }
}

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::invalid_input(message.into()))
}

fn out_of_range<T>(id: usize) -> Result<T> {
    fail(format!("Argument index \"{id}\" out of range"))
}

fn invalid_type<T>(ty: u8, of: &str) -> Result<T> {
    let ty = String::from_utf8_lossy(&[ty]).into_owned();
    fail(format!("Invalid type specifier \"{ty}\" for formatting a value of type {of}"))
}

/// `format(pattern, arguments...)`.
pub(crate) fn format(pattern: &str, arguments: &[Value]) -> Result<Value> {
    let arguments = read(arguments)?;
    let mut formatter = Formatter::new(pattern, &arguments);
    formatter.format()?;
    finish(pattern, formatter.out)
}

/// `printf(pattern, arguments...)`.
pub(crate) fn printf(pattern: &str, arguments: &[Value]) -> Result<Value> {
    let arguments = read(arguments)?;
    let mut formatter = Formatter::new(pattern, &arguments);
    formatter.printf()?;
    finish(pattern, formatter.out)
}

fn read(arguments: &[Value]) -> Result<Vec<Arg<'_>>> {
    arguments
        .iter()
        .map(|value| {
            Ok(match value {
                Value::Boolean(value) => Arg::Bool(*value),
                Value::BigInt(value) => Arg::Int(Int::signed(i128::from(*value), Kind::I64)),
                Value::UBigInt(value) => Arg::Int(Int::unsigned(u128::from(*value), Kind::U64)),
                Value::HugeInt(value) => Arg::Int(Int::signed(*value, Kind::I128)),
                Value::UHugeInt(value) => Arg::Int(Int::unsigned(*value, Kind::U128)),
                Value::Double(value) => Arg::Double(*value),
                Value::Varchar(text) => Arg::Text(text),
                other => {
                    return Err(Error::internal(format!(
                        "Unexpected type for printf format: {other:?}"
                    )));
                }
            })
        })
        .collect()
}

fn finish(pattern: &str, out: Vec<u8>) -> Result<Value> {
    match String::from_utf8(out) {
        Ok(text) => Ok(Value::Varchar(text)),
        Err(_) => fail(format!(
            "Invalid UTF8 produced by format string \"{pattern}\" - note that %c writes a single byte, use chr(...) to write a Unicode code point"
        )),
    }
}

struct Formatter<'p, 'a> {
    /// The pattern up to its first NUL, since the pin hands fmt a C string.
    pattern: &'p [u8],
    arguments: &'p [Arg<'a>],
    /// The next automatic index, or -1 once an index was written out.
    next: i64,
    out: Vec<u8>,
}

impl<'p, 'a> Formatter<'p, 'a> {
    fn new(pattern: &'p str, arguments: &'p [Arg<'a>]) -> Formatter<'p, 'a> {
        let pattern = pattern.as_bytes();
        let end = pattern.iter().position(|byte| *byte == 0).unwrap_or(pattern.len());
        Formatter { pattern: &pattern[..end], arguments, next: 0, out: Vec::new() }
    }

    fn at(&self, at: usize) -> u8 {
        self.pattern.get(at).copied().unwrap_or(0)
    }

    fn next_id(&mut self) -> Result<usize> {
        if self.next < 0 {
            return fail("cannot switch from manual to automatic argument indexing");
        }
        let id = self.next as usize;
        self.next += 1;
        Ok(id)
    }

    fn check_id(&mut self) -> Result<()> {
        if self.next > 0 {
            return fail("cannot switch from automatic to manual argument indexing");
        }
        self.next = -1;
        Ok(())
    }

    fn argument(&self, id: usize) -> Result<Arg<'a>> {
        match self.arguments.get(id) {
            Some(argument) => Ok(*argument),
            None => out_of_range(id),
        }
    }

    /// fmt's `parse_nonnegative_int`, where a leading zero is a number on its own.
    fn number(&self, at: &mut usize) -> Result<usize> {
        if self.at(*at) == b'0' {
            *at += 1;
            return Ok(0);
        }
        let mut value: u64 = 0;
        while self.at(*at).is_ascii_digit() {
            if value > i32::MAX as u64 / 10 {
                value = i32::MAX as u64 + 1;
                break;
            }
            value = value * 10 + u64::from(self.at(*at) - b'0');
            *at += 1;
        }
        if value > i32::MAX as u64 {
            return fail("number is too big");
        }
        Ok(value as usize)
    }

    fn format(&mut self) -> Result<()> {
        let end = self.pattern.len();
        let mut begin = 0;
        while begin != end {
            let Some(open) = self.pattern[begin..].iter().position(|byte| *byte == b'{') else {
                return self.text(begin, end);
            };
            let mut p = begin + open;
            self.text(begin, p)?;
            p += 1;
            if p == end {
                return fail("invalid format string");
            }
            match self.pattern[p] {
                b'}' => {
                    let id = self.next_id()?;
                    let argument = self.argument(id)?;
                    self.write(argument, Specs::default(), false)?;
                }
                b'{' => self.out.push(b'{'),
                _ => {
                    let id = self.arg_id(&mut p)?;
                    let argument = self.argument(id)?;
                    match self.at(p) {
                        b'}' if p < end => self.write(argument, Specs::default(), false)?,
                        b':' if p < end => {
                            p += 1;
                            let specs = self.specs(&mut p, argument)?;
                            if p == end || self.pattern[p] != b'}' {
                                return fail("missing '}' in format string");
                            }
                            self.write(argument, specs, false)?;
                        }
                        _ => return fail("missing '}' in format string"),
                    }
                }
            }
            begin = p + 1;
        }
        Ok(())
    }

    /// The text between replacement fields, where `}}` is one brace and a lone one is an error.
    fn text(&mut self, mut begin: usize, end: usize) -> Result<()> {
        while begin < end {
            let Some(close) = self.pattern[begin..end].iter().position(|byte| *byte == b'}') else {
                self.out.extend_from_slice(&self.pattern[begin..end]);
                return Ok(());
            };
            let p = begin + close + 1;
            if p == end || self.pattern[p] != b'}' {
                return fail("unmatched '}' in format string");
            }
            self.out.extend_from_slice(&self.pattern[begin..p]);
            begin = p + 1;
        }
        Ok(())
    }

    /// An argument index, or the next automatic one when there is none before `}` or `:`.
    fn arg_id(&mut self, p: &mut usize) -> Result<usize> {
        let c = self.at(*p);
        if c == b'}' || c == b':' {
            return self.next_id();
        }
        if c.is_ascii_digit() {
            let index = self.number(p)?;
            if *p == self.pattern.len() || !matches!(self.pattern[*p], b'}' | b':') {
                return fail("invalid format string");
            }
            self.check_id()?;
            return Ok(index);
        }
        let name_start = |c: u8| c.is_ascii_alphabetic() || c == b'_';
        if !name_start(c) {
            return fail("invalid format string");
        }
        let start = *p;
        let mut it = start + 1;
        while it < self.pattern.len() && (name_start(self.at(it)) || self.at(it).is_ascii_digit()) {
            it += 1;
        }
        let name = String::from_utf8_lossy(&self.pattern[start..it]).into_owned();
        fail(format!(
            "Argument with name \"{name}\" not found, did you mean to use it as a format specifier (e.g. {{:{name}}}"
        ))
    }

    /// fmt's `parse_format_specs` with the checks it makes against the argument.
    fn specs(&mut self, p: &mut usize, argument: Arg<'a>) -> Result<Specs> {
        let end = self.pattern.len();
        let mut specs = Specs::default();
        let arithmetic = !matches!(argument, Arg::Text(_));
        let integral = matches!(argument, Arg::Int(_) | Arg::Bool(_) | Arg::Char(_));
        let numeric = || {
            if arithmetic { Ok(()) } else { fail("format specifier requires numeric argument") }
        };
        let signed = || {
            numeric()?;
            let allowed = match argument {
                Arg::Int(int) => matches!(int.kind, Kind::I32 | Kind::I64),
                Arg::Bool(_) => false,
                _ => true,
            };
            if allowed { Ok(()) } else { fail("format specifier requires signed argument") }
        };
        if *p == end || self.pattern[*p] == b'}' {
            return Ok(specs);
        }
        // The fill is one byte and comes before the alignment when there is one.
        let alignment = |c: u8| match c {
            b'<' => Align::Left,
            b'>' => Align::Right,
            b'=' => Align::Numeric,
            b'^' => Align::Center,
            _ => Align::None,
        };
        let mut i = usize::from(*p + 1 != end);
        loop {
            let align = alignment(self.pattern[*p + i]);
            if align != Align::None {
                if i > 0 {
                    let c = self.pattern[*p];
                    if c == b'{' {
                        return fail("invalid fill character '{'");
                    }
                    *p += 2;
                    specs.fill = c;
                } else {
                    *p += 1;
                }
                if align == Align::Numeric {
                    numeric()?;
                }
                specs.align = align;
                break;
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }
        if *p == end {
            return Ok(specs);
        }
        match self.pattern[*p] {
            b'+' => {
                signed()?;
                specs.sign = Sign::Plus;
                *p += 1;
            }
            b'-' => {
                signed()?;
                specs.sign = Sign::Minus;
                *p += 1;
            }
            b' ' => {
                signed()?;
                specs.sign = Sign::Space;
                *p += 1;
            }
            c @ (b',' | b'_' | b'\'') => {
                specs.thousands = c;
                *p += 1;
            }
            b't' => {
                *p += 1;
                if *p == end {
                    return Ok(specs);
                }
                specs.thousands = self.pattern[*p];
                *p += 1;
            }
            _ => {}
        }
        if *p == end {
            return Ok(specs);
        }
        if self.pattern[*p] == b'#' {
            numeric()?;
            specs.alt = true;
            *p += 1;
            if *p == end {
                return Ok(specs);
            }
        }
        if self.pattern[*p] == b'0' {
            numeric()?;
            specs.align = Align::Numeric;
            specs.fill = b'0';
            *p += 1;
            if *p == end {
                return Ok(specs);
            }
        }
        if self.pattern[*p].is_ascii_digit() {
            specs.width = self.number(p)?;
        } else if self.pattern[*p] == b'{' {
            *p += 1;
            if *p != end {
                specs.width = self.dynamic(p, "width")?;
            }
            if *p == end || self.pattern[*p] != b'}' {
                return fail("invalid format string");
            }
            *p += 1;
        }
        if *p == end {
            return Ok(specs);
        }
        if self.pattern[*p] == b'.' {
            *p += 1;
            let c = self.at(*p);
            if c.is_ascii_digit() && *p < end {
                specs.precision = self.number(p)? as i32;
            } else if c == b'{' && *p < end {
                *p += 1;
                if *p != end {
                    specs.precision = self.dynamic(p, "precision")? as i32;
                }
                if *p == end || self.pattern[*p] != b'}' {
                    return fail("invalid format string");
                }
                *p += 1;
            } else {
                return fail("missing precision specifier");
            }
            if integral {
                return fail("precision not allowed for this argument type");
            }
        }
        if *p != end && self.pattern[*p] != b'}' {
            specs.ty = self.pattern[*p];
            *p += 1;
        }
        Ok(specs)
    }

    /// A width or precision taken from an argument, which `format` checks with fmt's
    /// `width_checker` and `precision_checker`.
    fn dynamic(&mut self, p: &mut usize, what: &str) -> Result<usize> {
        let id = self.arg_id(p)?;
        let Arg::Int(int) = self.argument(id)? else {
            return fail(format!("{what} is not integer"));
        };
        if int.negative() {
            return fail(format!("negative {what}"));
        }
        if int.bits > i32::MAX as u128 {
            return fail("number is too big");
        }
        Ok(int.bits as usize)
    }

    /// The printf loop, which is fmt's `basic_printf_context::format` with the pin's changes.
    fn printf(&mut self) -> Result<()> {
        let end = self.pattern.len();
        let mut start = 0;
        let mut it = 0;
        while it != end {
            let c = self.pattern[it];
            it += 1;
            if c != b'%' {
                continue;
            }
            if it != end && self.pattern[it] == b'%' {
                self.out.extend_from_slice(&self.pattern[start..it]);
                it += 1;
                start = it;
                continue;
            }
            self.out.extend_from_slice(&self.pattern[start..it - 1]);
            let mut specs = Specs { align: Align::Right, ..Specs::default() };
            let index = self.header(&mut it, &mut specs)?;
            if index == Some(0) {
                return fail("argument index out of range");
            }
            let mut empty_precision = false;
            if it != end && self.pattern[it] == b'.' {
                it += 1;
                let c = self.at(it);
                if c.is_ascii_digit() && it != end {
                    specs.precision = self.number(&mut it)? as i32;
                } else if c == b'*' && it != end {
                    it += 1;
                    let argument = self.printf_argument(None)?;
                    specs.precision = printf_precision(argument)?;
                } else {
                    specs.precision = 0;
                    empty_precision = true;
                }
            }
            let mut argument = self.printf_argument(index)?;
            let zero = match argument {
                Arg::Int(int) => int.bits == 0,
                Arg::Bool(value) => !value,
                Arg::Char(value) => value == 0,
                _ => false,
            };
            if specs.alt && zero {
                specs.alt = false;
            }
            if specs.fill == b'0' {
                if matches!(argument, Arg::Text(_)) {
                    specs.fill = b' ';
                } else {
                    specs.align = Align::Numeric;
                }
            }
            // The length modifier. With none, the character read is put back, and when the pattern
            // ended there that is the character before the end, which becomes the type.
            let c = if it != end {
                it += 1;
                self.pattern[it - 1]
            } else {
                0
            };
            let mut t = self.at(it);
            match c {
                b'h' => {
                    if t == b'h' {
                        it += 1;
                        t = self.at(it);
                        argument = convert(argument, Some(Target::I8), t);
                    } else {
                        argument = convert(argument, Some(Target::I16), t);
                    }
                }
                b'l' => {
                    if t == b'l' {
                        it += 1;
                        t = self.at(it);
                    }
                    argument = convert(argument, Some(Target::I64), t);
                }
                b'j' | b't' => argument = convert(argument, Some(Target::I64), t),
                b'z' => argument = convert(argument, Some(Target::U64), t),
                b'L' => {}
                _ => {
                    it -= 1;
                    argument = convert(argument, None, c);
                }
            }
            if it >= end {
                return fail("invalid format string");
            }
            specs.ty = self.pattern[it];
            it += 1;
            if let Arg::Int(_) | Arg::Bool(_) | Arg::Char(_) = argument {
                match specs.ty {
                    b'i' | b'u' => specs.ty = b'd',
                    b'c' => argument = Arg::Char(low_byte(argument)),
                    _ => {}
                }
            }
            if specs.ty == b'd' && empty_precision {
                specs.thousands = b'.';
            }
            start = it;
            self.write(argument, specs, true)?;
        }
        self.out.extend_from_slice(&self.pattern[start..it]);
        Ok(())
    }

    /// The argument index, the flags and the width of a printf conversion.
    fn header(&mut self, it: &mut usize, specs: &mut Specs) -> Result<Option<usize>> {
        let end = self.pattern.len();
        let mut index = None;
        let c = self.at(*it);
        if c.is_ascii_digit() && *it != end {
            let value = self.number(it)?;
            if *it != end && self.pattern[*it] == b'$' {
                *it += 1;
                index = Some(value);
            } else {
                if c == b'0' {
                    specs.fill = b'0';
                }
                if value != 0 {
                    specs.width = value;
                    return Ok(index);
                }
            }
        }
        while *it != end {
            match self.pattern[*it] {
                b'-' => specs.align = Align::Left,
                b'+' => specs.sign = Sign::Plus,
                b'0' => specs.fill = b'0',
                b' ' => specs.sign = Sign::Space,
                b'#' => specs.alt = true,
                c @ (b',' | b'\'' | b'_') => specs.thousands = c,
                _ => break,
            }
            *it += 1;
        }
        if *it != end {
            if self.pattern[*it].is_ascii_digit() {
                specs.width = self.number(it)?;
            } else if self.pattern[*it] == b'*' {
                *it += 1;
                let argument = self.printf_argument(None)?;
                specs.width = printf_width(argument, specs)?;
            }
        }
        Ok(index)
    }

    /// The argument a printf conversion names, counting from one, or the next one.
    fn printf_argument(&mut self, index: Option<usize>) -> Result<Arg<'a>> {
        let id = match index {
            None => self.next_id()?,
            Some(index) => {
                self.check_id()?;
                index - 1
            }
        };
        self.argument(id)
    }

    /// Writes one argument, the way fmt's `arg_formatter` or `printf_arg_formatter` does.
    fn write(&mut self, argument: Arg<'a>, mut specs: Specs, printf: bool) -> Result<()> {
        match argument {
            Arg::Int(int) => write_int(&mut self.out, int, &specs),
            Arg::Bool(value) => {
                let written = if printf { specs.ty != b's' } else { specs.ty != 0 };
                if written {
                    write_int(&mut self.out, Int::signed(i128::from(value), Kind::I32), &specs)
                } else {
                    specs.ty = 0;
                    let text = if value { "true" } else { "false" };
                    write_text(&mut self.out, text.as_bytes(), &specs);
                    Ok(())
                }
            }
            Arg::Char(value) => {
                specs.sign = Sign::None;
                specs.alt = false;
                specs.align = Align::Right;
                write_padded(&mut self.out, &specs, &[value], 1);
                Ok(())
            }
            Arg::Double(value) => write_float(&mut self.out, value, specs),
            Arg::Text(text) => {
                if specs.ty != 0 && specs.ty != b's' {
                    return invalid_type(specs.ty, "string");
                }
                write_text(&mut self.out, text.as_bytes(), &specs);
                Ok(())
            }
        }
    }
}

/// The type a printf length modifier converts an integer to.
#[derive(Clone, Copy)]
enum Target {
    I8,
    I16,
    I64,
    U64,
}

/// fmt's `arg_converter`, which gives an integer the size the length modifier names and the
/// signedness the conversion does.
fn convert(argument: Arg<'_>, target: Option<Target>, ty: u8) -> Arg<'_> {
    // A boolean keeps its size and is unsigned, so it is a kind of its own here.
    let (bits, kind) = match argument {
        Arg::Bool(_) if ty == b's' => return argument,
        Arg::Bool(value) => (u128::from(value), None),
        Arg::Int(int) => (int.bits, Some(int.kind)),
        _ => return argument,
    };
    let unsigned = kind.is_some_and(|kind| !kind.signed());
    let ty = if matches!(ty, b'd' | b'i') && unsigned { b'u' } else { ty };
    let from_signed = kind.is_some_and(Kind::signed);
    let is_signed = matches!(ty, b'd' | b'i')
        || (matches!(ty, b'a' | b'A' | b'e' | b'E' | b'f' | b'F' | b'g' | b'G') && from_signed);
    let size = match target {
        Some(Target::I8) => 1,
        Some(Target::I16) => 2,
        Some(Target::I64 | Target::U64) => 8,
        None => kind.map_or(1, Kind::size),
    };
    if size <= 4 {
        let narrowed = match target {
            Some(Target::I8) => {
                if is_signed {
                    i128::from(bits as u8 as i8)
                } else {
                    i128::from(bits as u8)
                }
            }
            Some(Target::I16) => {
                if is_signed {
                    i128::from(bits as u16 as i16)
                } else {
                    i128::from(bits as u16)
                }
            }
            _ => bits as i128,
        };
        return if is_signed {
            Arg::Int(Int::signed(i128::from(narrowed as i32), Kind::I32))
        } else {
            Arg::Int(Int::unsigned(u128::from(narrowed as u32), Kind::U32))
        };
    }
    if is_signed {
        if size > 8 {
            return Arg::Int(Int::signed(bits as i128, Kind::I128));
        }
        return Arg::Int(Int::signed(i128::from(bits as u64 as i64), Kind::I64));
    }
    match kind {
        None => argument,
        Some(Kind::I128 | Kind::U128) => Arg::Int(Int::unsigned(bits, Kind::U128)),
        Some(Kind::I64 | Kind::U64) => Arg::Int(Int::unsigned(u128::from(bits as u64), Kind::U64)),
        Some(Kind::I32 | Kind::U32) => Arg::Int(Int::unsigned(u128::from(bits as u32), Kind::U32)),
    }
}

fn low_byte(argument: Arg<'_>) -> u8 {
    match argument {
        Arg::Int(int) => int.bits as u8,
        Arg::Bool(value) => u8::from(value),
        Arg::Char(value) => value,
        _ => 0,
    }
}

/// fmt's `printf_width_handler`, where a negative width aligns to the left.
fn printf_width(argument: Arg<'_>, specs: &mut Specs) -> Result<usize> {
    let int = match argument {
        Arg::Int(int) => int,
        Arg::Bool(value) => Int::unsigned(u128::from(value), Kind::U32),
        Arg::Char(value) => Int::signed(i128::from(value as i8), Kind::I32),
        _ => return fail("width is not integer"),
    };
    if int.negative() {
        specs.align = Align::Left;
    }
    let width = int.magnitude();
    if width > i32::MAX as u128 {
        return fail("number is too big");
    }
    Ok(width as usize)
}

/// fmt's `printf_precision_handler`, where a negative precision is none.
fn printf_precision(argument: Arg<'_>) -> Result<i32> {
    let int = match argument {
        Arg::Int(int) => int,
        Arg::Bool(value) => return Ok(i32::from(value)),
        Arg::Char(value) => Int::signed(i128::from(value as i8), Kind::I32),
        _ => return fail("precision is not integer"),
    };
    let fits = if int.kind.signed() {
        i32::try_from(int.bits as i128).is_ok()
    } else {
        int.bits <= i32::MAX as u128
    };
    if !fits {
        return fail("number is too big");
    }
    Ok((int.bits as i128 as i32).max(0))
}

fn count_code_points(text: &[u8]) -> usize {
    text.iter().filter(|byte| (*byte & 0xc0) != 0x80).count()
}

/// fmt's `write_padded`, where anything but right and center pads on the right.
fn write_padded(out: &mut Vec<u8>, specs: &Specs, body: &[u8], width: usize) {
    if specs.width <= width {
        out.extend_from_slice(body);
        return;
    }
    let padding = specs.width - width;
    let fill = |out: &mut Vec<u8>, count: usize| out.extend(std::iter::repeat_n(specs.fill, count));
    match specs.align {
        Align::Right => {
            fill(out, padding);
            out.extend_from_slice(body);
        }
        Align::Center => {
            fill(out, padding / 2);
            out.extend_from_slice(body);
            fill(out, padding - padding / 2);
        }
        _ => {
            out.extend_from_slice(body);
            fill(out, padding);
        }
    }
}

/// A string, cut to its precision and padded to its width in code points.
fn write_text(out: &mut Vec<u8>, text: &[u8], specs: &Specs) {
    let mut text = text;
    if specs.precision >= 0 && (specs.precision as usize) < text.len() {
        let wanted = specs.precision as usize;
        let mut seen = 0;
        let cut = text
            .iter()
            .position(|byte| {
                if (byte & 0xc0) != 0x80 {
                    seen += 1;
                    seen > wanted
                } else {
                    false
                }
            })
            .unwrap_or(text.len());
        text = &text[..cut];
    }
    write_padded(out, specs, text, count_code_points(text));
}

/// fmt's `int_writer` behind `handle_int_type_spec`.
fn write_int(out: &mut Vec<u8>, int: Int, specs: &Specs) -> Result<()> {
    let mut prefix = Vec::new();
    if int.negative() {
        prefix.push(b'-');
    } else if specs.sign == Sign::Plus {
        prefix.push(b'+');
    } else if specs.sign == Sign::Space {
        prefix.push(b' ');
    }
    let magnitude = int.magnitude();
    let grouped = |separator: u8| {
        let digits = magnitude.to_string().into_bytes();
        let mut written = Vec::with_capacity(digits.len() * 4 / 3);
        for (i, digit) in digits.iter().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                written.push(separator);
            }
            written.push(*digit);
        }
        written
    };
    let digits = if specs.thousands != 0 {
        grouped(specs.thousands)
    } else {
        match specs.ty {
            0 | b'd' | b'n' | b'l' | b'L' => magnitude.to_string().into_bytes(),
            b'x' | b'X' => {
                if specs.alt {
                    prefix.extend_from_slice(&[b'0', specs.ty]);
                }
                if specs.ty == b'x' {
                    format!("{magnitude:x}").into_bytes()
                } else {
                    format!("{magnitude:X}").into_bytes()
                }
            }
            b'b' | b'B' => {
                if specs.alt {
                    prefix.extend_from_slice(&[b'0', specs.ty]);
                }
                format!("{magnitude:b}").into_bytes()
            }
            b'o' => {
                let digits = format!("{magnitude:o}").into_bytes();
                // The prefix counts as a digit, so a precision that pads already shows the zero.
                if specs.alt && specs.precision <= digits.len() as i32 && magnitude != 0 {
                    prefix.push(b'0');
                }
                digits
            }
            other => return invalid_type(other, "int"),
        }
    };
    let mut size = prefix.len() + digits.len();
    let mut fill = specs.fill;
    let mut padding = 0;
    if specs.align == Align::Numeric {
        if specs.width > size {
            padding = specs.width - size;
            size = specs.width;
        }
    } else if specs.precision > digits.len() as i32 {
        let precision = specs.precision as usize;
        size = prefix.len() + precision;
        padding = precision - digits.len();
        fill = b'0';
    }
    let mut body = prefix;
    body.extend(std::iter::repeat_n(fill, padding));
    body.extend_from_slice(&digits);
    let mut specs = *specs;
    if specs.align == Align::None {
        specs.align = Align::Right;
    }
    write_padded(out, &specs, &body, size);
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FloatFormat {
    General,
    Exp,
    Fixed,
    Hex,
}

#[derive(Clone, Copy, Debug)]
struct FloatSpecs {
    format: FloatFormat,
    upper: bool,
    trailing_zeros: bool,
    separator: u8,
}

/// fmt's `basic_writer::write` for a double.
fn write_float(out: &mut Vec<u8>, value: f64, mut specs: Specs) -> Result<()> {
    let mut float = FloatSpecs {
        format: FloatFormat::General,
        upper: false,
        trailing_zeros: specs.alt,
        separator: specs.thousands,
    };
    match specs.ty {
        0 => float.trailing_zeros |= specs.precision != 0,
        b'G' | b'g' => {
            float.upper = specs.ty == b'G';
        }
        b'E' | b'e' => {
            float.upper = specs.ty == b'E';
            float.format = FloatFormat::Exp;
            float.trailing_zeros |= specs.precision != 0;
        }
        b'F' | b'f' => {
            float.upper = specs.ty == b'F';
            float.format = FloatFormat::Fixed;
            float.trailing_zeros |= specs.precision != 0;
        }
        b'A' | b'a' => {
            float.upper = specs.ty == b'A';
            float.format = FloatFormat::Hex;
        }
        b'n' | b'l' | b'L' => {}
        other => return invalid_type(other, "float"),
    }
    let mut sign = specs.sign;
    let mut value = value;
    if value.is_sign_negative() {
        sign = Sign::Minus;
        value = -value;
    } else if sign == Sign::Minus {
        sign = Sign::None;
    }
    let sign_byte = match sign {
        Sign::Minus => Some(b'-'),
        Sign::Plus => Some(b'+'),
        Sign::Space => Some(b' '),
        Sign::None => None,
    };
    if !value.is_finite() {
        let text: &[u8] = match (value.is_infinite(), float.upper) {
            (true, false) => b"inf",
            (true, true) => b"INF",
            (false, false) => b"nan",
            (false, true) => b"NAN",
        };
        let mut body = Vec::new();
        body.extend(sign_byte);
        body.extend_from_slice(text);
        write_padded(out, &specs, &body, body.len());
        return Ok(());
    }
    let mut sign_byte = sign_byte;
    if specs.align == Align::None {
        specs.align = Align::Right;
    } else if specs.align == Align::Numeric {
        if let Some(sign) = sign_byte.take() {
            out.push(sign);
            specs.width = specs.width.saturating_sub(1);
        }
        specs.align = Align::Right;
    }
    let mut body = Vec::new();
    body.extend(sign_byte);
    if float.format == FloatFormat::Hex {
        hex_float(&mut body, value, specs.precision, float.trailing_zeros, float.upper);
        write_padded(out, &specs, &body, body.len());
        return Ok(());
    }
    let mut precision = if specs.precision >= 0 || specs.ty == 0 { specs.precision } else { 6 };
    if float.format == FloatFormat::Exp {
        precision += 1;
    }
    let (digits, exp) = float_digits(value, precision, &float);
    let point = if float.separator == b'.' { b',' } else { b'.' };
    prettify(&mut body, &digits, exp, &float, precision, point);
    write_padded(out, &specs, &body, body.len());
    Ok(())
}

/// fmt's `format_float`: the digits of a non-negative double and the power of ten after them.
///
/// A precision is Grisu's to meet when it can, and snprintf's when Grisu cannot tell which way to
/// round or the precision is past 17. Grisu's answer is kept exactly, including where it is not
/// the correctly rounded one: its error term is an unsigned 64 bit integer that wraps when a fixed
/// format asks for many digits, so `printf('%.3f', 1e300)` has digits that are not those of 1e300.
fn float_digits(value: f64, precision: i32, float: &FloatSpecs) -> (Vec<u8>, i32) {
    let fixed = float.format == FloatFormat::Fixed;
    if value == 0.0 {
        if precision <= 0 || !fixed {
            return (b"0".to_vec(), 0);
        }
        return (vec![b'0'; precision as usize], -precision);
    }
    if precision == -1 {
        let written = format!("{value:e}");
        return significant(&written, true);
    }
    if precision <= 17
        && let Some(answer) = grisu(value, precision, fixed)
    {
        return answer;
    }
    if fixed {
        if precision == 0 {
            // snprintf's own digits are kept whole, and `%#.0f` writes a point after them.
            let mut digits = format!("{value:.0}").into_bytes();
            if float.trailing_zeros {
                digits.push(b'.');
            }
            return (digits, 0);
        }
        let digits = format!("{value:.*}", precision as usize).replace('.', "").into_bytes();
        return (digits, -precision);
    }
    // snprintf is asked for `%e`, with no precision when there were no digits to ask for, which
    // is six after the point.
    let after = if precision > 0 { precision as usize - 1 } else { 6 };
    significant(&format!("{value:.*e}", after), true)
}

/// Grisu's powers of ten, `10^k` for `k` from -348 to 340 in steps of 8, as a significand and a
/// binary exponent.
const POW10_SIGNIFICANDS: [u64; 87] = [
    0xfa8fd5a0081c0288,
    0xbaaee17fa23ebf76,
    0x8b16fb203055ac76,
    0xcf42894a5dce35ea,
    0x9a6bb0aa55653b2d,
    0xe61acf033d1a45df,
    0xab70fe17c79ac6ca,
    0xff77b1fcbebcdc4f,
    0xbe5691ef416bd60c,
    0x8dd01fad907ffc3c,
    0xd3515c2831559a83,
    0x9d71ac8fada6c9b5,
    0xea9c227723ee8bcb,
    0xaecc49914078536d,
    0x823c12795db6ce57,
    0xc21094364dfb5637,
    0x9096ea6f3848984f,
    0xd77485cb25823ac7,
    0xa086cfcd97bf97f4,
    0xef340a98172aace5,
    0xb23867fb2a35b28e,
    0x84c8d4dfd2c63f3b,
    0xc5dd44271ad3cdba,
    0x936b9fcebb25c996,
    0xdbac6c247d62a584,
    0xa3ab66580d5fdaf6,
    0xf3e2f893dec3f126,
    0xb5b5ada8aaff80b8,
    0x87625f056c7c4a8b,
    0xc9bcff6034c13053,
    0x964e858c91ba2655,
    0xdff9772470297ebd,
    0xa6dfbd9fb8e5b88f,
    0xf8a95fcf88747d94,
    0xb94470938fa89bcf,
    0x8a08f0f8bf0f156b,
    0xcdb02555653131b6,
    0x993fe2c6d07b7fac,
    0xe45c10c42a2b3b06,
    0xaa242499697392d3,
    0xfd87b5f28300ca0e,
    0xbce5086492111aeb,
    0x8cbccc096f5088cc,
    0xd1b71758e219652c,
    0x9c40000000000000,
    0xe8d4a51000000000,
    0xad78ebc5ac620000,
    0x813f3978f8940984,
    0xc097ce7bc90715b3,
    0x8f7e32ce7bea5c70,
    0xd5d238a4abe98068,
    0x9f4f2726179a2245,
    0xed63a231d4c4fb27,
    0xb0de65388cc8ada8,
    0x83c7088e1aab65db,
    0xc45d1df942711d9a,
    0x924d692ca61be758,
    0xda01ee641a708dea,
    0xa26da3999aef774a,
    0xf209787bb47d6b85,
    0xb454e4a179dd1877,
    0x865b86925b9bc5c2,
    0xc83553c5c8965d3d,
    0x952ab45cfa97a0b3,
    0xde469fbd99a05fe3,
    0xa59bc234db398c25,
    0xf6c69a72a3989f5c,
    0xb7dcbf5354e9bece,
    0x88fcf317f22241e2,
    0xcc20ce9bd35c78a5,
    0x98165af37b2153df,
    0xe2a0b5dc971f303a,
    0xa8d9d1535ce3b396,
    0xfb9b7cd9a4a7443c,
    0xbb764c4ca7a44410,
    0x8bab8eefb6409c1a,
    0xd01fef10a657842c,
    0x9b10a4e5e9913129,
    0xe7109bfba19c0c9d,
    0xac2820d9623bf429,
    0x80444b5e7aa7cf85,
    0xbf21e44003acdd2d,
    0x8e679c2f5e44ff8f,
    0xd433179d9c8cb841,
    0x9e19db92b4e31ba9,
    0xeb96bf6ebadf77d9,
    0xaf87023b9bf0ee6b,
];

const POW10_EXPONENTS: [i16; 87] = [
    -1220, -1193, -1166, -1140, -1113, -1087, -1060, -1034, -1007, -980, -954, -927, -901, -874,
    -847, -821, -794, -768, -741, -715, -688, -661, -635, -608, -582, -555, -529, -502, -475, -449,
    -422, -396, -369, -343, -316, -289, -263, -236, -210, -183, -157, -130, -103, -77, -50, -24, 3,
    30, 56, 83, 109, 136, 162, 189, 216, 242, 269, 295, 322, 348, 375, 402, 428, 455, 481, 508,
    534, 561, 588, 614, 641, 667, 694, 720, 747, 774, 800, 827, 853, 880, 907, 933, 960, 986, 1013,
    1039, 1066,
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Round {
    Unknown,
    Up,
    Down,
}

/// fmt's `get_round_direction`, with the wrapping of its unsigned arithmetic.
fn round_direction(divisor: u64, remainder: u64, error: u64) -> Round {
    if remainder <= divisor.wrapping_sub(remainder)
        && error.wrapping_mul(2) <= divisor.wrapping_sub(remainder.wrapping_mul(2))
    {
        return Round::Down;
    }
    if remainder >= error && remainder - error >= divisor.wrapping_sub(remainder - error) {
        return Round::Up;
    }
    Round::Unknown
}

/// Grisu's digits for a precision, which is a count of significant digits unless `fixed` makes it
/// a count after the point, or None when Grisu gives up and snprintf is asked instead.
fn grisu(value: f64, precision: i32, fixed: bool) -> Option<(Vec<u8>, i32)> {
    // The double as f times 2^e, normalized so the top bit of f is set.
    let bits = value.to_bits();
    let mut f = bits & ((1 << 52) - 1);
    let mut biased = (bits >> 52) & 0x7ff;
    if biased != 0 {
        f += 1 << 52;
    } else {
        biased = 1;
    }
    let mut e = biased as i32 - 1075;
    while f & (1 << 52) == 0 {
        f <<= 1;
        e -= 1;
    }
    f <<= 11;
    e -= 11;
    // A cached power of ten that brings the binary exponent into [-60, -32].
    let min_exponent = -60 - (e + 64);
    let index = ((i64::from(min_exponent + 63)).wrapping_mul(0x4d10_4d42) + ((1 << 32) - 1)) >> 32;
    let index = ((index as i32 + 348 - 1) / 8 + 1) as usize;
    let cached_exp10 = -348 + index as i32 * 8;
    let product = u128::from(f) * u128::from(POW10_SIGNIFICANDS[index]);
    let rounded = (product as u64) & (1 << 63) != 0;
    let f = (product >> 64) as u64 + u64::from(rounded);
    let e = e + i32::from(POW10_EXPONENTS[index]) + 64;
    // fmt's `grisu_gen_digits` with its `fixed_handler`.
    let shift = (-e) as u32;
    let one = 1u64 << shift;
    let mut integral = (f >> shift) as u32;
    let mut fractional = f & (one - 1);
    let mut exp = integral.checked_ilog10().map_or(1, |log| log as i32 + 1);
    let mut error: u64 = 1;
    let mut digits: Vec<u8> = Vec::new();
    let mut precision = precision;
    let exp10 = -cached_exp10;
    const POW10: [u64; 11] =
        [1, 10, 100, 1000, 10000, 100000, 1000000, 10000000, 100000000, 1000000000, 10000000000];
    let finish = |mut digits: Vec<u8>, mut exp: i32| {
        if !fixed {
            while digits.last() == Some(&b'0') {
                digits.pop();
                exp += 1;
            }
        }
        Some((digits, exp - cached_exp10))
    };
    if fixed {
        precision += exp + exp10;
        if precision < 0 {
            return finish(digits, exp);
        }
        if precision == 0 {
            let divisor = POW10[exp as usize - 1] << shift;
            match round_direction(divisor, f / 10, error * 10) {
                Round::Unknown => return None,
                direction => digits.push(if direction == Round::Up { b'1' } else { b'0' }),
            }
            return finish(digits, exp);
        }
    }
    // Rounds the last digit written, where a carry out of the first makes it a one and a zero.
    let round_up = |digits: &mut Vec<u8>| {
        let last = digits.len() - 1;
        digits[last] += 1;
        let mut i = last;
        while i > 0 && digits[i] > b'9' {
            digits[i] = b'0';
            digits[i - 1] += 1;
            i -= 1;
        }
        if digits[0] > b'9' {
            digits[0] = b'1';
            digits.push(b'0');
        }
    };
    loop {
        let divisor = POW10[exp as usize - 1] as u32;
        let digit = integral / divisor;
        integral %= divisor;
        exp -= 1;
        let remainder = (u64::from(integral) << shift) + fractional;
        digits.push(b'0' + digit as u8);
        if (digits.len() as i32) >= precision {
            match round_direction(POW10[exp as usize] << shift, remainder, error) {
                Round::Down => return finish(digits, exp),
                Round::Unknown => return None,
                Round::Up => {
                    round_up(&mut digits);
                    return finish(digits, exp);
                }
            }
        }
        if exp == 0 {
            break;
        }
    }
    loop {
        fractional = fractional.wrapping_mul(10);
        error = error.wrapping_mul(10);
        let digit = (fractional >> shift) as u8;
        fractional &= one - 1;
        exp -= 1;
        digits.push(b'0' + digit);
        if (digits.len() as i32) < precision {
            continue;
        }
        if error >= one || error >= one - error {
            return None;
        }
        match round_direction(one, fractional, error) {
            Round::Down => return finish(digits, exp),
            Round::Unknown => return None,
            Round::Up => {
                round_up(&mut digits);
                return finish(digits, exp);
            }
        }
    }
}

/// The digits of Rust's `{:e}` and the power of ten after them, without trailing zeros when
/// `strip` is set.
fn significant(written: &str, strip: bool) -> (Vec<u8>, i32) {
    let (mantissa, exponent) = written.split_once('e').unwrap_or((written, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let mut digits: Vec<u8> = mantissa.bytes().filter(u8::is_ascii_digit).collect();
    let mut exp = exponent - (digits.len() as i32 - 1);
    if strip {
        while digits.len() > 1 && digits.last() == Some(&b'0') {
            digits.pop();
            exp += 1;
        }
    }
    (digits, exp)
}

fn write_exponent(out: &mut Vec<u8>, exp: i32) {
    let (sign, exp) = if exp < 0 { (b'-', -exp) } else { (b'+', exp) };
    out.push(sign);
    out.extend_from_slice(format!("{exp:02}").as_bytes());
}

/// Writes `digits` times ten to `exp` with a separator between each group of three integer
/// digits, padding the integer part with zeros to `full_exp` digits.
fn grouped(out: &mut Vec<u8>, digits: &[u8], full_exp: usize, separator: u8) {
    let mut count = if full_exp.is_multiple_of(3) { 3 } else { full_exp % 3 };
    let mut i = 0;
    while i < full_exp {
        if i > 0 {
            out.push(separator);
        }
        for at in i..i + count {
            out.push(digits.get(at).copied().unwrap_or(b'0'));
        }
        i += count;
        count = 3;
    }
}

/// fmt's `float_writer`.
fn prettify(
    out: &mut Vec<u8>,
    digits: &[u8],
    exp: i32,
    float: &FloatSpecs,
    precision: i32,
    point: u8,
) {
    let count = digits.len() as i32;
    let full_exp = count + exp;
    let mut format = float.format;
    let limit = if precision > 0 { precision } else { 16 };
    if format == FloatFormat::General && !(full_exp > -4 && full_exp - 1 < limit) {
        format = FloatFormat::Exp;
    }
    let zeros = |out: &mut Vec<u8>, count: i32| {
        out.extend(std::iter::repeat_n(b'0', count.max(0) as usize));
    };
    if format == FloatFormat::Exp {
        out.push(digits[0]);
        let num_zeros = precision - count;
        let trailing = num_zeros > 0 && float.trailing_zeros;
        if count > 1 || trailing {
            out.push(point);
        }
        out.extend_from_slice(&digits[1..]);
        if trailing {
            zeros(out, num_zeros);
        }
        out.push(if float.upper { b'E' } else { b'e' });
        write_exponent(out, full_exp - 1);
        return;
    }
    if count <= full_exp {
        if float.separator != 0 && full_exp > 3 {
            grouped(out, digits, full_exp as usize, float.separator);
        } else {
            out.extend_from_slice(digits);
            zeros(out, full_exp - count);
        }
        if float.trailing_zeros {
            out.push(point);
            let num_zeros = precision - full_exp;
            if num_zeros <= 0 {
                if format != FloatFormat::Fixed {
                    out.push(b'0');
                }
                return;
            }
            zeros(out, num_zeros);
        }
    } else if full_exp > 0 {
        let whole = full_exp as usize;
        if float.separator != 0 && full_exp > 3 {
            grouped(out, digits, whole, float.separator);
        } else {
            out.extend_from_slice(&digits[..whole]);
        }
        if !float.trailing_zeros {
            let mut end = digits.len();
            while end > whole && digits[end - 1] == b'0' {
                end -= 1;
            }
            if end != whole {
                out.push(point);
            }
            out.extend_from_slice(&digits[whole..end]);
            return;
        }
        out.push(point);
        out.extend_from_slice(&digits[whole..]);
        if precision > count {
            zeros(out, precision - count);
        }
    } else {
        out.push(b'0');
        let mut num_zeros = -full_exp;
        if count == 0 && precision >= 0 && precision < num_zeros {
            num_zeros = precision;
        }
        let mut end = digits.len();
        if !float.trailing_zeros {
            while end > 0 && digits[end - 1] == b'0' {
                end -= 1;
            }
        }
        if num_zeros != 0 || end != 0 {
            out.push(point);
            zeros(out, num_zeros);
            out.extend_from_slice(&digits[..end]);
        }
    }
}

/// glibc's `%a`, which fmt hands a hexadecimal float to. A subnormal keeps its leading zero, and
/// rounding to a precision rounds half to even and does not renormalize, so
/// `printf('%.1a', 1.96875)` is `0x2.0p+0`.
fn hex_float(out: &mut Vec<u8>, value: f64, precision: i32, alt: bool, upper: bool) {
    let bits = value.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let mut mantissa = bits & ((1 << 52) - 1);
    let (mut lead, exp) = match (value == 0.0, biased) {
        (true, _) => (0u64, 0),
        (false, 0) => (0, -1022),
        (false, _) => (1, biased - 1023),
    };
    let mut fraction = format!("{mantissa:013x}").into_bytes();
    if precision < 0 {
        while fraction.last() == Some(&b'0') {
            fraction.pop();
        }
    } else if precision < 13 {
        let dropped_bits = 4 * (13 - precision as u32);
        let dropped = mantissa & ((1 << dropped_bits) - 1);
        let half = 1u64 << (dropped_bits - 1);
        let mut kept = mantissa >> dropped_bits;
        let odd = if precision == 0 { lead & 1 == 1 } else { kept & 1 == 1 };
        if dropped > half || (dropped == half && odd) {
            kept += 1;
            if precision == 0 || kept >> (4 * precision as u32) != 0 {
                lead += 1;
                kept &= (1 << (4 * precision as u32)) - 1;
            }
        }
        mantissa = kept;
        fraction = if precision == 0 {
            Vec::new()
        } else {
            format!("{mantissa:0width$x}", width = precision as usize).into_bytes()
        };
    } else {
        fraction.resize(precision as usize, b'0');
    }
    out.extend_from_slice(if upper { b"0X" } else { b"0x" });
    out.extend_from_slice(lead.to_string().as_bytes());
    if !fraction.is_empty() || alt {
        out.push(b'.');
    }
    if upper {
        fraction.make_ascii_uppercase();
    }
    out.extend_from_slice(&fraction);
    out.push(if upper { b'P' } else { b'p' });
    out.push(if exp < 0 { b'-' } else { b'+' });
    out.extend_from_slice(exp.unsigned_abs().to_string().as_bytes());
}

#[cfg(test)]
#[allow(clippy::approx_constant)]
mod tests {
    use super::*;

    fn formatted(pattern: &str, arguments: &[Value]) -> String {
        match format(pattern, arguments) {
            Ok(Value::Varchar(text)) => text,
            Ok(other) => panic!("{pattern}: {other:?}"),
            Err(error) => panic!("{pattern}: {error}"),
        }
    }

    fn printed(pattern: &str, arguments: &[Value]) -> String {
        match printf(pattern, arguments) {
            Ok(Value::Varchar(text)) => text,
            Ok(other) => panic!("{pattern}: {other:?}"),
            Err(error) => panic!("{pattern}: {error}"),
        }
    }

    fn d(value: f64) -> Value {
        Value::Double(value)
    }

    fn i(value: i64) -> Value {
        Value::BigInt(value)
    }

    fn s(text: &str) -> Value {
        Value::Varchar(text.to_string())
    }

    #[test]
    fn a_double_is_written_the_way_the_pin_writes_it() {
        let cases = [
            ("{}", 1.0, "1.0"),
            ("{}", 1e16, "1e+16"),
            ("{}", 1e15, "1000000000000000.0"),
            ("{}", 0.0001, "0.0001"),
            ("{}", 0.00001, "1e-05"),
            ("{}", 1.5e300, "1.5e+300"),
            ("{}", -0.0, "-0.0"),
            ("{}", 0.10000000149011612, "0.10000000149011612"),
            ("{:.0}", 12345.0, "10000"),
            ("{:.0}", 1.5, "1.5"),
            ("{:.0}", 25.0, "25"),
            ("{:.0}", 1.25, "1"),
            ("{:.0}", 0.25, "0.25"),
            ("{:.2}", 1.23456, "1.2"),
            ("{:.3}", 1234.5, "1.23e+03"),
            ("{:.10}", 0.1, "0.1"),
            ("{:.17}", 0.1, "0.10000000000000001"),
            ("{:.20}", 0.1, "0.10000000000000000555"),
            ("{:.30g}", 1e-5, "1.00000000000000008180305391403e-05"),
            ("{:e}", 1.0, "1.000000e+00"),
            ("{:.1e}", 9.96, "1.0e+01"),
            ("{:e}", 0.0, "0.000000e+00"),
            ("{:.0e}", 0.0, "0e+00"),
            ("{:.0e}", 12345.0, "1e+04"),
            ("{:#.0e}", 12345.0, "1e+04"),
            ("{:g}", 1e-5, "1e-05"),
            ("{:G}", 1e20, "1E+20"),
            ("{:#g}", 1.0, "1.00000"),
            ("{:#g}", 1e20, "1.00000e+20"),
            ("{:g}", 0.0, "0"),
            ("{:.3g}", 0.0, "0"),
            ("{:.0f}", 2.5, "2"),
            ("{:#.0f}", 2.5, "2.."),
            ("{:#.0f}", 1.0, "1."),
            ("{:f}", 1e20, "100000000000000000000.000000"),
            ("{:f}", 0.0, "0.000000"),
            ("{:.3f}", 0.0, "0.000"),
            ("{:#}", 1.0, "1.0"),
            ("{:#.3}", 1.0, "1.00"),
            ("{:.25e}", 1.0, "1.0000000000000000000000000e+00"),
            ("{:,}", 1234567.891, "1,234,567.891"),
            ("{:t.}", 1234567.5, "1.234.567,5"),
            ("{:_.3f}", 1234.5, "1_234.500"),
            ("{:_}", 1.5, "1.5"),
            ("{:L}", 1234567.5, "1.23457e+06"),
            ("{:.3n}", 1.23456, "1.23"),
            ("{:n}", 1e20, "1e+20"),
            ("{:+}", 5.5, "+5.5"),
            ("{:08.3f}", -3.14159, "-003.142"),
            ("{:010}|", -1.5, "-0000001.5|"),
            ("{:+010.2e}|", 12345.0, "+01.23e+04|"),
            ("{:x<05}|", -1.5, "-01.5|"),
            ("{:=8}|", -1.5, "-    1.5|"),
            ("{:+08.2f}", 1.5, "+0001.50"),
            ("{:10}|", 1.5, "       1.5|"),
            ("{:10.3}|", 3.14159, "      3.14|"),
            ("{:<10.3e}|", 3.14159, "3.142e+00 |"),
            ("{:^10}|", -1.5, "   -1.5   |"),
            ("{:*^+10.1f}|", 1.25, "***+1.2***|"),
            ("{:a}", 1.5, "0x1.8p+0"),
            ("{:.2a}", 1.0, "0x1.00p+0"),
            ("{:#a}", 1.0, "0x1.p+0"),
            ("{:10a}|", 1.0, "    0x1p+0|"),
            ("{:010a}", 1.0, "00000x1p+0"),
            ("{}", f64::INFINITY, "inf"),
            ("{:08}", f64::INFINITY, "inf00000"),
            ("{:+}", f64::NAN, "+nan"),
            ("{:E}", f64::INFINITY, "INF"),
            ("{:>6}|", f64::NEG_INFINITY, "  -inf|"),
            ("{:6}|", f64::INFINITY, "inf   |"),
            ("{:.0}", 9.5, "9.5"),
            ("{:.1}", 0.95, "0.9"),
            ("{:.3f}", 123456789.0, "123456789.000"),
            ("{:.8f}", 1e10, "10000000000.00000000"),
            ("{:.12f}", 1e10 / 3.0, "3333333333.333333492279"),
            ("{:f}", 1e22, "10000000000000000000000.000000"),
            ("{:f}", 2e25, "20000000000000001811939328.000000"),
            ("{:.2e}", 1e300, "1.00e+300"),
            ("{:.17}", 1e300, "1.0000000000000001e+300"),
        ];
        for (pattern, value, expected) in cases {
            assert_eq!(formatted(pattern, &[d(value)]), expected, "{pattern} {value}");
        }
    }

    #[test]
    fn a_double_is_printed_the_way_the_pin_prints_it() {
        let cases = [
            ("%f", 1.5, "1.500000"),
            ("%.2f", 1.005, "1.00"),
            ("%e", 12345.678, "1.234568e+04"),
            ("%g", 0.0001234, "0.0001234"),
            ("%a", 5.0, "0x1.4p+2"),
            ("%.0a", -5.0, "-0x1p+2"),
            ("%.0a", 1.5, "0x2p+0"),
            ("%.1a", 1.03125, "0x1.0p+0"),
            ("%.1a", 1.09375, "0x1.2p+0"),
            ("%.1a", 1.96875, "0x2.0p+0"),
            ("%.0a", 2.0, "0x1p+1"),
            ("%.2a", 1e-310, "0x0.01p-1022"),
            ("%.0a", 1e-310, "0x0p-1022"),
            ("%a", 0.1, "0x1.999999999999ap-4"),
            ("%A", 1.0, "0X1P+0"),
            ("%.3a", 1.0, "0x1.000p+0"),
            ("%a", 0.0, "0x0p+0"),
            ("%a", 1e-310, "0x0.012688b70e62bp-1022"),
            ("%.0e", 2.5, "2e+00"),
            ("%.0g", 1.5, "1.5"),
            ("%.g", 15.0, "15"),
            ("%10.4g|", 3.14159, "     3.142|"),
            ("%G", 1e-10, "1E-10"),
            ("%E", 1.5, "1.500000E+00"),
            ("%#.3g", 1.0, "1.00"),
            ("%+.1e", 0.0, "+0.0e+00"),
            ("%g", 100000.0, "100000"),
            ("%g", 1000000.0, "1e+06"),
            ("%.0f", 0.5, "0"),
            ("%.0f", 1.5, "2"),
            ("%.1f", 0.25, "0.2"),
            ("%.1f", 0.35, "0.3"),
            ("%010.3f|", -3.5, "-00003.500|"),
            ("%-10.3f|", 3.5, "3.500     |"),
            ("% f", 1.0, " 1.000000"),
            ("%f", -0.0, "-0.000000"),
            ("%08.3e", -1.5, "-1.500e+00"),
            ("%,.2f", 1234.5, "1,234.50"),
            ("%.f", 1234.5, "1234"),
            ("%lf", 5.5, "5.500000"),
            ("%Lf", 5.5, "5.500000"),
            ("%hf", 5.5, "5.500000"),
            ("%.3f", 1e20, "100000000000000000000.000"),
            ("%.2f", 123456789012345678.0, "123456789012345680.00"),
            ("%.5f", 1e17, "100000000000000000.00000"),
            ("%.1f", 3.14159e25, "31415900000000000537919488.0"),
            ("%.17f", 0.1, "0.10000000000000001"),
            ("%.17g", 0.1, "0.10000000000000001"),
            ("%.16e", 1.0 / 3.0, "3.3333333333333331e-01"),
            ("%.10f", 1.0 / 3.0, "0.3333333333"),
            ("%.15f", 12345.678, "12345.677999999999884"),
            ("%.2f", 1e-300, "0.00"),
            ("%.17e", 5e-324, "4.94065645841246544e-324"),
            ("%.3g", 5e-324, "4.94e-324"),
            ("%.0f", 9.5, "10"),
            ("%.2f", 0.125, "0.12"),
            ("%.2f", 0.375, "0.38"),
            ("%.5f", 0.000001, "0.00000"),
            ("%.0e", 0.5, "5e-01"),
            ("%.3f", 999.9995, "1000.000"),
            ("%.2f", 9.995, "9.99"),
        ];
        for (pattern, value, expected) in cases {
            assert_eq!(printed(pattern, &[d(value)]), expected, "{pattern} {value}");
        }
        // Grisu's error term wraps around before it can refuse these digits.
        let digits = "10000000000000000525324139744043350219726562500";
        let long = format!("{digits}{}.000", "0".repeat(301 - digits.len()));
        assert_eq!(printed("%.3f", &[d(1e300)]), long);
        let whole = format!("{digits}{}", "0".repeat(301 - digits.len()));
        assert_eq!(printed("%.0f", &[d(1e300)]), whole);
        let digits = "17976931348623157080146484076976776123046875";
        let largest = format!("{digits}{}.0", "0".repeat(309 - digits.len()));
        assert_eq!(printed("%.1f", &[d(f64::MAX)]), largest);
    }

    #[test]
    fn an_integer_is_written_and_printed_the_way_the_pin_does_it() {
        let written = [
            ("{:+}", i(5), "+5"),
            ("{: }", i(5), " 5"),
            ("{:#x}", i(255), "0xff"),
            ("{:#b}", i(5), "0b101"),
            ("{:#o}", i(8), "010"),
            ("{:=+8}", i(5), "+      5"),
            ("{:#x}", i(0), "0x0"),
            ("{:#o}", i(0), "0"),
            ("{:#b}", i(0), "0b0"),
            ("{:#010x}", i(255), "0x000000ff"),
            ("{:#X}", i(255), "0XFF"),
            ("{:#B}", i(5), "0B101"),
            ("{:+#x}", i(255), "+0xff"),
            ("{:x}", i(-255), "-ff"),
            ("{:,x}", i(1234567), "1,234,567"),
            ("{:_}", i(1234567), "1_234_567"),
            ("{:,}", i(-1234), "-1,234"),
            ("{:n}", i(1234567), "1234567"),
            ("{:,n}", i(1234567), "1,234,567"),
            ("{:l}", i(12), "12"),
            ("{:=5}|", i(12), "   12|"),
            ("{:05}|", i(-12), "-0012|"),
            ("{:<05}|", i(12), "00012|"),
            ("{:*=+6}|", i(12), "+***12|"),
            ("{:<10}|", i(12), "12        |"),
            ("{:^5}|", i(1), "  1  |"),
            ("{:^6}|", i(1), "  1   |"),
            ("{:d}", Value::Boolean(true), "1"),
            ("{:x}", Value::Boolean(true), "1"),
            ("{:5d}|", Value::Boolean(false), "    0|"),
            ("{:5}|", Value::Boolean(true), "true |"),
            ("{:^7}|", Value::Boolean(true), " true  |"),
            ("{:=5}", Value::Boolean(true), "true "),
            ("{:#}", Value::Boolean(true), "true"),
            ("{}", Value::HugeInt(120381902481294715712), "120381902481294715712"),
            ("{:x}", Value::HugeInt(-255), "-ff"),
            ("{:,}", Value::HugeInt(-12345678901234567890123), "-12,345,678,901,234,567,890,123"),
            ("{:x}", Value::UHugeInt(u128::MAX), "ffffffffffffffffffffffffffffffff"),
        ];
        for (pattern, value, expected) in written {
            assert_eq!(formatted(pattern, std::slice::from_ref(&value)), expected, "{pattern} {value:?}");
        }
        let printed_cases = [
            ("%x", i(-5), "fffffffffffffffb"),
            ("%X", i(255), "FF"),
            ("%o", i(8), "10"),
            ("%#x", i(255), "0xff"),
            ("%#o", i(8), "010"),
            ("%b", i(5), "101"),
            ("%x", Value::HugeInt(-5), "fffffffffffffffffffffffffffffffb"),
            ("%lld", Value::HugeInt(100000000000000000000), "7766279631452241920"),
            ("%hd", i(70000), "4464"),
            ("%hhd", i(300), "44"),
            ("%hhu", i(-1), "255"),
            ("%c", i(65), "A"),
            ("%5c|", i(66), "    B|"),
            ("%-5c|", i(65), "    A|"),
            ("%05c|", i(65), "0000A|"),
            ("%hhc", i(65), "A"),
            ("%-05d|", i(3), "00003|"),
            ("%05d", i(-3), "-0003"),
            ("%+d", i(3), "+3"),
            ("% d", i(3), " 3"),
            ("%.3d", i(5), "005"),
            ("%8.3d|", i(5), "     005|"),
            ("%-8d|", i(5), "5       |"),
            ("%d", Value::UBigInt(1), "1"),
            ("%c", Value::HugeInt(66), "B"),
            ("%u", i(-1), "18446744073709551615"),
            ("%i", i(5), "5"),
            ("%d", Value::UHugeInt(u128::MAX), "340282366920938463463374607431768211455"),
            ("%x", Value::HugeInt(-1), "ffffffffffffffffffffffffffffffff"),
            ("%hd", Value::HugeInt(100000), "-31072"),
            ("%ld", Value::HugeInt(100000000000000000000), "7766279631452241920"),
            ("%lx", Value::HugeInt(100000000000000000000), "56bc75e2d63100000"),
            ("%hhx", Value::UBigInt(511), "ff"),
            ("%hx", i(-1), "ffff"),
            ("%lx", i(-1), "ffffffffffffffff"),
            ("%zx", i(-1), "ffffffffffffffff"),
            ("%jd", i(5), "5"),
            ("%td", i(5), "5"),
            ("%Ld", i(5), "5"),
            ("%.d", i(-123456), "-123.456"),
            ("%,d", i(-1234567), "-1,234,567"),
            ("%_x", i(1234567), "1_234_567"),
            ("%#5x|", i(255), " 0xff|"),
            ("%#05x", i(255), "0x0ff"),
            ("%-#8o|", i(8), "010     |"),
            ("%.0d|", i(0), "0|"),
            ("%#.0x|", i(0), "0|"),
            ("%#x", i(0), "0"),
            ("%#o", i(0), "0"),
            ("%.3x", i(5), "005"),
            ("%#.3o", i(8), "010"),
            ("%+x", i(255), "+ff"),
            ("%+u", i(5), "+5"),
            ("% x", i(5), " 5"),
            ("%+c", i(65), "A"),
            ("%#c", i(65), "A"),
            ("%n", i(1), "1"),
            ("%s", Value::Boolean(true), "true"),
            ("%d", Value::Boolean(true), "1"),
            ("%x", Value::Boolean(true), "1"),
            ("%5s|", Value::Boolean(false), "false|"),
            ("%5d|", Value::Boolean(false), "    0|"),
            ("%c", Value::Boolean(true), "\u{1}"),
        ];
        for (pattern, value, expected) in printed_cases {
            assert_eq!(printed(pattern, std::slice::from_ref(&value)), expected, "{pattern} {value:?}");
        }
    }

    #[test]
    fn strings_and_indexes_are_handled_the_way_the_pin_handles_them() {
        let written = [
            ("{:5}|", vec![s("é")], "é    |"),
            ("{:.1}|", vec![s("éa")], "é|"),
            ("{:>5}|", vec![s("ab")], "   ab|"),
            ("{:^6}|", vec![s("ab")], "  ab  |"),
            ("{:*<4}|", vec![s("ab")], "ab**|"),
            ("{:=^10}|", vec![s("ab")], "====ab====|"),
            ("{:3}|", vec![s("éé")], "éé |"),
            ("{:.2}|", vec![s("éé")], "éé|"),
            ("{0} {1} {0}", vec![s("a"), s("b")], "a b a"),
            ("{{}}", vec![], "{}"),
            ("{:{}}|", vec![s("a"), i(5)], "a    |"),
            ("{:{}}|", vec![s("a"), Value::HugeInt(5)], "a    |"),
            ("{:.{}f}", vec![d(3.14159), i(2)], "3.14"),
            ("{0:{1}}|", vec![s("a"), i(4)], "a   |"),
            ("{:{}.{}}|", vec![d(1.23456), i(8), i(2)], "     1.2|"),
            ("{1}{0}", vec![s("a"), s("b")], "ba"),
            ("{0:}", vec![i(1)], "1"),
            ("{:}", vec![i(1)], "1"),
            ("{: >}", vec![i(1)], "1"),
            ("{:>>}|", vec![i(1)], "1|"),
            ("{}|", vec![s("x"), s("y")], "x|"),
            ("{:,}", vec![s("ab")], "ab"),
        ];
        for (pattern, arguments, expected) in written {
            assert_eq!(formatted(pattern, &arguments), expected, "{pattern}");
        }
        let printed_cases = [
            ("%5s|%-5s|", vec![s("ab"), s("cd")], "   ab|cd   |"),
            ("%.1s", vec![s("abc")], "a"),
            ("%%", vec![], "%"),
            ("%2$s %1$s", vec![s("a"), s("b")], "b a"),
            ("%*d|", vec![i(5), i(3)], "    3|"),
            ("%-*d|", vec![i(5), i(3)], "3    |"),
            ("%*d|", vec![i(-5), i(3)], "3    |"),
            ("%.*f", vec![i(2), d(3.14159)], "3.14"),
            ("%*.*f|", vec![i(8), i(2), d(3.14159)], "    3.14|"),
            ("%.*s", vec![i(-1), s("abc")], ""),
            ("%*s|", vec![Value::Boolean(true), s("a")], "a|"),
            ("%*s|", vec![Value::HugeInt(5), s("a")], "    a|"),
            ("%5.2s|", vec![s("abc")], "   ab|"),
            ("%.3s|", vec![s("éa")], "éa|"),
            ("%3s|", vec![s("é")], "  é|"),
            ("%05s|", vec![s("a")], "    a|"),
            ("%5.1s|%-3s|%.0s|", vec![s("xyz"), s("é"), s("abc")], "    x|é  ||"),
            ("%,s", vec![s("ab")], "ab"),
            ("%+s", vec![s("a")], "a"),
            ("%ls", vec![s("a")], "a"),
            ("%s|", vec![s("x"), s("y")], "x|"),
            ("%1$d %1$d", vec![i(7)], "7 7"),
        ];
        for (pattern, arguments, expected) in printed_cases {
            assert_eq!(printed(pattern, &arguments), expected, "{pattern}");
        }
        let cut = format!("a{}%s", '\0');
        assert_eq!(printed(&cut, &[s("x")]), "a");
    }

    #[test]
    fn what_the_pin_refuses_is_refused_in_its_words() {
        let format_cases = [
            ("{:+}", vec![Value::UBigInt(5)], "format specifier requires signed argument"),
            ("{:+}", vec![Value::HugeInt(5)], "format specifier requires signed argument"),
            ("{:+}", vec![Value::Boolean(true)], "format specifier requires signed argument"),
            ("{:+}", vec![s("a")], "format specifier requires numeric argument"),
            ("{:#}", vec![s("a")], "format specifier requires numeric argument"),
            ("{:=5}", vec![s("a")], "format specifier requires numeric argument"),
            ("{:05}", vec![s("a")], "format specifier requires numeric argument"),
            ("{:.2}", vec![i(5)], "precision not allowed for this argument type"),
            (
                "{:x}",
                vec![d(5.5)],
                "Invalid type specifier \"x\" for formatting a value of type float",
            ),
            (
                "{:d}",
                vec![s("a")],
                "Invalid type specifier \"d\" for formatting a value of type string",
            ),
            (
                "{:c}",
                vec![s("a")],
                "Invalid type specifier \"c\" for formatting a value of type string",
            ),
            (
                "{:c}",
                vec![i(65)],
                "Invalid type specifier \"c\" for formatting a value of type int",
            ),
            ("{:s}", vec![i(1)], "Invalid type specifier \"s\" for formatting a value of type int"),
            (
                "{:s}",
                vec![d(1.5)],
                "Invalid type specifier \"s\" for formatting a value of type float",
            ),
            ("{:k}", vec![i(1)], "Invalid type specifier \"k\" for formatting a value of type int"),
            (
                "{:010,}",
                vec![i(1)],
                "Invalid type specifier \",\" for formatting a value of type int",
            ),
            (
                "{:05>}",
                vec![i(1)],
                "Invalid type specifier \">\" for formatting a value of type int",
            ),
            ("{", vec![], "invalid format string"),
            ("}", vec![], "unmatched '}' in format string"),
            ("{:}}", vec![s("a")], "unmatched '}' in format string"),
            (
                "{0} {}",
                vec![i(1), i(2)],
                "cannot switch from manual to automatic argument indexing",
            ),
            (
                "{} {0}",
                vec![i(1), i(2)],
                "cannot switch from automatic to manual argument indexing",
            ),
            ("{:{0}}", vec![i(1)], "cannot switch from automatic to manual argument indexing"),
            ("{:{}}", vec![s("a"), i(-5)], "negative width"),
            ("{:{}}", vec![s("a"), s("b")], "width is not integer"),
            ("{:{}}", vec![s("a"), Value::Boolean(true)], "width is not integer"),
            ("{:.{}}", vec![d(1.5), d(1.5)], "precision is not integer"),
            ("{:.{}}", vec![s("abc"), i(-1)], "negative precision"),
            ("{01}", vec![i(1), i(2)], "invalid format string"),
            ("{:é<5}", vec![s("a")], "missing '}' in format string"),
            ("{:5", vec![i(1)], "missing '}' in format string"),
            ("{:ss}", vec![s("a")], "missing '}' in format string"),
            ("{:t}", vec![i(1)], "missing '}' in format string"),
            ("{:{}", vec![i(1), i(2)], "missing '}' in format string"),
            ("{:{<5}", vec![i(1)], "invalid fill character '{'"),
            ("{2}", vec![i(1)], "Argument index \"2\" out of range"),
            ("{}{}", vec![i(1)], "Argument index \"1\" out of range"),
            ("{:2147483648}", vec![i(1)], "number is too big"),
            ("{:99999999999}", vec![i(1)], "number is too big"),
            (
                "{x",
                vec![i(1)],
                "Argument with name \"x\" not found, did you mean to use it as a format specifier (e.g. {:x}",
            ),
            (
                "{:{x}}",
                vec![i(1)],
                "Argument with name \"x\" not found, did you mean to use it as a format specifier (e.g. {:x}",
            ),
        ];
        for (pattern, arguments, expected) in format_cases {
            let said = format(pattern, &arguments).unwrap_err().to_string();
            assert!(said.contains(expected), "{pattern}: {said}");
        }
        let printf_cases = [
            ("%s", vec![i(33)], "Invalid type specifier \"s\" for formatting a value of type int"),
            (
                "%d",
                vec![s("x")],
                "Invalid type specifier \"d\" for formatting a value of type string",
            ),
            (
                "%s",
                vec![d(1.5)],
                "Invalid type specifier \"s\" for formatting a value of type float",
            ),
            ("%f", vec![i(5)], "Invalid type specifier \"f\" for formatting a value of type int"),
            (
                "%d",
                vec![d(5.5)],
                "Invalid type specifier \"d\" for formatting a value of type float",
            ),
            (
                "%hd",
                vec![d(5.5)],
                "Invalid type specifier \"d\" for formatting a value of type float",
            ),
            ("%q", vec![i(5)], "Invalid type specifier \"q\" for formatting a value of type int"),
            ("%p", vec![i(1)], "Invalid type specifier \"p\" for formatting a value of type int"),
            (
                "%f",
                vec![Value::UHugeInt(1)],
                "Invalid type specifier \"f\" for formatting a value of type int",
            ),
            (
                "%f",
                vec![Value::HugeInt(5)],
                "Invalid type specifier \"f\" for formatting a value of type int",
            ),
            ("%5", vec![i(1)], "Invalid type specifier \"5\" for formatting a value of type int"),
            ("%", vec![i(1)], "Invalid type specifier \"%\" for formatting a value of type int"),
            (
                "%-",
                vec![s("a")],
                "Invalid type specifier \"-\" for formatting a value of type string",
            ),
            ("%l", vec![i(1)], "invalid format string"),
            ("%hh", vec![i(1)], "invalid format string"),
            ("%s", vec![], "Argument index \"0\" out of range"),
            ("%", vec![], "Argument index \"0\" out of range"),
            ("%d %d", vec![i(1)], "Argument index \"1\" out of range"),
            ("%5$s", vec![s("a")], "Argument index \"4\" out of range"),
            ("%0$s", vec![s("a")], "argument index out of range"),
            (
                "%1$s %s",
                vec![s("a"), s("b")],
                "cannot switch from manual to automatic argument indexing",
            ),
            (
                "%2$*1$d|",
                vec![i(5), i(3)],
                "cannot switch from automatic to manual argument indexing",
            ),
            ("%*s|", vec![s("x"), s("abc")], "width is not integer"),
            ("%.*s", vec![s("x"), s("abc")], "precision is not integer"),
            ("%*s", vec![i(3000000000), s("a")], "number is too big"),
            ("%.*f", vec![i(3000000000), d(1.5)], "number is too big"),
            (
                "%c",
                vec![i(200)],
                "Invalid UTF8 produced by format string \"%c\" - note that %c writes a single byte, use chr(...) to write a Unicode code point",
            ),
        ];
        for (pattern, arguments, expected) in printf_cases {
            let said = printf(pattern, &arguments).unwrap_err().to_string();
            assert!(said.contains(expected), "{pattern}: {said}");
        }
    }
}
